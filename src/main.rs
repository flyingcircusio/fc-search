use std::{collections::HashMap, sync::Arc};

use anyhow::Context;
use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use tantivy::{collector::TopDocs, query::QueryParser, schema::Schema, Index, Searcher};
use tower_http::services::ServeDir;
use tracing::{debug, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use fc_search::{
    search::{create_index, write_entries},
    NixosOption,
};

use serde::Deserialize;
use tempfile::TempDir;

#[derive(Clone)]
struct AppState {
    // TODO hashmap of hashmaps for all channels
    // arc to prevent clones here, just need read access in the search handler
    options: Arc<HashMap<String, NixosOption>>,
    query_parser: QueryParser,
    searcher: Searcher,
    schema: Schema,
}

impl AppState {
    // TODO error handling
    fn trivial() -> Self {
        let index_path = TempDir::new().unwrap().into_path();
        let options: HashMap<String, NixosOption> =
            serde_json::from_str(&std::fs::read_to_string("out.json").unwrap()).unwrap();
        // init tantivy
        // TODO don't create a new index, parse + write entries on every server start
        {
            create_index(&index_path).unwrap();
            write_entries(&index_path, &options).unwrap();
        }

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
        let query_parser = QueryParser::for_index(&index, vec![name, description, default]);

        Self {
            options: Arc::new(options),
            query_parser,
            searcher,
            schema,
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

    let state = AppState::trivial();
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

async fn index_handler(State(_state): State<AppState>) -> impl IntoResponse {
    HtmlTemplate(IndexTemplate)
}

#[derive(Deserialize, Debug)]
struct SearchForm {
    q: String,
}

#[axum::debug_handler]
#[tracing::instrument(skip(state))]
async fn search_handler(
    State(state): State<AppState>,
    form: axum::extract::Form<SearchForm>,
) -> impl IntoResponse {
    debug!("item handler");

    let query = state.query_parser.parse_query(&form.q).unwrap();
    let top_docs = state
        .searcher
        .search(&query, &TopDocs::with_limit(10))
        .unwrap();

    let name = state
        .schema
        .get_field("name")
        .expect("schema has field name");

    let results: Vec<NaiveNixosOption> = top_docs
        .into_iter()
        .map(|(_score, doc_address)| {
            let retrieved = state.searcher.doc(doc_address).unwrap();
            retrieved
                .get_first(name)
                .expect("result has a value for name")
                .as_text()
                .expect("value is text")
                .to_string()
        })
        .map(|name| {
            let option = state.options.get(&name).unwrap();
            NaiveNixosOption {
                name,
                description: option.description.clone().unwrap_or_default(),
                declarations: option.declarations.clone(),
            }
        })
        .collect();

    let template = ItemTemplate { results };
    HtmlTemplate(template)
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate;

#[derive(Debug)]
struct NaiveNixosOption {
    name: String,
    declarations: Vec<String>,
    description: String,
}

#[derive(Template)]
#[template(path = "item.html")]
struct ItemTemplate {
    results: Vec<NaiveNixosOption>,
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
