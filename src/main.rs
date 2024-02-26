use std::{collections::HashMap, sync::Arc};

use anyhow::Context;
use askama::Template;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use itertools::Itertools;
use tantivy::{collector::TopDocs, query::QueryParser, schema::Schema, Index, Searcher};
use tower_http::services::ServeDir;
use tracing::{debug, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use fc_search::{
    build_options_for_input, get_fcio_flake_uris,
    search::{create_index, write_entries},
    Flake,
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
    fn test() -> Self {
        let mut channels = HashMap::new();

        let index_path = TempDir::new().unwrap().into_path();
        let options: HashMap<String, fc_search::NixosOption> =
            serde_json::from_str(&std::fs::read_to_string("out.json").unwrap()).unwrap();

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
        let name = schema.get_field("name").expect("the field should exist");
        let description = schema
            .get_field("description")
            .expect("the field should exist");

        let default = schema.get_field("default").expect("the field should exist");

        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::OnCommit)
            .try_into()
            .unwrap();

        let searcher = reader.searcher();
        let mut query_parser = QueryParser::for_index(&index, vec![name, description, default]);
        query_parser.set_field_fuzzy(name, true, 2, false);
        query_parser.set_field_boost(name, 5.0);

        channels.insert(
            "flake2.0".to_string(),
            ChannelSearcher {
                options: naive_options,
                query_parser,
                searcher,
                schema,
            },
        );

        Self {
            channels: Arc::new(channels),
        }
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

        let mut channels = HashMap::new();

        for uri in uris {
            let index_path = TempDir::new().unwrap().into_path();
            let Some(options) = build_options_for_input(&uri.flake_uri()) else {
                println!(
                    "failed to build options for branch {}, skipping",
                    uri.branch
                );
                continue;
            };

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
            let name = schema.get_field("name").expect("the field should exist");
            let description = schema
                .get_field("description")
                .expect("the field should exist");

            let default = schema.get_field("default").expect("the field should exist");

            let reader = index
                .reader_builder()
                .reload_policy(tantivy::ReloadPolicy::OnCommit)
                .try_into()
                .unwrap();

            let searcher = reader.searcher();
            let mut query_parser = QueryParser::for_index(&index, vec![name, description, default]);
            query_parser.set_field_fuzzy(name, true, 2, false);
            query_parser.set_field_boost(name, 5.0);

            channels.insert(
                uri.branch,
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

    let state = AppState::test();
    //let state = AppState::trivial().await;

    let assets_path = std::env::current_dir().unwrap();
    let port = 8000_u16;
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));

    let router = Router::new()
        .route("/", get(index_handler))
        .route("/search", get(search_handler))
        .with_state(state)
        .nest_service(
            "/assets",
            ServeDir::new(format!("{}/assets", assets_path.to_str().unwrap())),
        );

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

    let query = channel.query_parser.parse_query(&form.q).unwrap();
    let top_docs = channel
        .searcher
        .search(&query, &TopDocs::with_limit(10))
        .unwrap();

    let name = channel
        .schema
        .get_field("name")
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
