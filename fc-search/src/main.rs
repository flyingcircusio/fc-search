use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use fc_search::NixosOption;

fn main() {
    let fc_nixos_url = "github:PhilTaken/fc-nixos/flake2.0";

    let json_file = Command::new("nix")
        .args([
            "build",
            ".#options",
            "--impure",
            "--print-out-paths",
            "--no-link",
        ])
        .args(["--override-input", "fc-nixos", fc_nixos_url])
        .output()
        .unwrap()
        .stdout;

    let json_file = std::str::from_utf8(&json_file)
        .expect("valid utf-8")
        .strip_suffix('\n')
        .unwrap();

    // TODO logging / tracing
    println!("[fc-search] reading json from file {json_file:#?}");

    let contents = std::fs::read_to_string(Path::new(&json_file)).unwrap();
    let vals: HashMap<String, NixosOption> = serde_json::from_str(&contents).unwrap();

    let outstring = serde_json::to_string(&vals).unwrap();
    std::fs::write("out.json", outstring).unwrap();
}
