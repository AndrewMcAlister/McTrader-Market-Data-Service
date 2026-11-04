//! Provider-specific symbol rendering.
//!
//! Canonical identity is `(raw_symbol, asset_class, venue)`: `("IXIC", indices, None)`,
//! `("GLD", commodities, None)`, `("CBA", shares, AU)`. The raw symbol is never decorated by
//! the caller — no `^`, no `=F`, no `.US` — so each provider re-interprets the same identity
//! in its own convention.
//!
//! [`symbol_for_provider`] returns `None` when a provider cannot serve the instrument
//! (Binance has no indices; EODHD has no bare commodity futures). Callers must treat `None`
//! as "this provider cannot answer" and fall through — never as a symbol to attempt.
//!
//! Owner-confirmed EODHD codes (2026-09-15): `TNX.INDX`, `US10Y.INDX`, `GLD.US`. Commodities
//! deliberately use US-listed ETF proxies on EODHD rather than unverified `.COMM` futures
//! codes.

use super::asset_class::AssetClass;

pub const EODHD: &str = "eodhd";
pub const YAHOO: &str = "yahoo";
pub const BINANCE: &str = "binance";

/// A provider this service can route to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderId {
    Eodhd,
    Yahoo,
    Binance,
}

impl ProviderId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Eodhd => EODHD,
            Self::Yahoo => YAHOO,
            Self::Binance => BINANCE,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            EODHD => Some(Self::Eodhd),
            YAHOO => Some(Self::Yahoo),
            BINANCE => Some(Self::Binance),
            _ => None,
        }
    }
}

/// The US venue codes that mean the house default, `US`.
const US_VENUE_CODES: &[&str] = &[
    "US", "NASDAQ", "NASDAQGS", "NASDAQGM", "NASDAQCM", "NMS", "NYSE", "NYQ", "NYS", "NYSEARCA",
    "ARCA", "PCX", "AMEX", "ASE", "BATS", "IEX", "OTC", "OTCQX", "OTCQB", "PINK",
];

/// Which currency a venue's shares settle in. One row per currency, naming the venues and the
/// provider suffixes that mean it, so EODHD and Yahoo cannot disagree about what an ASX price
/// is denominated in.
const VENUE_CURRENCIES: &[(&str, &[&str], &[&str])] = &[
    ("AUD", &["ASX", "AU", "AX"], &[".AU", ".AX"]),
    ("GBP", &["LSE", "L", "LON", "UK"], &[".L"]),
    ("CAD", &["TSX", "TSE", "TO", "CA"], &[".TO"]),
    ("HKD", &["HKEX", "HKG", "HK"], &[".HK"]),
    (
        "EUR",
        &["XETRA", "DAX", "FRA", "F", "DE", "CAC", "PA", "PAR", "FR"],
        &[".F", ".PA"],
    ),
];

pub const DEFAULT_EODHD_EXCHANGE: &str = "US";

/// The undecorated identity for `symbol`, tolerating a Yahoo-style `^` prefix.
pub fn normalise_raw_symbol(symbol: &str) -> String {
    let mut raw = symbol.trim().to_uppercase();
    while let Some(stripped) = raw.strip_prefix('^') {
        raw = stripped.to_string();
    }
    raw
}

fn effective_exchange(exchange: Option<&str>) -> Option<String> {
    exchange
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_uppercase)
}

/// The currency `symbol`'s venue settles in, or `None` when unknown.
pub fn venue_currency_code(symbol: &str, exchange: Option<&str>) -> Option<&'static str> {
    let venue = effective_exchange(exchange);
    let upper = symbol.trim().to_uppercase();
    for (currency, venues, suffixes) in VENUE_CURRENCIES {
        let venue_matches = venue
            .as_deref()
            .is_some_and(|value| venues.contains(&value));
        if venue_matches || suffixes.iter().any(|suffix| upper.ends_with(suffix)) {
            return Some(currency);
        }
    }
    None
}

/// The EODHD venue suffix `exchange` names, or `None` if unknown.
///
/// `None` lets the price API refuse an unrecognised venue instead of silently falling back to
/// the house US default — a non-US symbol priced off a US record is not an answer.
pub fn eodhd_venue_suffix(exchange: Option<&str>) -> Option<&'static str> {
    let venue = effective_exchange(exchange)?;
    if US_VENUE_CODES.contains(&venue.as_str()) {
        return Some(DEFAULT_EODHD_EXCHANGE);
    }
    match venue.as_str() {
        "ASX" | "AU" | "AX" => Some("AU"),
        "LSE" | "L" | "LON" | "UK" => Some("L"),
        "TSX" | "TSE" | "TO" | "CA" => Some("TO"),
        "HKEX" | "HKG" | "HK" => Some("HK"),
        "XETRA" | "DAX" | "FRA" | "F" | "DE" => Some("F"),
        "CAC" | "PA" | "PAR" | "FR" => Some("PA"),
        _ => None,
    }
}

/// The one EODHD ticker for `raw_symbol` as a share/ETF — empty when the symbol is empty.
pub fn eodhd_equity_ticker(
    raw_symbol: &str,
    exchange: Option<&str>,
    default_exchange: Option<&str>,
) -> String {
    let raw = normalise_raw_symbol(raw_symbol);
    if raw.is_empty() {
        return String::new();
    }
    if let Some((root, suffix)) = raw.rsplit_once('.') {
        if root.is_empty() || suffix.is_empty() {
            return String::new();
        }
        let suffix = if suffix == "AX" { "AU" } else { suffix };
        return format!("{root}.{suffix}");
    }
    if let Some(suffix) = eodhd_venue_suffix(exchange) {
        return format!("{raw}.{suffix}");
    }
    let default = default_exchange
        .unwrap_or("")
        .trim()
        .to_uppercase()
        .trim_start_matches('.')
        .to_string();
    let default = if default.is_empty() {
        DEFAULT_EODHD_EXCHANGE.to_string()
    } else {
        default
    };
    format!("{raw}.{default}")
}

/// Render `raw_symbol` in `provider`'s convention, or `None` if it cannot serve it.
pub fn symbol_for_provider(
    raw_symbol: &str,
    asset_class: AssetClass,
    exchange: Option<&str>,
    provider: ProviderId,
) -> Option<String> {
    let raw = normalise_raw_symbol(raw_symbol);
    if raw.is_empty() {
        return None;
    }
    let exchange_key = effective_exchange(exchange);
    match provider {
        ProviderId::Eodhd => eodhd_symbol(&raw, asset_class, exchange_key.as_deref()),
        ProviderId::Yahoo => yahoo_symbol(&raw, asset_class, exchange_key.as_deref()),
        ProviderId::Binance => binance_symbol(&raw, asset_class),
    }
}

fn eodhd_symbol(raw: &str, asset_class: AssetClass, exchange: Option<&str>) -> Option<String> {
    match asset_class {
        AssetClass::Indices | AssetClass::Rates => Some(match raw {
            "IXIC" => "IXIC.INDX".to_string(),
            "GSPC" => "GSPC.INDX".to_string(),
            "US10Y" => "US10Y.INDX".to_string(),
            "TNX" => "TNX.INDX".to_string(),
            other => format!("{other}.INDX"),
        }),
        AssetClass::Commodities => match raw {
            "GLD" => Some("GLD.US".to_string()),
            "SLV" => Some("SLV.US".to_string()),
            "BNO" => Some("BNO.US".to_string()),
            "UUP" => Some("UUP.US".to_string()),
            _ => None,
        },
        AssetClass::Forex => Some(format!("{raw}.FOREX")),
        AssetClass::Crypto => Some(format!("{raw}.CC")),
        AssetClass::Shares => {
            let ticker = eodhd_equity_ticker(raw, exchange, None);
            if ticker.is_empty() {
                None
            } else {
                Some(ticker)
            }
        }
    }
}

fn yahoo_symbol(raw: &str, asset_class: AssetClass, exchange: Option<&str>) -> Option<String> {
    match asset_class {
        AssetClass::Indices | AssetClass::Rates => Some(match raw {
            "IXIC" => "^IXIC".to_string(),
            "GSPC" => "^GSPC".to_string(),
            "US10Y" | "TNX" => "^TNX".to_string(),
            other => format!("^{other}"),
        }),
        AssetClass::Commodities => match raw {
            "GLD" => Some("GC=F".to_string()),
            "SLV" => Some("SI=F".to_string()),
            "BNO" => Some("BZ=F".to_string()),
            "UUP" => Some("DX-Y.NYB".to_string()),
            _ => None,
        },
        AssetClass::Forex => Some(format!("{raw}=X")),
        AssetClass::Crypto => Some(raw.to_string()),
        AssetClass::Shares => {
            if exchange == Some("AU") {
                Some(format!("{raw}.AX"))
            } else {
                Some(raw.to_string())
            }
        }
    }
}

fn binance_symbol(raw: &str, asset_class: AssetClass) -> Option<String> {
    if asset_class != AssetClass::Crypto {
        return None;
    }
    Some(raw.replace(['-', '/'], ""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_symbols_are_upper_cased_and_stripped_of_carets() {
        assert_eq!(normalise_raw_symbol("  ^^ixic "), "IXIC");
        assert_eq!(normalise_raw_symbol(""), "");
    }

    #[test]
    fn a_share_gets_the_venue_suffix_or_the_house_default() {
        assert_eq!(eodhd_equity_ticker("AAPL", None, None), "AAPL.US");
        assert_eq!(eodhd_equity_ticker("BHP", Some("AU"), None), "BHP.AU");
        assert_eq!(eodhd_equity_ticker("BHP", Some("ASX"), None), "BHP.AU");
        assert_eq!(eodhd_equity_ticker("VOD", Some("L"), None), "VOD.L");
        assert_eq!(eodhd_equity_ticker("CBA.AX", None, None), "CBA.AU");
        assert_eq!(eodhd_equity_ticker("BRK.B", None, None), "BRK.B");
        assert_eq!(eodhd_equity_ticker("", None, None), "");
    }

    #[test]
    fn a_dotted_ticker_is_not_re_suffixed() {
        assert_eq!(eodhd_equity_ticker("BRK.B", Some("AU"), None), "BRK.B");
        assert_eq!(eodhd_equity_ticker("BHP.AU", None, None), "BHP.AU");
    }

    #[test]
    fn indexes_and_rates_use_the_confirmed_codes() {
        assert_eq!(
            symbol_for_provider("IXIC", AssetClass::Indices, None, ProviderId::Eodhd),
            Some("IXIC.INDX".to_string())
        );
        assert_eq!(
            symbol_for_provider("TNX", AssetClass::Rates, None, ProviderId::Eodhd),
            Some("TNX.INDX".to_string())
        );
        assert_eq!(
            symbol_for_provider("SPX", AssetClass::Indices, None, ProviderId::Eodhd),
            Some("SPX.INDX".to_string())
        );
        assert_eq!(
            symbol_for_provider("IXIC", AssetClass::Indices, None, ProviderId::Yahoo),
            Some("^IXIC".to_string())
        );
        assert_eq!(
            symbol_for_provider("SPX", AssetClass::Indices, None, ProviderId::Yahoo),
            Some("^SPX".to_string())
        );
        assert_eq!(
            symbol_for_provider("IXIC", AssetClass::Indices, None, ProviderId::Binance),
            None
        );
    }

    #[test]
    fn commodities_use_etf_proxies_on_eodhd_and_futures_on_yahoo() {
        assert_eq!(
            symbol_for_provider("GLD", AssetClass::Commodities, None, ProviderId::Eodhd),
            Some("GLD.US".to_string())
        );
        assert_eq!(
            symbol_for_provider("GLD", AssetClass::Commodities, None, ProviderId::Yahoo),
            Some("GC=F".to_string())
        );
        assert_eq!(
            symbol_for_provider("CL", AssetClass::Commodities, None, ProviderId::Eodhd),
            None
        );
        assert_eq!(
            symbol_for_provider("CL", AssetClass::Commodities, None, ProviderId::Yahoo),
            None
        );
        assert_eq!(
            symbol_for_provider("GLD", AssetClass::Commodities, None, ProviderId::Binance),
            None
        );
    }

    #[test]
    fn forex_uses_the_vendor_suffixes() {
        assert_eq!(
            symbol_for_provider("AUDUSD", AssetClass::Forex, None, ProviderId::Eodhd),
            Some("AUDUSD.FOREX".to_string())
        );
        assert_eq!(
            symbol_for_provider("AUDUSD", AssetClass::Forex, None, ProviderId::Yahoo),
            Some("AUDUSD=X".to_string())
        );
    }

    #[test]
    fn crypto_has_no_venue_and_binance_strips_separators() {
        assert_eq!(
            symbol_for_provider("BTC-USD", AssetClass::Crypto, Some("AU"), ProviderId::Eodhd),
            Some("BTC-USD.CC".to_string())
        );
        assert_eq!(
            symbol_for_provider("BTC-USD", AssetClass::Crypto, None, ProviderId::Yahoo),
            Some("BTC-USD".to_string())
        );
        assert_eq!(
            symbol_for_provider("BTC/USD", AssetClass::Crypto, None, ProviderId::Binance),
            Some("BTCUSD".to_string())
        );
    }

    #[test]
    fn yahoo_shares_use_the_asx_suffix_only_for_the_au_venue() {
        assert_eq!(
            symbol_for_provider("CBA", AssetClass::Shares, Some("AU"), ProviderId::Yahoo),
            Some("CBA.AX".to_string())
        );
        assert_eq!(
            symbol_for_provider("CBA", AssetClass::Shares, Some("ASX"), ProviderId::Yahoo),
            Some("CBA".to_string())
        );
    }

    #[test]
    fn binance_serves_crypto_only() {
        assert_eq!(
            symbol_for_provider("AAPL", AssetClass::Shares, None, ProviderId::Binance),
            None
        );
    }

    #[test]
    fn venue_currency_reads_the_venue_or_the_suffix() {
        assert_eq!(venue_currency_code("CBA", Some("AU")), Some("AUD"));
        assert_eq!(venue_currency_code("CBA.AX", None), Some("AUD"));
        assert_eq!(venue_currency_code("VOD", Some("LSE")), Some("GBP"));
        assert_eq!(venue_currency_code("SHOP", Some("TO")), Some("CAD"));
        assert_eq!(venue_currency_code("0700", Some("HK")), Some("HKD"));
        assert_eq!(venue_currency_code("SAP", Some("XETRA")), Some("EUR"));
        assert_eq!(venue_currency_code("AAPL", Some("US")), None);
    }
}
