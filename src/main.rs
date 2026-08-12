use anyhow::Result;
use clap::Parser;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use davfs_sync::cli::{self, Cli};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "davfs_sync=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    cli::run(Cli::parse()).await
}
