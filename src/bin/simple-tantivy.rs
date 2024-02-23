use fc_search::NixosOption;
use std::collections::HashMap;
use tempfile::TempDir;

use fc_search::search::{create_index, search_entries, write_entries};

fn main() -> tantivy::Result<()> {
    let index_path = TempDir::new()?.into_path();

    create_index(&index_path)?;

    let options: HashMap<String, NixosOption> =
        serde_json::from_str(&std::fs::read_to_string("out.json")?)?;

    write_entries(&index_path, &options)?;

    let results = search_entries(&index_path, "flyingcircus.roles.devhost enable".to_string())?;

    dbg!(&results);

    Ok(())
}
