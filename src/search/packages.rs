use std::collections::HashMap;

use tantivy::query::{
    BooleanQuery, BoostQuery, FuzzyTermQuery, Occur, Query, RegexQuery, TermQuery,
};
use tantivy::schema::{Schema, TextFieldIndexing, TextOptions, TEXT};
use tantivy::{doc, DocId, Score, SegmentReader, TantivyDocument, Term};

use super::{GenericSearcher, Searcher};
use crate::nix::NixPackage;

impl Searcher for GenericSearcher<NixPackage> {
    type Item = NixPackage;

    fn parse_query(&self, query_string: &str) -> Box<dyn Query> {
        let name_field = self.inner.schema.get_field("name").unwrap();
        let description = self.inner.schema.get_field("description").unwrap();
        let mut subqueries: Vec<(Occur, Box<dyn Query>)> = vec![];

        for (i, word) in query_string.split(' ').enumerate() {
            // words further back in the query get assigned less importance
            let length_loss = 1. - i as f32 / 10.;

            let qlen = word.len();

            let name_term = Term::from_field_text(name_field, word);
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
            if let Ok(regex_query) = RegexQuery::from_pattern(query_string, name_field) {
                subqueries.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(Box::new(regex_query), 1.2 * length_loss)),
                ));
            }

            // fuzzily search on the name field
            let fq = FuzzyTermQuery::new(name_term.clone(), 2, true);
            subqueries.push((
                Occur::Should,
                Box::new(BoostQuery::new(Box::new(fq), 0.9 * length_loss)),
            ));

            if qlen > 2 {
                let fq = FuzzyTermQuery::new(name_term.clone(), 1, true);
                subqueries.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(Box::new(fq), 1.2 * length_loss)),
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

    fn schema() -> (tantivy::schema::Field, tantivy::schema::Schema) {
        let mut schema_builder = Schema::builder();

        let name_field_options = TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_index_option(tantivy::schema::IndexRecordOption::WithFreqsAndPositions)
                    .set_tokenizer("raw"),
            )
            .set_stored();

        let attribute_name = schema_builder.add_text_field(
            "attribute_name",
            TextOptions::default().set_fast(None).set_stored(),
        );
        schema_builder.add_text_field("name", name_field_options);
        schema_builder.add_text_field("description", TEXT);
        let schema = schema_builder.build();

        (attribute_name, schema)
    }

    fn update_entries(&mut self, entries: HashMap<String, Self::Item>) -> anyhow::Result<()> {
        let index = &self.inner.index;
        let schema = &self.inner.schema;
        let mut index_writer = index.writer(50_000_000)?;

        let attribute_name = self.inner.reference_field;
        let name = schema.get_field("name").expect("name field should exist");
        let description = schema
            .get_field("description")
            .expect("the field description should exist");

        index_writer
            .delete_all_documents()
            .expect("failed to delete all documents");
        index_writer.commit()?;

        index_writer
            .run(entries.iter().map(|(aname, package)| {
                tantivy::indexer::UserOperation::Add(doc! {
                    attribute_name => aname.clone(),
                    name => package.name.clone(),
                    description => package.description.clone().unwrap_or_default()
                })
            }))
            .unwrap();

        index_writer.commit()?;
        self.map = entries;
        self.inner.reader.reload().unwrap();

        Ok(())
    }

    fn scorer() -> impl tantivy::collector::ScoreTweaker<(f32, f32)> + Send {
        |segment_reader: &SegmentReader| {
            let store_reader = segment_reader.get_store_reader(10).unwrap();
            move |doc: DocId, score: Score| {
                let d: TantivyDocument = store_reader.get(doc).unwrap();
                let tantivy::schema::OwnedValue::Str(attribute_name) =
                    d.field_values().first().unwrap().value()
                else {
                    unreachable!("can't be anything else");
                };
                (score, 1. / attribute_name.len() as f32)
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_search() {
        let tmp = tempdir().unwrap();
        let mut searcher = GenericSearcher::<NixPackage>::new(&tmp.path());

        let mut entries = HashMap::new();
        let entry = NixPackage {
            attribute_name: "gitlab-workhorse".to_string(),
            default_output: "".to_string(),
            description: None,
            long_description: None,
            license: crate::nix::Plurality::None,
            name: "gitlab-workhorse".to_string(),
            outputs: vec![],
            version: None,
            homepage: crate::nix::Plurality::None,
        };

        entries.insert("gitlab-workhorse".to_string(), entry.clone());
        searcher.update_entries(entries).unwrap();

        let results = searcher.search_entries("gitlab-wrkhorse", 10, 1);
        assert_eq!(results, vec![entry]);
    }
}
