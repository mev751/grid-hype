use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{env, fs, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Long,
    Short,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridConfig {
    pub symbol: String,
    pub direction: Direction,
    pub lower_price: f64,
    pub upper_price: f64,
    pub grid_count: usize,
    pub amount_per_grid: f64,
    #[serde(default = "default_true")]
    pub post_only: bool,
    #[serde(default = "default_active_entry_grids")]
    pub active_entry_grids: usize,
    #[serde(default = "default_false")]
    pub enable_base_position: bool,
    #[serde(default = "default_base_position_grids")]
    pub base_position_grids: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeConfig {
    #[serde(default)]
    pub testnet: bool,
    #[serde(default = "default_pk_env")]
    pub private_key_env: String,
    pub account_address: Option<String>,
    #[serde(default = "default_reconcile_interval")]
    pub reconcile_interval_secs: u64,
    #[serde(default)]
    pub cancel_on_exit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub grid: GridConfig,
    pub exchange: ExchangeConfig,
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

fn default_base_position_grids() -> usize {
    3
}

fn default_active_entry_grids() -> usize {
    3
}

fn default_pk_env() -> String {
    "HYPERLIQUID_PK".to_string()
}

fn default_reconcile_interval() -> u64 {
    5
}

impl AppConfig {
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path.as_ref())
            .with_context(|| format!("Failed to read config file at {:?}", path.as_ref()))?;
        let config: AppConfig = toml::from_str(&content)
            .with_context(|| format!("Failed to parse TOML configuration from {:?}", path.as_ref()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.grid.symbol.trim().is_empty() {
            bail!("grid.symbol cannot be empty");
        }
        if self.grid.lower_price <= 0.0 || self.grid.upper_price <= 0.0 {
            bail!("grid lower_price and upper_price must be > 0.0");
        }
        if self.grid.lower_price >= self.grid.upper_price {
            bail!(
                "grid.lower_price ({}) must be strictly less than grid.upper_price ({})",
                self.grid.lower_price,
                self.grid.upper_price
            );
        }
        if self.grid.grid_count == 0 {
            bail!("grid.grid_count must be >= 1");
        }
        if self.grid.amount_per_grid <= 0.0 {
            bail!("grid.amount_per_grid must be > 0.0");
        }
        if self.grid.enable_base_position && self.grid.base_position_grids == 0 {
            bail!("grid.base_position_grids must be >= 1 when enable_base_position is true");
        }
        Ok(())
    }

    pub fn resolve_private_key(&self) -> Result<String> {
        let env_key = &self.exchange.private_key_env;
        if let Ok(val) = env::var(env_key) {
            let clean = val.trim().trim_matches('"').trim_matches('\'').to_string();
            if !clean.is_empty() {
                return Ok(clean);
            }
        }
        if let Ok(val) = env::var("PRIVATE_KEY") {
            let clean = val.trim().trim_matches('"').trim_matches('\'').to_string();
            if !clean.is_empty() {
                return Ok(clean);
            }
        }
        bail!(
            "Missing private key: environment variable '{}' (or 'PRIVATE_KEY') not set",
            env_key
        )
    }

    pub fn resolve_account_address(&self) -> Option<String> {
        if let Some(ref addr) = self.exchange.account_address {
            let clean = addr.trim().trim_matches('"').trim_matches('\'').to_string();
            if !clean.is_empty() {
                return Some(clean);
            }
        }
        if let Ok(val) = env::var("HYPERLIQUID_ACCOUNT_ADDRESS") {
            let clean = val.trim().trim_matches('"').trim_matches('\'').to_string();
            if !clean.is_empty() {
                return Some(clean);
            }
        }
        None
    }
}
