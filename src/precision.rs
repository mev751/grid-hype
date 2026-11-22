use nautilus_hyperliquid::common::parse::{normalize_price, normalize_quantity};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use std::str::FromStr;

/// Hyperliquid Perpetual max price decimals = `max(0, 6 - sz_decimals)`.
pub fn perp_price_decimals(sz_decimals: u8) -> u8 {
    6_u8.saturating_sub(sz_decimals)
}

/// Rounds a perpetual price according to Hyperliquid's official NautilusTrader rules:
/// 1. At most 5 significant figures (`round_to_sig_figs(price, 5)`).
/// 2. At most `(6 - sz_decimals)` decimal places.
pub fn round_hl_price_decimal(price: f64, sz_decimals: u8) -> Decimal {
    if !price.is_finite() || price <= 0.0 {
        return Decimal::ZERO;
    }
    let raw = Decimal::from_str(&format!("{price:.8}")).unwrap_or(Decimal::ZERO);
    let price_decimals = perp_price_decimals(sz_decimals);
    normalize_price(raw, price_decimals).normalize()
}

pub fn round_hl_price(price: f64, sz_decimals: u8) -> f64 {
    round_hl_price_decimal(price, sz_decimals)
        .to_f64()
        .unwrap_or(0.0)
}

pub fn format_hl_price(price: f64, sz_decimals: u8) -> String {
    round_hl_price_decimal(price, sz_decimals).to_string()
}

/// Rounds a perpetual order size to `sz_decimals` decimal places.
pub fn round_hl_size_decimal(size: f64, sz_decimals: u8) -> Decimal {
    if !size.is_finite() || size <= 0.0 {
        return Decimal::ZERO;
    }
    let raw = Decimal::from_str(&format!("{size:.8}")).unwrap_or(Decimal::ZERO);
    normalize_quantity(raw, sz_decimals).normalize()
}

pub fn round_hl_size(size: f64, sz_decimals: u8) -> f64 {
    round_hl_size_decimal(size, sz_decimals)
        .to_f64()
        .unwrap_or(0.0)
}

pub fn format_hl_size(size: f64, sz_decimals: u8) -> String {
    round_hl_size_decimal(size, sz_decimals).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_hyperliquid_perp_prices_to_5_sig_figs_and_max_decimals() {
        // For HYPE-PERP (sz_decimals = 2), max price decimals = 6 - 2 = 4, and 5 sig figs:
        // 55.33333333 -> 5 sig figs is 55.333 (3 decimal places <= 4)
        assert_eq!(format_hl_price(55.0, 2), "55");
        assert_eq!(format_hl_price(55.333333333, 2), "55.333");
        assert_eq!(format_hl_price(60.12, 2), "60.12");
        assert_eq!(format_hl_price(64.666666667, 2), "64.667");
    }

    #[test]
    fn formats_hyperliquid_perp_sizes_to_sz_decimals() {
        // 1000.0 / 55.0 = 18.181818... -> sz_decimals = 2 -> 18.18
        assert_eq!(format_hl_size(1000.0 / 55.0, 2), "18.18");
        assert!((round_hl_size(1000.0 / 55.0, 2) - 18.18).abs() < 1e-9);
    }
}
