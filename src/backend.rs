use anyhow::Context;
use askama::Template;
use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use fc_search::{
    get_fcio_flake_uris, nix::NixPackage, search::ChannelSearcher, Flake, NaiveNixosOption, NixHtml,
};
use itertools::Itertools;
use rust_embed::RustEmbed;
use serde::Deserialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::time::interval;
use tracing::{debug, error, info};

#[derive(Clone)]
struct AppState {
    // Arc to prevent clones for every request, just need read access in the search handler
    channels: Arc<RwLock<HashMap<String, ChannelSearcher>>>,
    state_dir: PathBuf,
}

const fn default_n_items() -> u8 {
    15
}

const fn default_page() -> u8 {
    1
}

#[derive(Deserialize, Debug)]
struct SearchForm {
    #[serde(default)]
    q: String,
    channel: Option<String>,
    #[serde(default = "default_n_items")]
    n_items: u8,
    #[serde(default = "default_page")]
    page: u8,
}

impl AppState {
    // TODO cache this between requests, only changes on rebuilds
    fn active_branches(&self) -> Vec<String> {
        self.channels
            .read()
            .unwrap()
            .iter()
            .filter_map(|channel| channel.1.active().then_some(channel.0))
            .sorted()
            .rev()
            .cloned()
            .collect_vec()
    }

    fn in_dir(state_dir: &Path, branches: Vec<Flake>) -> anyhow::Result<Self> {
        debug!("initializing app state");

        if !state_dir.exists() {
            std::fs::create_dir_all(state_dir)?;
        }

        let mut channels = HashMap::new();
        for flake in branches {
            let searcher = ChannelSearcher::in_statedir(state_dir, &flake);
            channels.insert(flake.branch, searcher.into());
        }

        let mut ret = Self {
            channels: Arc::new(RwLock::new(channels)),
            state_dir: state_dir.to_path_buf(),
        };
        Ok(ret)
    }
}

pub async fn run(port: u16, state_dir: &Path, test: bool) -> anyhow::Result<()> {
    let state = {
        let default_branches = || {
            vec![Flake {
                owner: "flyingcircusio".to_string(),
                name: "fc-nixos".to_string(),
                branch: "fc-23.11-dev".to_string(),
                rev: fc_search::FlakeRev::FallbackToCached,
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
        AppState::in_dir(state_dir, branches)?
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
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(
        "router initialized, now listening on http://{}",
        listener.local_addr().unwrap()
    );

    let updater_channels = state.channels.clone();

    // run update loop in the background
    let updater_handle = tokio::spawn(async move {
        let freq = Duration::from_hours(5);
        let mut interval = interval(freq);
        loop {
            interval.tick().await;
            if let Ok(upstream_flakes) = get_fcio_flake_uris().await {
                let channels: HashMap<String, RwLock<ChannelSearcher>> = updater_channels
                    .read()
                    .unwrap()
                    .iter()
                    .map(|(x, y)| (x.clone(), y.clone().into()))
                    .collect();

                // update existing channels
                for (branch, searcher) in &channels {
                    update_channel(branch, searcher).await;
                }

                // initialise possibly missing channels, they will be updated on the next run
                for flake in upstream_flakes {
                    // index new branches
                    if !channels.contains_key(&flake.branch) {
                        let searcher = ChannelSearcher::in_statedir(&state.state_dir, &flake);

                        updater_channels
                            .write()
                            .unwrap()
                            .insert(flake.branch, searcher.into());
                    }
                }
            }
        }
    });

    if let Err(e) = axum::serve(listener, router.into_make_service())
        .await
        .context("error while starting server")
    {
        let _ = updater_handle.abort();
        Err(e)
    } else {
        Ok(())
    }
}

async fn index_handler() -> impl IntoResponse {
    Redirect::permanent("/search").into_response()
}

async fn search_options_handler<'a>(
    State(state): State<AppState>,
    headers: HeaderMap,
    form: axum::extract::Form<SearchForm>,
) -> impl IntoResponse {
    if form.page == 0 {
        return axum::http::StatusCode::IM_A_TEAPOT.into_response();
    }

    let search_results = if !form.q.is_empty() {
        let channel = form.channel.clone().unwrap_or_else(|| {
            state
                .channels
                .read()
                .unwrap()
                .keys()
                .sorted()
                .find(|x| x.contains("prod"))
                .cloned()
                .context("no channels active")
                .unwrap()
        });

        match state.channels.read().unwrap().get(&channel) {
            Some(c) => c.search_options(&form.q, form.n_items, form.page),
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    if headers.contains_key("HX-Request") {
        let template = OptionItemTemplate {
            results: search_results,
            page: form.page,
        };
        return HtmlTemplate(template).into_response();
    }

    HtmlTemplate(OptionsIndexTemplate {
        branches: state.active_branches(),
        results: search_results,
        search_value: &form.q,
        page: form.page,
    })
    .into_response()
}

async fn search_packages_handler<'a>(
    State(state): State<AppState>,
    headers: HeaderMap,
    form: axum::extract::Form<SearchForm>,
) -> impl IntoResponse {
    if form.page == 0 {
        return axum::http::StatusCode::IM_A_TEAPOT.into_response();
    }

    let search_results = if !form.q.is_empty() {
        let channel = form.channel.clone().unwrap_or_else(|| {
            state
                .channels
                .read()
                .unwrap()
                .keys()
                .sorted()
                .find(|x| x.contains("prod"))
                .cloned()
                .context("no prod channels active")
                .unwrap()
        });
        match state.channels.read().unwrap().get(&channel) {
            Some(c) => c.search_packages(&form.q, form.n_items, form.page),
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    if headers.contains_key("HX-Request") {
        let template = PackageItemTemplate {
            page: form.page,
            results: search_results,
        };
        return HtmlTemplate(template).into_response();
    }

    HtmlTemplate(PackagesIndexTemplate {
        branches: state.active_branches(),
        results: search_results,
        search_value: &form.q,
        page: form.page,
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
    branches: Vec<String>,
    results: Vec<NaiveNixosOption>,
    search_value: &'a str,
    page: u8,
}

#[derive(Template)]
#[template(path = "packages_index.html")]
struct PackagesIndexTemplate<'a> {
    branches: Vec<String>,
    results: Vec<NixPackage>,
    search_value: &'a str,
    page: u8,
}

#[derive(Template)]
#[template(path = "option_item.html")]
struct OptionItemTemplate {
    results: Vec<NaiveNixosOption>,
    page: u8,
}

#[derive(Template)]
#[template(path = "package_item.html")]
struct PackageItemTemplate {
    results: Vec<NixPackage>,
    page: u8,
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

async fn update_channel(branch: &str, channel: &RwLock<ChannelSearcher>) {
    // obtain the current searcher
    let mut cs: ChannelSearcher = channel.read().unwrap().clone();

    // no lock on the channel searcher here, so we can update it
    // and replace the value on success while search is still running
    // in an error case the old status is retained and the error logged
    info!("starting update for branch {}", branch);
    match cs.update().await {
        Err(e) => error!("error updating branch {}: {e:?}", branch),
        Ok(()) => {
            // replace the old searcher with the updated one on success
            let mut old = channel.write().unwrap();
            *old = cs;
        }
    }
}
