use anyhow::Result;
use clap::Parser;
use grid_hype::config::AppConfig;
use grid_hype::engine::LiveGridRunner;
use std::path::PathBuf;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser, Debug)]
#[command(
    name = "grid-hype",
    author = "mev751",
    version = "0.2.0",
    about = "High-performance Hyperliquid Maker Grid Arbitrage Engine in Native Rust"
)]
struct Args {
    /// Path to config TOML file
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,

    /// Run in dry-run mode (display grid levels and calculations without placing real orders)
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,grid_hype=debug")),
        )
        .with_target(false)
        .init();

    let args = Args::parse();
    let config = AppConfig::load_from_file(&args.config)?;

    let runner = LiveGridRunner::new(config, args.dry_run);
    runner.run().await?;

    Ok(())
}
