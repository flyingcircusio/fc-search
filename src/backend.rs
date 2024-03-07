use rust_embed::RustEmbed;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context;
use askama::Template;
use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use itertools::Itertools;
use tantivy::{
    collector::TopDocs, query::QueryParser, schema::Schema, Index, Searcher, TantivyError,
};
use tracing::{debug, error, info, warn};

use fc_search::{
    build_new_index, get_fcio_flake_uris, load_options,
    nix::NixosOption,
    option_to_naive,
    search::{create_index, write_entries},
    update_index, Flake, NaiveNixosOption,
};

use tokio::time::{interval_at, Duration, Instant};

use serde::Deserialize;

#[derive(Clone)]
struct AppState {
    // Arc to prevent clones for every request, just need read access in the search handler
    channels: Arc<HashMap<String, ChannelSearcher>>,
}

struct ChannelSearcherInner {
    options: HashMap<String, NaiveNixosOption>,
    query_parser: QueryParser,
    searcher: Searcher,
    schema: Schema,
}

struct ChannelSearcher {
    inner: Option<ChannelSearcherInner>,

    // members required for updating the options at runtime
    branch_path: PathBuf,
    flake: Option<Flake>,
}

impl ChannelSearcherInner {
    fn with_options(
        branch_path: &Path,
        options: HashMap<String, NixosOption>,
    ) -> anyhow::Result<Self> {
        let naive_options = option_to_naive(&options);

        let index_path = branch_path.join("tantivy");

        std::fs::create_dir_all(index_path.clone()).expect("could not create the index path");
        match create_index(&index_path) {
            Ok(_) => {
                write_entries(&index_path, &options)?;
            }
            Err(TantivyError::IndexAlreadyExists) => {
                debug!("tantivy index already exists, continuing");
            }
            Err(e) => return Err(e.into()),
        }
        Self::with_naive_options(branch_path, naive_options)
    }

    fn with_naive_options(
        branch_path: &Path,
        naive_options: HashMap<String, NaiveNixosOption>,
    ) -> anyhow::Result<Self> {
        let index_path = branch_path.join("tantivy");

        let index = Index::open_in_dir(index_path).expect("could not open the index path");
        let schema = index.schema();
        let description = schema
            .get_field("description")
            .expect("the field description should exist");
        let name = schema
            .get_field("name")
            .expect("the field name should exist");
        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::OnCommit)
            .try_into()?;

        let searcher = reader.searcher();
        let mut query_parser = QueryParser::for_index(&index, vec![name, description]);
        query_parser.set_field_fuzzy(name, true, 1, false);
        query_parser.set_field_boost(name, 3.0);

        Ok(Self {
            options: naive_options,
            query_parser,
            searcher,
            schema,
        })
    }
}

impl ChannelSearcher {
    pub fn start_timer(&self) {
        if let Some(flake) = &self.flake {
            info!("[{}] started timer", flake.branch);

            let mut flake = flake.clone();
            let branch_path = self.branch_path.clone();
            tokio::spawn(async move {
                let start = Instant::now() + Duration::from_hours(2);
                let mut interval = interval_at(start, Duration::from_hours(2));

                loop {
                    match Flake::get_latest_rev(&flake.owner, &flake.name, &flake.branch).await {
                        Ok(new_flake_rev) => {
                            flake.rev = new_flake_rev;
                            match update_index(&branch_path, &flake) {
                                Ok(_) => info!("[{}] successfully updated branch", flake.branch),
                                Err(e) => error!("[{}] error updating branch: {}", flake.branch, e),
                            };
                        }
                        Err(e) => {
                            error!("[{}] error getting the newest commit: {}", flake.branch, e)
                        }
                    };

                    let period = interval.period();
                    info!(
                        "[{}] next tick in {:?}h {:?}m",
                        flake.branch,
                        period.as_secs() / (60 * 60),
                        (period.as_secs() / 60) % 60
                    );
                    interval.tick().await;
                }
            });
        }
    }

    pub fn from_flake(branch_path: &Path, flake: &Flake) -> anyhow::Result<Self> {
        // in case of failure or when the cached options are different from the requested ones
        // try to regenerate the options
        let inner = match load_options(branch_path, flake) {
            Ok(opts) => ChannelSearcherInner::with_naive_options(branch_path, opts).ok(),
            Err(e) => {
                // TODO: cache old options and restore if building the new ones fails?
                if branch_path.exists() {
                    std::fs::remove_dir_all(branch_path).expect("failed to remove old index path");
                }
                std::fs::create_dir_all(branch_path)
                    .expect("failed to create index path in state dir");
                info!("failed to load cached options ({:?}), rebuilding", e);

                build_new_index(branch_path, flake)
                    .map(|opts| ChannelSearcherInner::with_naive_options(branch_path, opts).ok())
                    .unwrap_or_default()
            }
        };

        Ok(Self {
            inner,
            branch_path: branch_path.to_path_buf(),
            flake: Some(flake.clone()),
        })
    }
}

fn default_channel() -> String {
    "fc-23.11-dev".to_string()
}

#[derive(Deserialize, Debug)]
struct SearchForm {
    #[serde(default)]
    q: String,
    #[serde(default = "default_channel")]
    channel: String,
}

impl AppState {
    fn from_dir(state_dir: &Path, branches: Vec<Flake>) -> anyhow::Result<Self> {
        anyhow::ensure!(state_dir.exists(), "state dir does not exist");

        let mut channels = HashMap::new();
        for flake in branches {
            let branchname = flake.branch.clone();
            let branch_path = state_dir.join(branchname.clone());

            let Ok(searcher) = ChannelSearcher::from_flake(&branch_path, &flake) else {
                warn!("failed to build channel {}", flake.branch);
                continue;
            };

            searcher.start_timer();

            channels.insert(branchname, searcher);
        }

        Ok(Self {
            channels: Arc::new(channels),
        })
    }

    fn new_with_options(
        state_dir: &Path,
        channel_options: HashMap<String, HashMap<String, NixosOption>>,
    ) -> anyhow::Result<Self> {
        assert!(state_dir.exists(), "state dir does not exist");

        let mut channels = HashMap::new();
        for (branch_name, options) in channel_options {
            let branch_path = state_dir.join(branch_name.clone());
            let searcher = ChannelSearcher {
                inner: ChannelSearcherInner::with_options(&branch_path, options).ok(),
                branch_path,
                flake: None,
            };
            channels.insert(branch_name, searcher);
        }

        Ok(Self {
            channels: Arc::new(channels),
        })
    }
}

fn test_options() -> HashMap<String, HashMap<String, NixosOption>> {
    let branch_name = "fc-23.11-dev";
    let options: HashMap<String, fc_search::nix::NixosOption> = serde_json::from_str(
        &std::fs::read_to_string("out.json")
            .expect("unable to read 'out.json', please generate it first"),
    )
    .expect("unable to parse 'out.json'");

    let mut channels = HashMap::new();
    channels.insert(branch_name.to_string(), options);
    channels
}

pub async fn run(port: u16, fetch_all_channels: bool, state_dir: &Path) -> anyhow::Result<()> {
    info!("initializing router...");

    let state = {
        if cfg!(debug_assertions) {
            debug!("running in debug mode with pre-generated json");
            let test_options = test_options();
            AppState::new_with_options(state_dir, test_options)?
        } else {
            let default_branches = || {
                vec![Flake {
                    owner: "flyingcircusio".to_string(),
                    name: "fc-nixos".to_string(),
                    branch: "fc-23.11-dev".to_string(),
                    rev: fc_search::FlakeRev::Specific(
                        "62dd02d70222ffc1f3841fb8308952bedb2bfe96".to_string(),
                    ),
                }]
            };

            // fetch branches from hydra
            let branches = if fetch_all_channels {
                get_fcio_flake_uris()
                    .await
                    .unwrap_or_else(|_| default_branches())
            } else {
                default_branches()
            };

            // in release mode try to load the cached index from disk
            AppState::from_dir(state_dir, branches)?
        }
    };

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));

    let router = Router::new()
        .route("/", get(index_handler))
        .route("/search", get(search_handler))
        .route("/assets/*file", get(static_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(
        "router initialized, now listening on http://{}",
        listener.local_addr().unwrap()
    );

    axum::serve(listener, router.into_make_service())
        .await
        .context("error while starting server")?;
    Ok(())
}

async fn index_handler() -> impl IntoResponse {
    Redirect::permanent("/search").into_response()
}

#[tracing::instrument(skip(state))]
async fn search_handler<'a>(
    State(state): State<AppState>,
    headers: HeaderMap,
    form: axum::extract::Form<SearchForm>,
) -> impl IntoResponse {
    if headers.contains_key("HX-Request") {
        let results = get_results(&form, &state);
        let template = ItemTemplate { results };
        HtmlTemplate(template).into_response()
    } else {
        let branches = state
            .channels
            .iter()
            .filter_map(|(name, searcher)| searcher.inner.is_some().then(|| name))
            .sorted()
            .collect_vec();
        let results = get_results(&form, &state);
        HtmlTemplate(IndexTemplate {
            branches,
            results,
            search_value: &form.q,
        })
        .into_response()
    }
}

async fn static_handler(uri: Uri) -> impl IntoResponse {
    let mut path = uri.path().trim_start_matches('/').to_string();

    if path.starts_with("assets/") {
        path = path.replace("assets/", "");
    }

    StaticFile(path)
}

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Asset;

pub struct StaticFile<T>(pub T);

impl<T> IntoResponse for StaticFile<T>
where
    T: Into<String>,
{
    fn into_response(self) -> Response {
        let path = self.0.into();

        match Asset::get(path.as_str()) {
            Some(content) => {
                let mime = mime_guess::from_path(path).first_or_octet_stream();
                ([(header::CONTENT_TYPE, mime.as_ref())], content.data).into_response()
            }
            None => (StatusCode::NOT_FOUND, "404 Not Found").into_response(),
        }
    }
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate<'a> {
    branches: Vec<&'a String>,
    results: Vec<&'a NaiveNixosOption>,
    search_value: &'a str,
}

#[derive(Template)]
#[template(path = "item.html")]
struct ItemTemplate<'a> {
    results: Vec<&'a NaiveNixosOption>,
}

struct HtmlTemplate<T>(T);

impl<T> IntoResponse for HtmlTemplate<T>
where
    T: Template,
{
    fn into_response(self) -> Response {
        match self.0.render() {
            Ok(html) => axum::response::Html(html).into_response(),
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to render template. Error: {}", err),
            )
                .into_response(),
        }
    }
}

fn get_results<'a>(form: &SearchForm, state: &'a AppState) -> Vec<&'a NaiveNixosOption> {
    // return nothing if channel not found
    let Some(channel) = state.channels.get(&form.channel) else {
        return Vec::new();
    };

    let Some(ref inner) = channel.inner else {
        debug!(
            "attempted to query from channel that has not yet been indexed successfully: {}",
            channel
                .flake
                .as_ref()
                .map(|f| f.branch.clone())
                .unwrap_or_default()
        );
        // TODO: display in ui that the channel is not available?
        return Vec::new();
    };

    let query = inner.query_parser.parse_query_lenient(&form.q).0;

    let top_docs = inner
        .searcher
        .search(&query, &TopDocs::with_limit(30))
        .unwrap();

    let name = inner
        .schema
        .get_field("original_name")
        .expect("schema has field name");

    let results: Vec<&NaiveNixosOption> = top_docs
        .into_iter()
        .map(|(_score, doc_address)| {
            let retrieved = inner.searcher.doc(doc_address).unwrap();
            retrieved
                .get_first(name)
                .expect("result has a value for name")
                .as_text()
                .expect("value is text")
                .to_string()
        })
        .map(|name| {
            inner
                .options
                .get(&name)
                .expect("found option exists in hashmap")
        })
        .collect();

    results
}
