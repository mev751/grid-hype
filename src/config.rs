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
    #[serde(default = "default_true")]
    pub follow_price_into_idle_grids: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeConfig {
    #[serde(default)]
    pub testnet: bool,
    #[serde(default = "default_pk_env")]
    pub private_key_env: String,
    #[serde(default = "default_vault_env")]
    pub vault_address_env: String,
    #[serde(default = "default_account_env")]
    pub account_address_env: String,
    #[serde(default = "default_timeout_secs")]
    pub http_timeout_secs: u64,
    #[serde(default = "default_reconcile_secs")]
    pub reconcile_interval_secs: u64,
    #[serde(default)]
    pub cancel_on_exit: bool,
    #[serde(default)]
    pub include_builder_attribution: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub grid: GridConfig,
    pub exchange: ExchangeConfig,
}

fn default_true() -> bool {
    true
}

fn default_pk_env() -> String {
    "HYPERLIQUID_PK".to_string()
}

fn default_vault_env() -> String {
    "HYPERLIQUID_VAULT".to_string()
}

fn default_account_env() -> String {
    "HYPERLIQUID_ACCOUNT_ADDRESS".to_string()
}

fn default_timeout_secs() -> u64 {
    15
}

fn default_reconcile_secs() -> u64 {
    10
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        let mut cfg: Self = toml::from_str(&content)
            .with_context(|| format!("Failed to parse TOML in: {}", path.display()))?;
        cfg.grid.symbol = cfg.grid.symbol.trim().to_uppercase();
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.grid.symbol.trim().is_empty() {
            bail!("grid.symbol cannot be empty");
        }
        if !self.grid.lower_price.is_finite()
            || !self.grid.upper_price.is_finite()
            || self.grid.lower_price <= 0.0
        {
            bail!("grid.lower_price and grid.upper_price must be positive finite numbers");
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
        if !self.grid.amount_per_grid.is_finite() || self.grid.amount_per_grid < 10.0 {
            bail!(
                "grid.amount_per_grid ({}) must be at least 10.0 USDC (Hyperliquid minimum order value)",
                self.grid.amount_per_grid
            );
        }
        Ok(())
    }

    /// Resolves the EVM private key from the configured environment variable,
    /// with fallback to `HYPERLIQUID_PK`, `HYPERLIQUID_TESTNET_PK`, and `PRIVATE_KEY`.
    pub fn resolve_private_key(&self) -> Result<String> {
        let candidates = [
            self.exchange.private_key_env.as_str(),
            "HYPERLIQUID_PK",
            "HYPERLIQUID_TESTNET_PK",
            "PRIVATE_KEY",
        ];
        for var_name in candidates {
            if var_name.is_empty() {
                continue;
            }
            if let Ok(val) = env::var(var_name) {
                let trimmed = val.trim();
                if !trimmed.is_empty() {
                    return Ok(trimmed.to_string());
                }
            }
        }
        bail!(
            "Missing private key in environment variable '{}' (or fallback HYPERLIQUID_PK / PRIVATE_KEY)",
            self.exchange.private_key_env
        )
    }

    /// Resolves the optional Hyperliquid vault or subaccount address from the environment.
    pub fn resolve_vault_address(&self) -> Option<String> {
        let candidates = [
            self.exchange.vault_address_env.as_str(),
            "HYPERLIQUID_VAULT",
            "HYPERLIQUID_TESTNET_VAULT",
        ];
        for var_name in candidates {
            if var_name.is_empty() {
                continue;
            }
            if let Ok(val) = env::var(var_name) {
                let trimmed = val.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
        None
    }

    /// Resolves the optional Hyperliquid master account address (for agent/API wallet usage) from the environment.
    pub fn resolve_account_address(&self) -> Option<String> {
        let candidates = [
            self.exchange.account_address_env.as_str(),
            "HYPERLIQUID_ACCOUNT_ADDRESS",
            "ACCOUNT_ADDRESS",
            "HL_WALLET_ADDRESS",
        ];
        for var_name in candidates {
            if var_name.is_empty() {
                continue;
            }
            if let Ok(val) = env::var(var_name) {
                let trimmed = val.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn loads_default_config_toml_for_hype_long_55_to_65_30_grids_1000_usdc() {
        let cfg = AppConfig::load(&PathBuf::from("config.toml")).expect("should parse config.toml");
        assert_eq!(cfg.grid.symbol, "HYPE");
        assert_eq!(cfg.grid.direction, Direction::Long);
        assert!((cfg.grid.lower_price - 55.0).abs() < 1e-9);
        assert!((cfg.grid.upper_price - 65.0).abs() < 1e-9);
        assert_eq!(cfg.grid.grid_count, 30);
        assert!((cfg.grid.amount_per_grid - 1000.0).abs() < 1e-9);
        assert!(cfg.grid.post_only);
    }

    #[test]
    fn rejects_invalid_grid_bounds_and_counts() {
        let mut cfg = AppConfig::load(&PathBuf::from("config.toml")).expect("load config");
        cfg.grid.lower_price = 65.0;
        cfg.grid.upper_price = 55.0;
        assert!(cfg.validate().is_err());

        cfg.grid.lower_price = 55.0;
        cfg.grid.upper_price = 65.0;
        cfg.grid.grid_count = 0;
        assert!(cfg.validate().is_err());

        cfg.grid.grid_count = 30;
        cfg.grid.amount_per_grid = 5.0;
        assert!(cfg.validate().is_err());
    }
}
