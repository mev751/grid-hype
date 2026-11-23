/// Formats and rounds price for Hyperliquid perp markets.
/// Hyperliquid requires max 5 significant figures and respects tick size.
pub fn format_hl_price(price: f64, sz_decimals: u8) -> String {
    let max_decimals = (6u8.saturating_sub(sz_decimals)).min(4) as usize;
    if price >= 10000.0 {
        format!("{:.1}", (price * 10.0).round() / 10.0)
    } else if price >= 1000.0 {
        format!("{:.2}", (price * 100.0).round() / 100.0)
    } else if price >= 100.0 {
        format!("{:.3}", (price * 1000.0).round() / 1000.0)
    } else if price >= 10.0 {
        // e.g. HYPE at 63.333 -> 3 decimals (5 sig figs)
        let dec = max_decimals.min(3);
        let factor = 10f64.powi(dec as i32);
        format!("{:.prec$}", (price * factor).round() / factor, prec = dec)
    } else {
        let dec = max_decimals.min(4);
        let factor = 10f64.powi(dec as i32);
        format!("{:.prec$}", (price * factor).round() / factor, prec = dec)
    }
}

/// Formats and rounds size for Hyperliquid perp markets according to `sz_decimals`.
pub fn format_hl_size(size: f64, sz_decimals: u8) -> String {
    let factor = 10f64.powi(sz_decimals as i32);
    let rounded = (size * factor).floor() / factor;
    format!("{:.prec$}", rounded, prec = sz_decimals as usize)
}

pub fn round_hl_price(price: f64, sz_decimals: u8) -> f64 {
    format_hl_price(price, sz_decimals).parse::<f64>().unwrap_or(price)
}

pub fn round_hl_size(size: f64, sz_decimals: u8) -> f64 {
    format_hl_size(size, sz_decimals).parse::<f64>().unwrap_or(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_precision_hype_formatting() {
        assert_eq!(format_hl_price(63.3333333, 2), "63.333");
        assert_eq!(format_hl_price(60.0, 2), "60.000");
        assert_eq!(format_hl_size(15.87654, 2), "15.87");
        assert_eq!(format_hl_size(4.7619, 2), "4.76");
    }
}
