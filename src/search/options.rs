use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tantivy::collector::{Collector, TopDocs};
use tantivy::query::{
    BooleanQuery, BoostQuery, ConstScoreQuery, FuzzyTermQuery, Occur, Query, TermQuery,
};
use tantivy::schema::{Facet, FacetOptions, Schema, TextOptions, TEXT};
use tantivy::{DocId, Document, Score, SegmentReader, Term};

use super::{open_or_create_index, FCFruit, Searcher, SearcherInner};
use crate::{LogValue, NaiveNixosOption};

pub struct OptionsSearcher {
    pub index_path: PathBuf,
    pub options: HashMap<String, NaiveNixosOption>,
    inner: Option<SearcherInner>,
}

impl OptionsSearcher {
    pub fn new(index_path: &Path) -> Self {
        Self {
            index_path: index_path.to_path_buf(),
            options: HashMap::new(),
            inner: None,
        }
    }

    pub fn new_with_options(
        index_path: &Path,
        options: HashMap<String, NaiveNixosOption>,
    ) -> anyhow::Result<Self> {
        let mut ret = Self::new(index_path);
        ret.create_index()?;
        ret.update_entries(options)?;
        Ok(ret)
    }
}

impl Searcher for OptionsSearcher {
    type Item = NaiveNixosOption;

    fn entries(&self) -> &HashMap<String, Self::Item> {
        &self.options
    }

    fn inner(&self) -> Option<&SearcherInner> {
        self.inner.as_ref()
    }

    fn parse_query(&self, query_string: &str) -> Box<dyn Query> {
        let Some(ref inner) = self.inner else {
            unreachable!("searcher not initialized, cannot parse");
        };
        let mut subqueries: Vec<(Occur, Box<dyn Query>)> = vec![];

        let name_field = inner.schema.get_field("name").unwrap();
        for (i, word) in query_string.split(" ").enumerate() {
            let qlen = word.len();
            let name_term = Term::from_field_text(name_field, word);

            // words further back in the query get assigned less importance
            let length_loss = 1. - i as f32 / 10.;

            // search for exact fit on the name field, highest priority
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

            // fuzzily search on the name field
            let fq =
                FuzzyTermQuery::new_prefix(name_term.clone(), qlen.clamp(2, 4) as u8 - 2, true);
            subqueries.push((Occur::Should, Box::new(BoostQuery::new(Box::new(fq), 2.2))));
        }

        //description queries
        let mut description_subqueries: Vec<(Occur, Box<dyn Query>)> = vec![];
        let description_field = inner.schema.get_field("description").unwrap();
        for (i, word) in query_string.split(" ").enumerate() {
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

        //println!("{:#?}", subqueries);

        Box::new(BooleanQuery::new(subqueries))
    }

    /// creates the index and initializes the struct that holds
    /// fields that are important for searching
    #[tracing::instrument(skip(self))]
    fn create_index(&mut self) -> anyhow::Result<()> {
        let mut schema_builder = Schema::builder();

        // name of the option, stored to access it's data from the searcher's hashmap
        let attribute_name = schema_builder.add_text_field(
            "attribute_name",
            TextOptions::default().set_fast(None).set_stored(),
        );

        // faceted name of the option for access to related fields
        schema_builder.add_facet_field("name_facet", FacetOptions::default());

        // split up name of the option for search
        schema_builder.add_text_field("name", TEXT);

        // description
        schema_builder.add_text_field("description", TEXT);

        let schema = schema_builder.build();
        let index = open_or_create_index(&self.index_path, &schema)?;

        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::OnCommit)
            .try_into()
            .unwrap();

        self.options = HashMap::new();
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
        self.options = entries;
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
