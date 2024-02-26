use super::NixosOption;
use std::collections::HashMap;
use std::path::PathBuf;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Schema, FAST, STORED, TEXT};
use tantivy::{Document, Index};

pub fn create_index(index_path: &PathBuf) -> tantivy::Result<()> {
    let mut schema_builder = Schema::builder();

    schema_builder.add_text_field("name", TEXT | STORED | FAST);
    schema_builder.add_text_field("description", TEXT);
    schema_builder.add_text_field("default", TEXT);

    let schema = schema_builder.build();

    Index::create_in_dir(index_path, schema.clone())?;
    Ok(())
}

pub fn write_entries(
    index_path: &PathBuf,
    entries: &HashMap<String, NixosOption>,
) -> tantivy::Result<()> {
    let index = Index::open_in_dir(index_path)?;
    let schema = index.schema();

    let mut index_writer = index.writer(50_000_000)?;

    let name = schema.get_field("name").expect("the field should exist");
    let description = schema
        .get_field("description")
        .expect("the field should exist");
    let default = schema.get_field("default").expect("the field should exist");

    for (option_name, option) in entries {
        let mut document = Document::default();
        document.add_text(name, option_name.clone());
        document.add_text(description, option.description.clone().unwrap_or_default());
        document.add_text(
            default,
            option.default.clone().map(|e| e.text).unwrap_or_default(),
        );
        index_writer.add_document(document)?;
    }

    index_writer.commit()?;
    Ok(())
}

pub fn search_entries(index_path: &PathBuf, query: String) -> tantivy::Result<Vec<String>> {
    let index = Index::open_in_dir(index_path)?;
    let schema = index.schema();
    let name = schema.get_field("name").expect("the field should exist");
    let description = schema
        .get_field("description")
        .expect("the field should exist");

    let default = schema.get_field("default").expect("the field should exist");

    let reader = index
        .reader_builder()
        .reload_policy(tantivy::ReloadPolicy::OnCommit)
        .try_into()?;

    let searcher = reader.searcher();
    let query_parser = QueryParser::for_index(&index, vec![name, description, default]);

    {
        // needs query_parser, searcher, schema

        // this part happens per request in the server
        let query = query_parser.parse_query(&query)?;
        let top_docs = searcher.search(&query, &TopDocs::with_limit(10))?;

        Ok(top_docs
            .into_iter()
            .map(|(_score, doc_address)| {
                let retrieved = searcher.doc(doc_address).unwrap();
                retrieved
                    .get_first(name)
                    .expect("result has a value for name")
                    .as_text()
                    .expect("value is text")
                    .to_string()
            })
            .collect())
    }
}
