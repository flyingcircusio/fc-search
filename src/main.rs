use rust_embed::RustEmbed;
use std::{collections::HashMap, sync::Arc};

use anyhow::Context;
use askama::Template;
use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode, Uri},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use itertools::Itertools;
use tantivy::{collector::TopDocs, query::QueryParser, schema::Schema, Index, Searcher};
use tracing::{debug, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use fc_search::{
    build_options_for_input, get_fcio_flake_uris,
    search::{create_index, write_entries},
    Flake, NixosOption,
};

use serde::Deserialize;
use tempfile::TempDir;

#[derive(Debug)]
struct NaiveNixosOption {
    name: String,
    declarations: Vec<String>,
    description: String,
    default: String,
}

#[derive(Clone)]
struct AppState {
    // TODO hashmap of hashmaps for all channels
    // arc to prevent clones here, just need read access in the search handler
    channels: Arc<HashMap<String, ChannelSearcher>>,
}

struct ChannelSearcher {
    options: HashMap<String, NaiveNixosOption>,
    query_parser: QueryParser,
    searcher: Searcher,
    schema: Schema,
}

// TODO adjust after testing
fn default_channel() -> String {
    "flake2.0".to_string()
}

#[derive(Deserialize, Debug)]
struct SearchForm {
    #[serde(default)]
    q: String,
    #[serde(default = "default_channel")]
    channel: String,
}

impl AppState {
    fn with_options(channel_options: HashMap<String, HashMap<String, NixosOption>>) -> Self {
        let mut channels = HashMap::new();

        for (branch_name, options) in channel_options {
            let index_path = TempDir::new().unwrap().into_path();
            create_index(&index_path).unwrap();
            write_entries(&index_path, &options).unwrap();

            let mut naive_options = HashMap::new();
            for (name, option) in options.into_iter() {
                naive_options.insert(
                    name.clone(),
                    NaiveNixosOption {
                        name,
                        description: option.description.clone().unwrap_or_default(),
                        declarations: option.declarations.clone(),
                        default: option.default.clone().map(|e| e.text).unwrap_or_default(),
                    },
                );
            }

            // ----------------------

            let index = Index::open_in_dir(&index_path).unwrap();
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
                .try_into()
                .unwrap();

            let searcher = reader.searcher();
            let mut query_parser = QueryParser::for_index(&index, vec![name, description]);
            query_parser.set_field_fuzzy(name, true, 1, false);
            query_parser.set_field_boost(name, 3.0);

            channels.insert(
                branch_name,
                ChannelSearcher {
                    options: naive_options,
                    query_parser,
                    searcher,
                    schema,
                },
            );
        }

        Self {
            channels: Arc::new(channels),
        }
    }

    fn test() -> Self {
        let index_path = TempDir::new().unwrap().into_path();
        let options: HashMap<String, fc_search::NixosOption> = serde_json::from_str(
            &std::fs::read_to_string("out.json")
                .expect("unable to read 'out.json', please generate it first"),
        )
        .expect("unable to parse 'out.json'");

        create_index(&index_path).unwrap();
        write_entries(&index_path, &options).unwrap();

        let mut channels = HashMap::new();
        channels.insert("flake2.0".to_string(), options);
        Self::with_options(channels)
    }

    // TODO error handling
    #[allow(dead_code)]
    async fn trivial() -> Self {
        //let uris = get_fcio_flake_uris().await.unwrap();
        let uris = vec![Flake {
            owner: "PhilTaken".to_string(),
            name: "fc-nixos".to_string(),
            branch: "flake2.0".to_string(),
        }];

        println!(
            "building options for branches: {:#?}",
            uris.iter().map(|u| u.branch.clone()).collect_vec()
        );

        let mut all_options = HashMap::new();

        for uri in uris {
            if let Some(options) = build_options_for_input(&uri.flake_uri()) {
                all_options.insert(uri.branch, options);
            } else {
                println!(
                    "failed to build options for branch {}, skipping",
                    uri.branch
                );
            }
        }

        Self::with_options(all_options)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "with_axum_htmx_askama=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("initializing router...");

    let state = if cfg!(debug_assertions) {
        AppState::test()
    } else {
        AppState::trivial().await
    };

    let port = 8000_u16;
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
async fn search_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    form: axum::extract::Form<SearchForm>,
) -> impl IntoResponse {
    debug!("item handler");

    if headers.contains_key("HX-Request") {
        let results = get_results(&form, &state);
        let template = ItemTemplate { results };
        HtmlTemplate(template).into_response()
    } else {
        let branches = state.channels.keys().sorted().cloned().collect_vec();
        let results = get_results(&form, &state);
        HtmlTemplate(IndexTemplate {
            branches,
            results,
            search_value: form.q.clone(),
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
    branches: Vec<String>,
    results: Vec<&'a NaiveNixosOption>,
    search_value: String,
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
            Ok(html) => Html(html).into_response(),
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

    let query = channel.query_parser.parse_query_lenient(&form.q).0;

    dbg!(&query);

    let top_docs = channel
        .searcher
        .search(&query, &TopDocs::with_limit(30))
        .unwrap();

    let name = channel
        .schema
        .get_field("original_name")
        .expect("schema has field name");

    let results: Vec<&NaiveNixosOption> = top_docs
        .into_iter()
        .map(|(_score, doc_address)| {
            let retrieved = channel.searcher.doc(doc_address).unwrap();
            retrieved
                .get_first(name)
                .expect("result has a value for name")
                .as_text()
                .expect("value is text")
                .to_string()
        })
        .map(|name| {
            channel
                .options
                .get(&name)
                .expect("found option exists in hashmap")
        })
        .collect();

    results
}
