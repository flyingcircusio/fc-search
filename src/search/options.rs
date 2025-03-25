use itertools::Itertools;
use std::collections::HashMap;
use tantivy::indexer::UserOperation;
use tantivy::query::{
    BooleanQuery, BoostQuery, ConstScoreQuery, FuzzyTermQuery, Occur, PhraseQuery, Query, TermQuery,
};
use tantivy::schema::{Facet, FacetOptions, Schema, TextFieldIndexing, TextOptions, TEXT};
use tantivy::{doc, DocId, Score, SegmentReader, TantivyDocument, Term};

use super::{GenericSearcher, Searcher};
use crate::NaiveNixosOption;

impl Searcher for GenericSearcher<NaiveNixosOption> {
    type Item = NaiveNixosOption;

    fn parse_query(&self, query_string: &str) -> Box<dyn Query> {
        let mut subqueries: Vec<(Occur, Box<dyn Query>)> = vec![];

        let name_field = self.inner.schema.get_field("name").unwrap();
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
        let description_field = self.inner.schema.get_field("description").unwrap();
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

    fn schema() -> (tantivy::schema::Field, tantivy::schema::Schema) {
        let mut schema_builder = Schema::builder();

        let name_field_options = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_index_option(tantivy::schema::IndexRecordOption::WithFreqsAndPositions)
                .set_tokenizer("default_ws"),
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

        (attribute_name, schema)
    }

    /// updates indexed + cached entries with new ones
    fn update_entries(&mut self, entries: HashMap<String, Self::Item>) -> anyhow::Result<()> {
        let index = &self.inner.index;
        let schema = &self.inner.schema;

        let mut index_writer = index.writer(50_000_000)?;

        let name = schema
            .get_field("name")
            .expect("the field name should exist");
        let name_facet = schema
            .get_field("name_facet")
            .expect("the field name_facet should exist");
        let attribute_name = self.inner.reference_field;
        let description = schema
            .get_field("description")
            .expect("the description field should exist");

        index_writer
            .delete_all_documents()
            .expect("failed to delete all documents");
        index_writer.commit().unwrap();

        index_writer
            .run(entries.iter().map(|(option_name, option)| {
                UserOperation::Add(doc! {
                    attribute_name => option_name.clone(),
                    name => option_name.replace('.', " "),
                    name_facet => Facet::from_path(option_name.clone().split('.')),
                    description => option.description.0.clone()
                })
            }))
            .unwrap();

        index_writer.commit().unwrap();
        self.map = entries;
        self.inner.reader.reload().unwrap();

        Ok(())
    }

    fn scorer() -> impl tantivy::collector::ScoreTweaker<(f32, f32)> {
        move |segment_reader: &SegmentReader| {
            // TODO: replace with much more efficient faceted search
            // should not read the entire Document to score it based on its name
            let store_reader = segment_reader.get_store_reader(100).unwrap();

            move |doc: DocId, mut score: Score| {
                let d: TantivyDocument = store_reader.get(doc).unwrap();
                let tantivy::schema::OwnedValue::Str(attribute_name) =
                    d.field_values().first().unwrap().value()
                else {
                    unreachable!("can't be anything else");
                };

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

                (score, 1.0f32)
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::Html;
    use tempfile::tempdir;

    #[test]
    fn test_search() {
        let tmp = tempdir().unwrap();
        let mut searcher = GenericSearcher::<NaiveNixosOption>::new(&tmp.path());

        let mut entries = HashMap::new();
        let entry = NaiveNixosOption {
            name: "foo".to_string(),
            declarations: Vec::new(),
            description: Html("foo".to_string()),
            default: Html("foo".to_string()),
            example: Html(String::new()),
            option_type: String::new(),
            read_only: false,
        };
        entries.insert("foo".to_string(), entry.clone());

        searcher.update_entries(entries).unwrap();

        let results = searcher.search_entries("foo", 10, 1);
        assert_eq!(results, vec![entry]);
    }
}
