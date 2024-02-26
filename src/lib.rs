pub mod search;

use itertools::Itertools;
use reqwest::header::HeaderMap;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

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
            .split_once(" ")
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

pub fn build_options_for_input(fc_nixos_url: &str) -> Option<HashMap<String, NixosOption>> {
    let json_file_cmd = Command::new("nix")
        .args([
            "build",
            ".#options",
            "--impure",
            "--print-out-paths",
            "--no-link",
        ])
        .args(["--override-input", "fc-nixos", fc_nixos_url])
        .output()
        .unwrap();

    if !json_file_cmd.status.success() {
        let stderr = String::from_utf8(json_file_cmd.stderr).expect("valid utf-8 in stderr");
        println!(
            "failed to build options for {fc_nixos_url}\nstderr: {}",
            stderr
        );
        return None;
    }

    let json_file = std::str::from_utf8(&json_file_cmd.stdout)
        .expect("valid utf-8")
        .strip_suffix('\n')
        .unwrap();

    // TODO logging / tracing
    println!("[fc-search] reading json from file {json_file:#?}");

    let contents = std::fs::read_to_string(Path::new(&json_file)).unwrap();
    serde_json::from_str(&contents).ok()
}
