use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use tracing::{debug, info};

use crate::Flake;

#[derive(Deserialize, Debug, Serialize, Clone)]
pub enum ExpressionType {
    #[serde(rename = "literalExpression")]
    LiteralExpression,
    #[serde(rename = "literalMD")]
    LiteralMd,
}

#[derive(Deserialize, Debug, Serialize, Clone)]
pub struct Expression {
    #[serde(rename = "_type")]
    pub option_type: ExpressionType,
    pub text: String,
}

// TODO include name during deserialization from hashmap
#[derive(Deserialize, Debug, Serialize, Clone, Default)]
pub struct NixosOption {
    pub declarations: Vec<String>,
    pub default: Option<Expression>,
    pub description: Option<String>,
    pub example: Option<Expression>,
    #[serde(rename = "readOnly")]
    pub read_only: bool,
    #[serde(rename = "type")]
    pub option_type: String,
}

#[derive(RustEmbed)]
#[folder = "nix/"]
struct NixFiles;

pub fn build_options_for_fcio_branch(
    flake: &Flake,
) -> anyhow::Result<HashMap<String, NixosOption>> {
    anyhow::ensure!(flake.owner == "flyingcircusio");
    anyhow::ensure!(flake.name == "fc-nixos");

    let eval_nixfile = {
        let data = NixFiles::get("eval.nix").unwrap().data;
        let mut tmp = tempfile::NamedTempFile::new()?;
        tmp.write_all(&data)?;
        tmp
    };

    let options_nixfile = {
        let data = NixFiles::get("options.nix").unwrap().data;
        let mut tmp = tempfile::NamedTempFile::new()?;
        tmp.write_all(&data)?;
        tmp
    };

    let derivation_cmd = Command::new("nix-instantiate")
        .arg(eval_nixfile.path())
        .args(["--argstr", "branch", &flake.branch])
        .args([
            "--argstr",
            "options_nix",
            &options_nixfile.path().display().to_string(),
        ])
        .output()?;

    drop(eval_nixfile);
    drop(options_nixfile);

    if !derivation_cmd.status.success() {
        let stderr = String::from_utf8(derivation_cmd.stderr).expect("valid utf-8 in stderr");
        anyhow::bail!(
            "failed to instantiate options for {}\nstderr: {}",
            flake.flake_uri(),
            stderr
        );
    }

    let derivation_output = std::str::from_utf8(&derivation_cmd.stdout)
        .expect("valid utf-8")
        .trim_end();

    let build_cmd = Command::new("nix-build").arg(derivation_output).output()?;

    if !build_cmd.status.success() {
        let stderr = String::from_utf8(build_cmd.stderr).expect("valid utf-8 in stderr");
        anyhow::bail!(
            "failed to build options for {}\nstderr: {}",
            flake.flake_uri(),
            stderr
        );
    }

    let build_output = std::str::from_utf8(&build_cmd.stdout)
        .expect("valid utf-8")
        .trim_end();

    let path = PathBuf::from(build_output);

    let contents = std::fs::read_to_string(path.join("options.json")).unwrap();
    let nixpkgs_path = std::fs::read_to_string(path.join("nixpkgs"))
        .expect("could not read path to nixpkgs in store")
        .trim()
        .to_string();
    let fc_nixos_path = std::fs::read_to_string(path.join("fc-nixos"))
        .expect("could not read path to fc-nixos in store")
        .trim()
        .to_string();

    debug!("nixpkgs path is {}", nixpkgs_path);
    debug!("fc_nixos path is {}", fc_nixos_path);

    // TODO infer actual nixpkgs url from versions.json
    let nixpkgs_url = "https://github.com/nixos/nixpkgs/blob/master";

    Ok(
        serde_json::from_str(&contents).map(|mut options: HashMap<String, NixosOption>| {
            for (_, option) in options.iter_mut() {
                for declaration in option.declarations.iter_mut() {
                    let decl = if declaration.starts_with(&nixpkgs_path) {
                        declaration.replace(&nixpkgs_path, nixpkgs_url)
                    } else {
                        declaration.replace(&fc_nixos_path, &flake.github_base_url())
                    };

                    *declaration = decl;
                }
            }

            options
        })?,
    )
}
