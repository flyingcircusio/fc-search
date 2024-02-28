pub mod search;

use itertools::Itertools;
use reqwest::header::HeaderMap;
use reqwest::Client;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use url::Url;

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

fn deserialize_declaration<'de, D>(deserializer: D) -> Result<Declaration, D::Error>
where
    D: Deserializer<'de>,
{
    let buf = String::deserialize(deserializer)?;
    Ok(Declaration::Naive(buf))
}

#[derive(Deserialize, Debug, Serialize, Clone)]
#[serde(untagged)]
pub enum Declaration {
    Naive(String),
    Processed(Url),
}

impl Declaration {
    pub fn to_string(&self) -> String {
        match self {
            Self::Naive(s) => s.to_string(),
            Self::Processed(u) => u.as_str().to_string(),
        }
    }
}

// TODO include name during deserialization from hashmap
#[derive(Deserialize, Debug, Serialize, Clone, Default)]
pub struct NixosOption {
    pub declarations: Vec<Declaration>,
    pub default: Option<Expression>,
    pub description: Option<String>,
    pub example: Option<Expression>,
    #[serde(rename = "readOnly")]
    pub read_only: bool,
    #[serde(rename = "type")]
    pub option_type: String,
}

#[derive(Debug, Deserialize)]
struct Project {
    jobsets: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct JobsetInput {
    value: String,
}

#[derive(Debug, Deserialize)]
struct Jobset {
    inputs: HashMap<String, JobsetInput>,
}

#[derive(Debug)]
pub struct Flake {
    pub owner: String,
    pub name: String,
    pub branch: String,
}

impl Flake {
    pub fn flake_uri(&self) -> String {
        format!("github:{}/{}/{}", self.owner, self.name, self.branch)
    }

    pub fn github_base_url(&self) -> String {
        format!(
            "https://github.com/{}/{}/blob/{}",
            self.owner, self.name, self.branch
        )
    }
}

const HYDRA_BASE_URL: &str = "https://hydra.flyingcircus.io";

pub async fn get_fcio_flake_uris() -> anyhow::Result<Vec<Flake>> {
    let mut headers = HeaderMap::new();
    headers.insert("Accept", "application/json".parse()?);
    let client = Client::builder().default_headers(headers).build()?;

    let project_id = "flyingcircus";

    let query_result = client
        .get(format!("{HYDRA_BASE_URL}/project/{project_id}"))
        .send()
        .await?
        .text()
        .await?;

    let project: Project = serde_json::from_str(&query_result)?;

    let jobsets: Vec<_> = project
        .jobsets
        .iter()
        .filter(|j| {
            j.starts_with("fc-")
                && (j.ends_with("production") || j.ends_with("dev") || j.ends_with("staging"))
        })
        .sorted()
        .collect();

    let mut ret = Vec::new();

    for jobset_id in jobsets {
        let jobset = client
            .get(format!("{HYDRA_BASE_URL}/jobset/{project_id}/{jobset_id}"))
            .send()
            .await?
            .text()
            .await?;

        let jobset: Jobset = serde_json::from_str(&jobset).unwrap();

        let Some(fc) = jobset.inputs.get("fc") else {
            // println!("{jobset_id} has no input 'fc'");
            continue;
        };

        let (repo, branch) = fc
            .value
            .split_once(' ')
            .expect("value has scheme `uri branch`");

        // TODO error handling?
        assert_eq!(repo, "https://github.com/flyingcircusio/fc-nixos");

        ret.push(Flake {
            owner: "flyingcircusio".to_string(),
            name: "fc-nixos".to_string(),
            branch: branch.to_string(),
        });
    }

    Ok(ret)
}

pub fn build_options_for_input(fc_nixos: &Flake) -> Option<HashMap<String, NixosOption>> {
    let build_command = Command::new("nix")
        .args([
            "build",
            ".#options",
            "--impure",
            "--print-out-paths",
            "--no-link",
        ])
        .args(["--override-input", "fc-nixos", &fc_nixos.flake_uri()])
        .output()
        .unwrap();

    if !build_command.status.success() {
        let stderr = String::from_utf8(build_command.stderr).expect("valid utf-8 in stderr");
        println!(
            "failed to build options for {}\nstderr: {}",
            fc_nixos.flake_uri(),
            stderr
        );
        return None;
    }

    let build_output = std::str::from_utf8(&build_command.stdout)
        .expect("valid utf-8")
        .strip_suffix('\n')
        .unwrap();

    // TODO logging / tracing
    println!("[fc-search] reading json from directory {build_output:#?}");

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

    dbg!(&nixpkgs_path);
    dbg!(&fc_nixos_path);

    let nixpkgs_url = "https://github.com/nixos/nixpkgs/blob/master";

    let raw_options = serde_json::from_str(&contents)
        .map(|mut options: HashMap<String, NixosOption>| {
            for (_, option) in options.iter_mut() {
                for dec in option.declarations.iter_mut() {
                    if let Declaration::Naive(ref mut declaration) = dec.clone() {
                        let decl = if declaration.starts_with(&nixpkgs_path) {
                            declaration.replace(&nixpkgs_path, &nixpkgs_url)
                        } else {
                            declaration.replace(&fc_nixos_path, &fc_nixos.github_base_url())
                        };

                        let Ok(mut url) = Url::parse(&decl) else {
                            continue;
                        };

                        if !url.path().ends_with(".nix") {
                            url = url
                                .join("default.nix")
                                .expect("could not join url with simple string");
                        }

                        *dec = Declaration::Processed(url);
                    }
                }
            }

            options
        })
        .ok();

    raw_options
}
