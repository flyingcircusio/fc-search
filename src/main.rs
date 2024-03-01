use std::path::PathBuf;
use std::process::exit;

use clap::Parser;
use tempfile::TempDir;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod backend;

/// Simple program to greet a person
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    dbg!(&args);

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "with_axum_htmx_askama=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    if let Some(state_dir) = args.state_dir {
        info!("Persistent state dir is {}", state_dir.display());
        backend::run(args.port, false, &state_dir).await?;
    } else {
        let temp_state_dir = TempDir::new().unwrap();
        info!("Temporary state dir is {}", temp_state_dir.path().display());

        // remove the temp dir on ctrl-c
        let path: PathBuf = temp_state_dir.path().to_path_buf();
        ctrlc::set_handler(move || {
            info!("Removing temporary state dir: {}", path.display());
            std::fs::remove_dir_all(&path).expect("failed to remove the temp dir");
            exit(0);
        })
        .expect("could not set a handler for c-c");

        backend::run(args.port, false, temp_state_dir.path()).await?;
    }

    Ok(())
}
