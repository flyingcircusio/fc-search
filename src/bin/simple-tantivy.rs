use fc_search::nix::NixosOption;
use fc_search::option_to_naive;
use std::collections::HashMap;
use tempfile::TempDir;

use fc_search::search::{options::OptionsSearcher, Searcher};

fn main() -> anyhow::Result<()> {
    let index_path = TempDir::new()?;

    let naive_options = {
        let options: HashMap<String, NixosOption> =
            serde_json::from_str(&std::fs::read_to_string("out.json")?)?;
        option_to_naive(&options)
    };

    let searcher = OptionsSearcher::new_with_options(index_path.path(), naive_options)?;
    let results = searcher.search_entries("flyingcircus.roles.devhost enable");

    dbg!(&results);
    Ok(())
}
