pub mod nix;
pub mod search;

use nix::NixosOption;

use crate::search::{create_index, write_entries};
use std::path::Path;

use itertools::Itertools;
use reqwest::header::HeaderMap;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Display;
use tracing::{debug, error, info, warn};
use url::Url;

use self::nix::Expression;

#[derive(Debug, Serialize, Deserialize)]
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

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
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
        Html(markdown::to_html(&self))
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

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum FlakeRev {
    Specific(String),
    Latest,
    FallbackToCached,
}

#[derive(Debug, Serialize, Deserialize)]
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
    #[tracing::instrument]
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
        format!("github:{}/{}/{}", self.owner, self.name, self.branch)
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

#[tracing::instrument]
pub fn load_options(
    branch_path: &Path,
    flake: &Flake,
) -> anyhow::Result<HashMap<String, NaiveNixosOption>> {
    anyhow::ensure!(
        branch_path.exists(),
        "failed to load branch for channel searcher. path {} does not exist",
        branch_path.display()
    );

    if flake.rev != FlakeRev::FallbackToCached {
        let saved_flake: Flake = serde_json::from_str(&std::fs::read_to_string(
            branch_path.join("flake_info.json"),
        )?)?;

        // rebuild options if the revision is not cached or the cached rev is "latest"
        if saved_flake.rev == FlakeRev::Latest || saved_flake.rev != flake.rev {
            warn!(
                "saved flake rev != requested flake ref: {:?} != {:?}",
                saved_flake.rev, flake.rev
            );
            info!("channel info is outdated, need to rebuild");
            anyhow::bail!("rebuild the options");
        }
        info!("loading options from cache");
    } else {
        info!("falling back to cached options");
    }

    let naive_options_raw = std::fs::read_to_string(branch_path.join("options.json"))?;
    Ok(serde_json::from_str(&naive_options_raw)?)
}

#[tracing::instrument]
pub fn build_new_index(
    branch_path: &Path,
    flake: &Flake,
) -> anyhow::Result<HashMap<String, NaiveNixosOption>> {
    // generate tantivy index in a separate dir
    let index_path = branch_path.join("tantivy");

    info!("rebuilding options + index");

    std::fs::create_dir_all(index_path.clone()).expect("failed to create index path in state dir");
    let options = nix::build_options_for_fcio_branch(flake)?;

    // generate the tantivy index
    create_index(&index_path)?;
    write_entries(&index_path, &options)?;

    let naive_options = option_to_naive(&options);

    // cache the generated naive nixos options
    std::fs::write(
        branch_path.join("options.json"),
        serde_json::to_string(&naive_options).expect("failed to serialize naive options"),
    )
    .expect("failed to save naive options");

    // cache the current branch + revision
    std::fs::write(
        branch_path.join("flake_info.json"),
        serde_json::to_string(&flake).expect("failed to serialize flake info"),
    )
    .expect("failed to save flake info");

    info!("successfully rebuilt options + index");

    Ok(naive_options)
}
