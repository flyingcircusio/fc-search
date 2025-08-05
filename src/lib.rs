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
use tracing::{debug, info};
use url::Url;

use self::nix::Expression;

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
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

#[derive(Debug, Serialize, Deserialize, Clone)]
struct JobsetInput {
    #[serde(rename = "type")]
    input_type: String,

    #[serde(default)]
    uri: Option<String>,

    #[serde(default)]
    revision: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChannelEvalOverview {
    evals: Vec<ChannelEval>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChannelEval {
    jobsetevalinputs: HashMap<String, JobsetInput>,
    id: i64,
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

impl Flake {
    pub fn new(
        owner: impl ToString,
        name: impl ToString,
        branch: impl ToString,
        rev: FlakeRev,
    ) -> Self {
        Self {
            owner: owner.to_string(),
            name: name.to_string(),
            branch: branch.to_string(),
            rev,
        }
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

    pub async fn fetch_latest_rev(
        branch: impl ToString + std::fmt::Display,
    ) -> anyhow::Result<FlakeRev> {
        let mut headers = HeaderMap::new();
        headers.insert("Accept", "application/json".parse()?);
        let client = Client::builder().default_headers(headers).build()?;

        let evals = client
            .get(format!(
                "{HYDRA_BASE_URL}/jobset/flyingcircus/{}/evals",
                branch
            ))
            .send()
            .await?
            .text()
            .await?;

        let evals = {
            let tmp: ChannelEvalOverview = serde_json::from_str(&evals)?;
            let mut evals = tmp.evals;
            evals.sort_unstable_by_key(|x| x.id);
            evals.reverse();
            evals
        };

        evals
            .first()
            .ok_or(anyhow::anyhow!("not enough evaluations for that job"))?
            .jobsetevalinputs
            .get("fc")
            .map(|input| {
                FlakeRev::Specific(
                    input
                        .clone()
                        .revision
                        .expect("expected every fc-nixos input to provide a commit revision"),
                )
            })
            .ok_or(anyhow::anyhow!("no input called fc in response from hydra"))
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

    let mut flakes: Vec<Flake> = Vec::new();

    for jobset in jobsets {
        debug!("fetching latest rev for '{}'", jobset);
        if let Ok(rev) = Flake::fetch_latest_rev(jobset).await {
            flakes.push(Flake::new("flyingcircusio", "fc-nixos", jobset, rev));
        }
        // don't spam the hydra api
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
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
        self.map_err(|e| debug!("{}: {e}", context)).ok()
    }
}
