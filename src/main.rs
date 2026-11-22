use anyhow::Result;
use clap::Parser;
use grid_hype::{config::AppConfig, engine::LiveGridRunner};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "grid-hype",
    version,
    about = "Generalized Hyperliquid Maker Grid Arbitrage Engine built on NautilusTrader (Rust)"
)]
struct Cli {
    /// Path to the TOML configuration file
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,

    /// Run in dry-run mode (fetches live market metadata & BBO and prints the Maker grid table without placing orders)
    #[arg(long)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let cfg = AppConfig::load(&cli.config)?;
    let runner = LiveGridRunner::new(cfg, cli.dry_run);
    runner.run().await
}
