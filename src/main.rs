use std::collections::HashMap;

use anyhow::Context;
use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use tower_http::services::ServeDir;
use tracing::{debug, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use fc_search::NixosOption;
use serde::Deserialize;

#[derive(Debug, Clone)]
struct AppState {
    options: HashMap<String, NixosOption>,
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

    let assets_path = std::env::current_dir().unwrap();

    let options = HashMap::new();
    let state = AppState { options };

    let router = Router::new()
        .route("/", get(index_handler))
        .route("/search", get(search_handler))
        .with_state(state)
        .nest_service(
            "/assets",
            ServeDir::new(format!("{}/assets", assets_path.to_str().unwrap())),
        );
    let port = 8000_u16;
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));

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
#[tracing::instrument]
async fn search_handler(
    State(_state): State<AppState>,
    form: axum::extract::Form<SearchForm>,
) -> impl IntoResponse {
    debug!("item handler");

    let template = ItemTemplate {
        results: vec![
            NaiveNixosOption {
                name: "nginx 1".to_string(),
                description: "just a nginx item".to_string(),
                declarations: vec![
                    "/nix/store/test".to_string(),
                    "/and/a/second/location".to_string(),
                ],
            },
            NaiveNixosOption {
                name: "nginx 2".to_string(),
                description: "just a test item 2".to_string(),
                declarations: vec![
                    "/nix/store/test".to_string(),
                    "/and/a/second/location".to_string(),
                ],
            },
        ],
    };

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
