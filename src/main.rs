#![feature(duration_constructors)]

use std::path::PathBuf;
use std::process::exit;

use clap::Parser;
use tempfile::TempDir;
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod backend;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Port to run on
    #[arg(short, long, default_value_t = 8000)]
    port: u16,

    /// Path to a state directory for caching indexed data.
    /// If not provided will cache in memory
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// fetch + index a single branch at a specific revision
    /// only use for testing purposes
    /// default behaviour is to fetch all branches from hydra
    /// and build all of them
    #[arg(long)]
    test: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "with_axum_htmx_askama=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    if let Some(state_dir) = args.state_dir {
        info!("Persistent state dir is {}", state_dir.display());
        backend::run(args.port, &state_dir, args.test).await?;
    } else {
        let temp_state_dir = TempDir::new().unwrap();
        info!("Temporary state dir is {}", temp_state_dir.path().display());

        // remove the temp dir on ctrl-c
        let path: PathBuf = temp_state_dir.path().to_path_buf();
        ctrlc::set_handler(move || {
            info!("Removing temporary state dir: {}", path.display());
            std::fs::remove_dir_all(&path)
                .unwrap_or_else(|e| warn!("failed to remove temp state dir {:?}", e));
            exit(0);
        })
        .expect("failed to set a handler for c-c");

        backend::run(args.port, temp_state_dir.path(), args.test).await?;
    }

    Ok(())
}
