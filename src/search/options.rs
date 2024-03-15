use itertools::Itertools;
use std::collections::HashMap;
use tantivy::collector::{Collector, TopDocs};
use tantivy::query::{
    BooleanQuery, BoostQuery, ConstScoreQuery, FuzzyTermQuery, Occur, PhraseQuery, Query, TermQuery,
};
use tantivy::schema::{Facet, FacetOptions, Schema, TextFieldIndexing, TextOptions, TEXT};
use tantivy::tokenizer::{TextAnalyzer, WhitespaceTokenizer};
use tantivy::{DocId, Document, Score, SegmentReader, Term};

use super::{open_or_create_index, FCFruit, GenericSearcher, Searcher, SearcherInner};
use crate::NaiveNixosOption;

impl Searcher for GenericSearcher<NaiveNixosOption> {
    type Item = NaiveNixosOption;

    fn parse_query(&self, query_string: &str) -> Box<dyn Query> {
        let Some(ref inner) = self.inner else {
            unreachable!("searcher not initialized, cannot parse");
        };
        let mut subqueries: Vec<(Occur, Box<dyn Query>)> = vec![];

        let name_field = inner.schema.get_field("name").unwrap();
        for (i, word) in query_string.split(' ').enumerate() {
            let qlen = word.len();
            let name_term = Term::from_field_text(name_field, word);

            // words further back in the query get assigned less importance
            let length_loss = 1. - i as f32 / 10.;

            // search for exact fit on the name field, highest priority
            if word.contains('.') {
                let subterms = word
                    .split('.')
                    .map(|p| Term::from_field_text(name_field, p))
                    .collect_vec();

                subqueries.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(
                        Box::new(PhraseQuery::new(subterms.clone())),
                        1.5 * length_loss,
                    )),
                ));

                let mut fz_sqs: Vec<(Occur, Box<dyn Query>)> = vec![];
                subterms.into_iter().for_each(|t| {
                    fz_sqs.push((
                        Occur::Should,
                        Box::new(FuzzyTermQuery::new_prefix(t, 0, false)),
                    ))
                });

                subqueries.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(
                        Box::new(BooleanQuery::new(fz_sqs)),
                        3. * length_loss,
                    )),
                ))
            } else {
                subqueries.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(
                        Box::new(TermQuery::new(
                            name_term.clone(),
                            tantivy::schema::IndexRecordOption::WithFreqsAndPositions,
                        )),
                        1.5 * length_loss,
                    )),
                ));
            }

            // fuzzily search on the name field
            let fq =
                FuzzyTermQuery::new_prefix(name_term.clone(), qlen.clamp(2, 4) as u8 - 2, true);
            subqueries.push((Occur::Should, Box::new(BoostQuery::new(Box::new(fq), 2.2))));
        }

        //description queries
        let mut description_subqueries: Vec<(Occur, Box<dyn Query>)> = vec![];
        let description_field = inner.schema.get_field("description").unwrap();
        for (i, word) in query_string.split(' ').enumerate() {
            let length_loss = 0.5 - i as f32 / 10.;
            let qlen = word.len();
            let description_term = Term::from_field_text(description_field, word);

            // search for exact fit on the description field
            description_subqueries.push((
                Occur::Should,
                Box::new(ConstScoreQuery::new(
                    Box::new(TermQuery::new(
                        description_term.clone(),
                        tantivy::schema::IndexRecordOption::WithFreqsAndPositions,
                    )),
                    length_loss,
                )),
            ));

            if qlen >= 3 {
                let fq = FuzzyTermQuery::new_prefix(description_term.clone(), 1, false);
                description_subqueries.push((
                    Occur::Should,
                    Box::new(ConstScoreQuery::new(Box::new(fq), 0.5 * length_loss)),
                ));
            }
        }

        let description_query =
            BoostQuery::new(Box::new(BooleanQuery::new(description_subqueries)), 0.2);
        subqueries.push((Occur::Should, Box::new(description_query)));

        Box::new(BooleanQuery::new(subqueries))
    }

    /// creates the index and initializes the struct that holds
    /// fields that are important for searching
    fn create_index(&mut self) -> anyhow::Result<()> {
        let mut schema_builder = Schema::builder();

        let name_field_options = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_index_option(tantivy::schema::IndexRecordOption::WithFreqsAndPositions)
                .set_tokenizer("option_name"),
        );

        // name of the option, stored to access it's data from the searcher's hashmap
        let attribute_name = schema_builder.add_text_field(
            "attribute_name",
            TextOptions::default().set_fast(None).set_stored(),
        );

        // faceted name of the option for access to related fields
        schema_builder.add_facet_field("name_facet", FacetOptions::default());

        // split up name of the option for search
        schema_builder.add_text_field("name", name_field_options);

        // description
        schema_builder.add_text_field("description", TEXT);

        let schema = schema_builder.build();

        let index = open_or_create_index(&self.index_path, &schema)?;

        let options_tk = TextAnalyzer::builder(WhitespaceTokenizer::default()).build();
        index.tokenizers().register("option_name", options_tk);

        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::OnCommit)
            .try_into()
            .unwrap();

        self.map = HashMap::new();
        self.inner = Some(SearcherInner {
            schema,
            index,
            reader,
            reference_field: attribute_name,
        });

        Ok(())
    }

    /// updates indexed + cached entries with new ones
    fn update_entries(&mut self, entries: HashMap<String, Self::Item>) -> anyhow::Result<()> {
        let Some(ref inner) = self.inner else {
            anyhow::bail!("can not update options before index creation");
        };

        let index = &inner.index;
        let schema = &inner.schema;

        let mut index_writer = index.writer(50_000_000)?;
        let name = schema
            .get_field("name")
            .expect("the field name should exist");
        let name_facet = schema
            .get_field("name_facet")
            .expect("the field name_facet should exist");
        let attribute_name = schema
            .get_field("attribute_name")
            .expect("the field attribute_name should exist");
        let description = schema
            .get_field("description")
            .expect("the description field should exist");

        index_writer
            .delete_all_documents()
            .expect("failed to delete all documents");

        for (option_name, option) in &entries {
            let mut document = Document::default();
            document.add_text(attribute_name, option_name.clone());
            document.add_text(name, option_name.replace('.', " "));
            document.add_facet(name_facet, Facet::from_path(option_name.clone().split('.')));
            document.add_text(description, option.description.0.clone());
            index_writer.add_document(document)?;
        }

        index_writer.commit()?;
        self.map = entries;
        Ok(())
    }

    fn collector(&self) -> impl Collector<Fruit = Vec<FCFruit>> {
        TopDocs::with_limit(20).tweak_score(move |segment_reader: &SegmentReader| {
            let store_reader = segment_reader.get_store_reader(100).unwrap();

            move |doc: DocId, mut score: Score| {
                let d = store_reader.get(doc).unwrap();
                let attribute_name = d.field_values().first().unwrap().value.as_text().unwrap();

                let fcio_option = attribute_name.starts_with("flyingcircus");
                let enable_option = attribute_name.ends_with("enable");
                let roles_option = attribute_name.contains("roles");

                if fcio_option {
                    score *= 1.3;
                }
                if enable_option {
                    score *= 1.05;
                }
                if roles_option {
                    score *= 0.8;
                }

                (score, 1.0)
            }
        })
    }
}
