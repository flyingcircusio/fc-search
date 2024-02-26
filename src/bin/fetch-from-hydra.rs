use fc_search::get_fcio_flake_uris;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dbg!(&get_fcio_flake_uris().await.unwrap());
    Ok(())
}
