use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tantivy::collector::{Collector, TopDocs};
use tantivy::query::{
    BooleanQuery, BoostQuery, FuzzyTermQuery, Occur, Query, RegexQuery, TermQuery,
};
use tantivy::schema::{Schema, TextFieldIndexing, TextOptions, TEXT};
use tantivy::{DocId, Document, Score, SegmentReader, Term};

use super::{open_or_create_index, FCFruit, Searcher, SearcherInner};
use crate::nix::NixPackage;

pub struct PackagesSearcher {
    pub index_path: PathBuf,
    pub packages: HashMap<String, NixPackage>,
    inner: Option<SearcherInner>,
}

impl PackagesSearcher {
    pub fn new(index_path: &Path) -> Self {
        Self {
            index_path: index_path.to_path_buf(),
            packages: HashMap::new(),
            inner: None,
        }
    }

    pub fn new_with_packages(
        index_path: &Path,
        packages: HashMap<String, NixPackage>,
    ) -> anyhow::Result<Self> {
        let mut ret = Self::new(index_path);
        ret.create_index()?;
        ret.update_entries(packages)?;
        Ok(ret)
    }
}

impl Searcher for PackagesSearcher {
    type Item = NixPackage;

    fn entries(&self) -> &HashMap<String, Self::Item> {
        &self.packages
    }

    fn inner(&self) -> Option<&SearcherInner> {
        self.inner.as_ref()
    }

    fn parse_query(&self, query_string: &str) -> Box<dyn Query> {
        let Some(ref inner) = self.inner else {
            unreachable!("searcher not initialized, cannot parse");
        };

        let attribute_name = inner.schema.get_field("attribute_name").unwrap();
        let description = inner.schema.get_field("description").unwrap();
        let mut subqueries: Vec<(Occur, Box<dyn Query>)> = vec![];

        for (i, word) in query_string.split(" ").enumerate() {
            // words further back in the query get assigned less importance
            let length_loss = 1. - i as f32 / 10.;

            let qlen = word.len();

            let name_term = Term::from_field_text(attribute_name, word);
            let description_term = Term::from_field_text(description, word);

            // search for exact fit on the name field, highest priority
            subqueries.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(TermQuery::new(
                        name_term.clone(),
                        tantivy::schema::IndexRecordOption::WithFreqsAndPositions,
                    )),
                    1.3,
                )),
            ));

            // search for possible regex matches on the name field
            if let Ok(regex_query) = RegexQuery::from_pattern(query_string, attribute_name) {
                subqueries.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(Box::new(regex_query), 1.2 * length_loss)),
                ));
            }

            // fuzzily search on the name field
            if qlen > 1 {
                let fq = FuzzyTermQuery::new_prefix(name_term.clone(), 0, true);
                subqueries.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(Box::new(fq), 1.1 * length_loss)),
                ));
            }

            if qlen > 2 {
                let fq = FuzzyTermQuery::new_prefix(name_term.clone(), 1, true);
                subqueries.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(Box::new(fq), length_loss)),
                ));
            }

            // search for exact fit on the description field
            // similar priority to a fuzzy search on the name field
            subqueries.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(TermQuery::new(
                        description_term.clone(),
                        tantivy::schema::IndexRecordOption::WithFreqsAndPositions,
                    )),
                    1.2 * length_loss,
                )),
            ));

            if qlen > 2 {
                let fq = FuzzyTermQuery::new_prefix(description_term.clone(), 1, true);
                subqueries.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(Box::new(fq), length_loss)),
                ));
            }
        }

        Box::new(BooleanQuery::new(subqueries))
    }

    #[tracing::instrument(skip(self))]
    fn create_index(&mut self) -> anyhow::Result<()> {
        let mut schema_builder = Schema::builder();

        let raw_stored = TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_index_option(tantivy::schema::IndexRecordOption::WithFreqsAndPositions)
                    .set_tokenizer("raw"),
            )
            .set_stored();

        let attribute_name = schema_builder.add_text_field("attribute_name", raw_stored);
        schema_builder.add_text_field("description", TEXT);
        let schema = schema_builder.build();

        let index = open_or_create_index(&self.index_path, &schema)?;

        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::OnCommit)
            .try_into()
            .unwrap();

        self.packages = HashMap::new();
        self.inner = Some(SearcherInner {
            schema,
            index,
            reader,
            reference_field: attribute_name,
        });

        Ok(())
    }

    fn update_entries(&mut self, entries: HashMap<String, Self::Item>) -> anyhow::Result<()> {
        let Some(ref inner) = self.inner else {
            anyhow::bail!("can not update options before index creation");
        };

        let index = &inner.index;
        let schema = &inner.schema;
        let mut index_writer = index.writer(50_000_000)?;

        let attribute_name = schema
            .get_field("attribute_name")
            .expect("the field attribute_name should exist");
        let description = schema
            .get_field("description")
            .expect("the field description should exist");

        index_writer
            .delete_all_documents()
            .expect("failed to delete all documents");
        for (aname, package) in &entries {
            let mut document = Document::default();
            document.add_text(attribute_name, aname.clone());
            document.add_text(description, package.description.clone().unwrap_or_default());
            index_writer.add_document(document)?;
        }

        index_writer.commit()?;
        self.packages = entries;
        Ok(())
    }

    fn collector(&self) -> impl Collector<Fruit = Vec<FCFruit>> {
        TopDocs::with_limit(20).tweak_score(move |segment_reader: &SegmentReader| {
            let store_reader = segment_reader.get_store_reader(10).unwrap();
            move |doc: DocId, score: Score| {
                let d = store_reader.get(doc).unwrap();
                let name = d.field_values().first().unwrap().value.as_text().unwrap();
                (score, 1. / name.len() as f32)
            }
        })
    }
}
