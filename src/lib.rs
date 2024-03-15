#![feature(duration_constructors)]

pub mod nix;
pub mod search;

use nix::NixosOption;

use itertools::Itertools;
use reqwest::header::HeaderMap;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Display;
use tracing::{debug, error, info, warn};
use url::Url;

use self::nix::Expression;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NaiveNixosOption {
    pub name: String,
    pub declarations: Vec<Html>,
    pub description: Html,
    pub default: Html,
    pub example: Html,
    pub option_type: String,
    pub read_only: bool,
}

pub trait NixHtml {
    fn as_html(&self) -> Html;
}

impl<T: NixHtml> NixHtml for Option<T> {
    fn as_html(&self) -> Html {
        match self {
            Some(s) => s.as_html(),
            None => Html("".to_string()),
        }
    }
}

#[derive(Debug, Default, PartialEq, Serialize, Deserialize, Clone)]
pub struct Html(pub String);

impl Display for Html {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone)]
pub enum Declaration {
    Naive(String),
    Processed(Url),
}

impl NixHtml for Declaration {
    fn as_html(&self) -> Html {
        match self {
            Declaration::Naive(s) => Html(format!("<i>{}</i>", s)),
            Declaration::Processed(url) => Html(format!(
                "<a class=\"text-blue-900 hover:underline\" href=\"{}\">{}</a>",
                url, url
            )),
        }
    }
}

impl NixHtml for Expression {
    fn as_html(&self) -> Html {
        match self.option_type {
            nix::ExpressionType::LiteralExpression => Html(self.text.clone()),
            nix::ExpressionType::LiteralMd => Html(markdown::to_html(&self.text)),
        }
    }
}

impl NixHtml for String {
    fn as_html(&self) -> Html {
        Html(markdown::to_html(self))
    }
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

#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
pub enum FlakeRev {
    Specific(String),
    Latest,
    FallbackToCached,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Flake {
    pub owner: String,
    pub name: String,
    pub branch: String,
    pub rev: FlakeRev,
}

#[derive(Deserialize)]
struct GithubCommitInfo {
    sha: String,
}

#[derive(Deserialize)]
struct GithubBranchInfo {
    name: String,
    commit: GithubCommitInfo,
}

impl Flake {
    pub async fn new(owner: &str, name: &str, branch: &str) -> anyhow::Result<Self> {
        let rev = Self::get_latest_rev(owner, name, branch)
            .await
            .unwrap_or_else(|_| {
                warn!("failed to fetch latest rev. Trying to fall back to cached options");
                FlakeRev::FallbackToCached
            });
        Ok(Self {
            owner: owner.to_string(),
            name: name.to_string(),
            branch: branch.to_string(),
            rev,
        })
    }

    pub fn flake_uri(&self) -> String {
        match &self.rev {
            FlakeRev::Specific(r) => format!("github:{}/{}?rev={r}", self.owner, self.name),
            _ => format!("github:{}/{}/{}", self.owner, self.name, self.branch),
        }
    }

    pub fn github_base_url(&self) -> String {
        format!(
            "https://github.com/{}/{}/blob/{}",
            self.owner, self.name, self.branch
        )
    }

    pub async fn get_latest_rev(owner: &str, name: &str, branch: &str) -> anyhow::Result<FlakeRev> {
        let client = Client::builder()
            .build()
            .expect("could not build request client");

        let url = format!(
            "https://api.github.com/repos/{}/{}/branches/{}",
            owner, name, branch
        );
        let response_text = client
            .get(url)
            .header("Accept", "application/json")
            .header("User-Agent", "fc-search")
            .send()
            .await
            .expect("unable to fetch repository info")
            .text()
            .await
            .expect("expected to get text for api response from github");

        let ghinfo: GithubBranchInfo = match serde_json::from_str(&response_text) {
            Ok(s) => s,
            Err(e) => {
                error!(
                    "did not get valid json from the github api {} {}",
                    response_text, e
                );
                anyhow::bail!("invalid json");
            }
        };

        assert_eq!(
            ghinfo.name, branch,
            "got an api response for a different branch"
        );
        debug!("latest rev is {}", ghinfo.commit.sha);

        Ok(FlakeRev::Specific(ghinfo.commit.sha))
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

    let mut branches: Vec<String> = Vec::new();

    for jobset_id in jobsets {
        let jobset = client
            .get(format!("{HYDRA_BASE_URL}/jobset/{project_id}/{jobset_id}"))
            .send()
            .await?
            .text()
            .await?;

        let jobset: Jobset = serde_json::from_str(&jobset).unwrap();

        match jobset.inputs.get("fc") {
            Some(input) => {
                let (repo, branch) = input
                    .value
                    .split_once(' ')
                    .expect("value does not have scheme `uri branch`");

                // TODO error handling?
                assert_eq!(repo, "https://github.com/flyingcircusio/fc-nixos");
                branches.push(branch.to_string());
            }
            _ => {
                warn!("jobset {:?} has no input fc", jobset);
            }
        }
    }

    // index newest branches first to circumvent rate limits when indexing the more important newer branches
    branches.sort();
    branches.reverse();

    // only keep the newest 9 branches => 3 channels (dev, staging + prod each)
    branches.truncate(3 * 3);

    let mut flakes = Vec::new();
    for branch in branches.into_iter() {
        match Flake::new("flyingcircusio", "fc-nixos", &branch).await {
            Ok(s) => flakes.push(s),
            Err(e) => error!("error fetching information about branch {}: {e:?}", branch),
        };
    }

    info!(
        "fetched branches {:?} from hydra",
        flakes.iter().map(|f| f.branch.clone()).collect_vec()
    );

    Ok(flakes)
}

pub fn option_to_naive(
    options: &HashMap<String, NixosOption>,
) -> HashMap<String, NaiveNixosOption> {
    let mut out = HashMap::new();
    for (name, option) in options.iter() {
        let declarations = option
            .declarations
            .iter()
            .map(|decl| match Url::parse(decl) {
                Ok(mut url) => {
                    if !url.path().ends_with(".nix") {
                        url = url
                            .join("default.nix")
                            .expect("could not join url with simple string");
                    }
                    Declaration::Processed(url).as_html()
                }
                Err(_) => Declaration::Naive(decl.to_string()).as_html(),
            })
            .collect_vec();

        out.insert(
            name.clone(),
            NaiveNixosOption {
                name: name.to_string(),
                declarations,
                description: option
                    .description
                    .clone()
                    .map(|e| e.as_html())
                    .unwrap_or_default(),
                default: option
                    .default
                    .clone()
                    .map(|e| e.as_html())
                    .unwrap_or_default(),
                example: option
                    .example
                    .clone()
                    .map(|e| e.as_html())
                    .unwrap_or_default(),
                option_type: option.option_type.clone(),
                read_only: option.read_only,
            },
        );
    }
    out
}

pub trait LogError<T> {
    fn log_to_option(self, context: &str) -> Option<T>;
}

impl<T, E: Display> LogError<T> for Result<T, E> {
    fn log_to_option(self, context: &str) -> Option<T> {
        self.map_err(|e| error!("{}: {e}", context)).ok()
    }
}
