use fc_search::nix::NixosOption;
use fc_search::search::GenericSearcher;
use fc_search::{option_to_naive, NaiveNixosOption};
use std::collections::HashMap;
use tempfile::TempDir;

fn main() -> anyhow::Result<()> {
    let index_path = TempDir::new()?;

    let naive_options = {
        let options: HashMap<String, NixosOption> =
            serde_json::from_str(&std::fs::read_to_string("out.json")?)?;
        option_to_naive(&options)
    };

    let searcher =
        GenericSearcher::<NaiveNixosOption>::new_with_values(index_path.path(), naive_options)?;
    let results = searcher.search_entries("flyingcircus.roles.devhost enable", 15, 1);

    dbg!(&results);
    Ok(())
}
