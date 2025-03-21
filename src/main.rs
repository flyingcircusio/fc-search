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

    /// run a test version with pre-compile options + packages and no updater
    #[arg(long)]
    test: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "fc_search=debug,tokio=trace,runtime=trace".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let state_dir = args.state_dir.unwrap_or_else(|| {
        let temp_state_dir = TempDir::new().unwrap();
        info!("Temporary state dir is {}", temp_state_dir.path().display());

        // remove the temp dir on ctrl-c
        let path: PathBuf = temp_state_dir.path().to_path_buf();
        let handler_path = path.clone();
        ctrlc::set_handler(move || {
            info!("Removing temporary state dir: {}", handler_path.display());
            std::fs::remove_dir_all(&handler_path)
                .unwrap_or_else(|e| warn!("failed to remove temp state dir {:?}", e));
            exit(0);
        })
        .expect("failed to set a handler for c-c");
        path
    });
    info!("State dir is {}", state_dir.display());

    if args.test {
        backend::run_test(args.port, &state_dir).await?;
    } else {
        backend::run(args.port, &state_dir).await?;
    }

    Ok(())
}
