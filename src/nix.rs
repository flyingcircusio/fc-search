use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Display;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use tracing::{debug, error};
use url::Url;

use crate::{option_to_naive, Flake, NaiveNixosOption};

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

#[derive(Deserialize, Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct License {
    pub free: Option<bool>,
    pub full_name: Option<String>,
    pub redistributable: Option<bool>,
    pub short_name: Option<String>,
    pub spdx_id: Option<String>,
    pub url: Option<Url>,
}

#[derive(Deserialize, Debug, Serialize, Clone)]
#[serde(untagged)]
pub enum LicenseT {
    Verbatim(String),
    Informative(License),
}

#[derive(Deserialize, Debug, Serialize, Clone)]
#[serde(untagged)]
pub enum LicenseType {
    Single(LicenseT),
    Multiple(Vec<LicenseT>),
}

impl Default for LicenseType {
    fn default() -> Self {
        LicenseType::Single(LicenseT::Verbatim("unknown".to_string()))
    }
}

impl Display for LicenseType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&serde_json::to_string_pretty(self).unwrap_or_default())
    }
}

#[derive(Deserialize, Debug, Serialize, Clone)]
pub struct NixPackage {
    pub attribute_name: String,
    pub default_output: String,
    pub description: Option<String>,
    #[serde(rename = "camelCase")]
    pub long_description: Option<String>,
    pub license: Option<LicenseType>,
    pub name: String,
    pub outputs: Vec<String>,
    pub version: Option<String>,
}

#[derive(RustEmbed)]
#[folder = "nix/"]
struct NixFiles;

#[tracing::instrument(skip(flake))]
pub fn build_options_for_fcio_branch(
    flake: &Flake,
) -> anyhow::Result<(
    HashMap<String, NaiveNixosOption>,
    HashMap<String, NixPackage>,
)> {
    let eval_nixfile = {
        let data = NixFiles::get("eval.nix").unwrap().data;
        let mut tmp = tempfile::NamedTempFile::new()?;
        tmp.write_all(&data)?;
        tmp
    };

    debug!("starting nix-instantiate");
    let derivation_cmd = Command::new("nix-instantiate")
        .arg(eval_nixfile.path())
        .args(["--argstr", "flake", &flake.flake_uri()])
        .output()?;

    drop(eval_nixfile);

    if !derivation_cmd.status.success() {
        let stderr = String::from_utf8(derivation_cmd.stderr).expect("valid utf-8 in stderr");
        error!("failed instantiating: {}", stderr);
        anyhow::bail!(
            "failed to instantiate options for {}\nstderr: {}",
            flake.flake_uri(),
            stderr
        );
    }
    debug!("finished nix-instantiate");

    let derivation_output = std::str::from_utf8(&derivation_cmd.stdout)
        .expect("valid utf-8")
        .trim_end();

    debug!("starting nix-build");
    let build_cmd = Command::new("nix-build").arg(derivation_output).output()?;

    if !build_cmd.status.success() {
        let stderr = String::from_utf8(build_cmd.stderr).expect("valid utf-8 in stderr");
        error!("failed building: {}", stderr);
        anyhow::bail!(
            "failed to build options for {}\nstderr: {}",
            flake.flake_uri(),
            stderr
        );
    }
    debug!("finished nix-build");

    let build_output = std::str::from_utf8(&build_cmd.stdout)
        .expect("valid utf-8")
        .trim_end();

    let path = PathBuf::from(build_output);

    debug!("build output path is `{}`", path.display());

    let options_json = std::fs::read_to_string(path.join("options.json")).unwrap();
    let packages_json = std::fs::read_to_string(path.join("packages.json")).unwrap();
    let nixpkgs_path = std::fs::read_to_string(path.join("nixpkgs"))
        .expect("could not read path to nixpkgs in store")
        .trim()
        .to_string();
    let fc_nixos_path = std::fs::read_to_string(path.join("fc-nixos"))
        .expect("could not read path to fc-nixos in store")
        .trim()
        .to_string();

    debug!("nixpkgs path is `{}`", nixpkgs_path);
    debug!("fc_nixos path is `{}`", fc_nixos_path);

    // TODO infer actual nixpkgs url from versions
    let nixpkgs_url = "https://github.com/nixos/nixpkgs/blob/master";

    let packages = serde_json::from_str(&packages_json)?;
    let options =
        serde_json::from_str(&options_json).map(|mut options: HashMap<String, NixosOption>| {
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
        })?;
    let options = option_to_naive(&options);
    Ok((options, packages))
}
