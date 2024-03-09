use std::path::PathBuf;

use clap::Parser;
use fc_search::nix::build_options_for_fcio_branch;
use fc_search::search::update_index;
use fc_search::{get_fcio_flake_uris, load_packages_and_options, Flake};
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
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
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "with_axum_htmx_askama=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let args = Args::parse();
    if args.test {
        let fc_nixos = Flake::new("flyingcircusio", "fc-nixos", "fc-23.11-dev").await?;
        let (options, packages) = build_options_for_fcio_branch(&fc_nixos).unwrap();
        let outstring = serde_json::to_string(&options)?;
        std::fs::write("out.json", outstring)?;

        let outstring = serde_json::to_string(&packages)?;
        std::fs::write("packages.json", outstring)?;

        return Ok(());
    }

    let Some(state_dir) = args.state_dir else {
        anyhow::bail!("state dir is required if not testing");
    };
    anyhow::ensure!(state_dir.exists(), "state dir does not exist");

    let branches = get_fcio_flake_uris()
        .await
        .expect("failed to get branch information from hydra");

    for flake in branches {
        let branchname = flake.branch.clone();
        let branch_path = state_dir.join(branchname.clone());

        match load_packages_and_options(&branch_path, &flake) {
            Ok(_) => info!("branch {} is up to date", flake.branch),
            Err(_) => {
                warn!("need to rebuild options for branch {}", flake.branch);
                if let Err(e) = update_index(&branch_path, &flake) {
                    error!("failed to build options for branch {}: {e:?}", flake.branch);
                } else {
                    info!("successfully rebuilt options for branch {}", flake.branch);
                }
            }
        }
    }

    Ok(())
}
