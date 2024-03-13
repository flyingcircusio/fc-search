use rust_embed::RustEmbed;
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::time::interval_at;

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
use tracing::{debug, info};

use fc_search::{
    get_fcio_flake_uris, nix::NixPackage, search::ChannelSearcher, Flake, NaiveNixosOption,
};

use serde::Deserialize;

#[derive(Clone)]
struct AppState {
    // Arc to prevent clones for every request, just need read access in the search handler
    channels: Arc<HashMap<String, Weak<Mutex<ChannelSearcher>>>>,
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
    fn active_branches(&self) -> Vec<&String> {
        self.channels
            .iter()
            .filter(|(_, searcher)| {
                searcher
                    .upgrade()
                    .and_then(|s| s.lock().map(|s| s.active()).ok())
                    .unwrap_or(false)
            })
            .map(|(name, _)| name)
            .sorted()
            .collect_vec()
    }

    fn in_dir(state_dir: &Path, branches: Vec<Flake>, start_timers: bool) -> anyhow::Result<Self> {
        debug!("initializing app state");

        if !state_dir.exists() {
            std::fs::create_dir_all(state_dir)?;
        }

        let mut channels = HashMap::new();
        for (i, flake) in branches.iter().enumerate() {
            let branchname = flake.branch.clone();
            let branch_path = state_dir.join(branchname.clone());

            debug!("starting searcher for branch {}", &branchname);
            let searcher = ChannelSearcher::new(&branch_path, flake);

            // attempt not to (re)build multiple channels at the same time by spreading them 5
            // minutes apart
            let weak = if start_timers {
                let freq = Duration::from_hours(5);
                let start_time = tokio::time::Instant::now() + Duration::from_mins(i as u64 * 5);
                let interval = interval_at(start_time, freq);
                searcher.start_timer(interval)
            } else {
                let start_time = if !searcher.active() {
                    tokio::time::Instant::now()
                } else {
                    tokio::time::Instant::now() + Duration::from_days(100_000)
                };
                let freq = Duration::from_days(100_000);
                let interval = interval_at(start_time, freq);
                searcher.start_timer(interval)
            };
            channels.insert(branchname, weak);
        }

        Ok(Self {
            channels: Arc::new(channels),
        })
    }
}

pub async fn run(port: u16, state_dir: &Path, test: bool) -> anyhow::Result<()> {
    let state = {
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

        let branches = if test {
            default_branches()
        } else {
            get_fcio_flake_uris()
                .await
                .unwrap_or_else(|_| default_branches())
        };

        // in release mode try to load the cached index from disk
        AppState::in_dir(state_dir, branches, !test)?
    };

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));

    let router = Router::new()
        .route("/", get(index_handler))
        .route(
            "/search",
            get(|| async { Redirect::permanent("/search/options").into_response() }),
        )
        .route("/search/options", get(search_options_handler))
        .route("/search/packages", get(search_packages_handler))
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

fn search_with_channel<F, V>(state: &AppState, channel: &str, f: F) -> Vec<V>
where
    F: FnOnce(&ChannelSearcher) -> Vec<&V>,
    V: Clone,
{
    state
        .channels
        .get(channel)
        .and_then(|c| {
            let channel = c.upgrade()?;
            channel
                .lock()
                .map(|c| f(&c).into_iter().cloned().collect_vec())
                .ok()
        })
        .unwrap_or_default()
}

#[tracing::instrument(skip(state, headers))]
async fn search_options_handler<'a>(
    State(state): State<AppState>,
    headers: HeaderMap,
    form: axum::extract::Form<SearchForm>,
) -> impl IntoResponse {
    let search_results = if !form.q.is_empty() {
        search_with_channel(&state, &form.channel, |c| c.search_options(&form.q))
    } else {
        Vec::new()
    };

    if headers.contains_key("HX-Request") {
        let template = OptionItemTemplate {
            results: search_results,
        };
        return HtmlTemplate(template).into_response();
    }

    HtmlTemplate(OptionsIndexTemplate {
        branches: state.active_branches(),
        results: search_results,
        search_value: &form.q,
    })
    .into_response()
}

#[tracing::instrument(skip(state))]
async fn search_packages_handler<'a>(
    State(state): State<AppState>,
    headers: HeaderMap,
    form: axum::extract::Form<SearchForm>,
) -> impl IntoResponse {
    let search_results = if !form.q.is_empty() {
        search_with_channel(&state, &form.channel, |c| c.search_packages(&form.q))
    } else {
        Vec::new()
    };

    if headers.contains_key("HX-Request") {
        let template = PackageItemTemplate {
            results: search_results,
        };
        return HtmlTemplate(template).into_response();
    }

    HtmlTemplate(PackagesIndexTemplate {
        branches: state.active_branches(),
        results: search_results,
        search_value: &form.q,
    })
    .into_response()
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
#[template(path = "options_index.html")]
struct OptionsIndexTemplate<'a> {
    branches: Vec<&'a String>,
    results: Vec<NaiveNixosOption>,
    search_value: &'a str,
}

#[derive(Template)]
#[template(path = "packages_index.html")]
struct PackagesIndexTemplate<'a> {
    branches: Vec<&'a String>,
    results: Vec<NixPackage>,
    search_value: &'a str,
}

#[derive(Template)]
#[template(path = "option_item.html")]
struct OptionItemTemplate {
    results: Vec<NaiveNixosOption>,
}

#[derive(Template)]
#[template(path = "package_item.html")]
struct PackageItemTemplate {
    results: Vec<NixPackage>,
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
