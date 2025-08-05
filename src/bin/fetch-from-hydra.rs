use fc_search::get_fcio_flake_uris;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "fc_search=debug,tokio=trace,runtime=trace".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
    dbg!(&get_fcio_flake_uris().await.unwrap());
    Ok(())
}
