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
    path::Path,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::time::interval;
use tracing::{debug, error, info};

#[derive(Clone)]
struct AppState {
    // Arc to prevent clones for every request, just need read access in the search handler
    channels: Arc<HashMap<String, RwLock<ChannelSearcher>>>,
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
    async fn active_branches(&self) -> Vec<&String> {
        let mut channels = Vec::new();
        for channel in self.channels.iter() {
            if channel.1.read().unwrap().active() {
                channels.push(channel.0)
            }
        }
        channels
    }

    fn in_dir(state_dir: &Path, branches: Vec<Flake>) -> anyhow::Result<Self> {
        debug!("initializing app state");

        if !state_dir.exists() {
            std::fs::create_dir_all(state_dir)?;
        }

        let mut channels = HashMap::new();
        for mut flake in branches {
            let branchname = flake.branch.clone();
            let branch_path = state_dir.join(branchname.clone());

            debug!("starting searcher for branch {}", &branchname);

            let flake_info_path = branch_path.join("flake_info.json");
            if matches!(flake.rev, fc_search::FlakeRev::FallbackToCached)
                && flake_info_path.exists()
            {
                if let Ok(saved_flake) = serde_json::from_str::<Flake>(
                    &std::fs::read_to_string(flake_info_path)
                        .expect("flake_info.json exists but could not be read"),
                ) {
                    info!("loaded flake from file cache: {:#?}", saved_flake);
                    flake = saved_flake;
                };
            }

            let searcher = RwLock::new(ChannelSearcher::new(&branch_path, &flake));
            channels.insert(branchname, searcher);
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
    let updater_handle = if !test {
        // run update loop in the background
        tokio::spawn(async move {
            let freq = Duration::from_hours(5);
            let mut interval = interval(freq);
            loop {
                interval.tick().await;
                for (branch, searcher) in updater_channels.iter() {
                    update_channel(branch, searcher).await;
                }
            }
        })
    } else {
        // just update once, no need for a timed update
        tokio::spawn(async move {
            for (branch, searcher) in updater_channels.iter() {
                update_channel(branch, searcher).await;
            }
        })
    };

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
        let channel = form.channel.as_ref().unwrap_or_else(|| {
            state
                .channels
                .keys()
                .sorted()
                .next()
                .context("no channels active")
                .unwrap()
        });
        match state.channels.get(channel) {
            Some(c) => c
                .read()
                .unwrap()
                .search_options(&form.q, form.n_items, form.page),
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
        branches: state.active_branches().await,
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
        let channel = form.channel.as_ref().unwrap_or_else(|| {
            state
                .channels
                .keys()
                .sorted()
                .next()
                .context("no channels active")
                .unwrap()
        });
        match state.channels.get(channel) {
            Some(c) => c
                .read()
                .unwrap()
                .search_packages(&form.q, form.n_items, form.page),
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
        branches: state.active_branches().await,
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
    branches: Vec<&'a String>,
    results: Vec<NaiveNixosOption>,
    search_value: &'a str,
    page: u8,
}

#[derive(Template)]
#[template(path = "packages_index.html")]
struct PackagesIndexTemplate<'a> {
    branches: Vec<&'a String>,
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
