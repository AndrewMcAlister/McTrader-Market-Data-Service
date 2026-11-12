//! Which market a symbol request means: the exchange, and every way a caller states it.
//!
//! A ticker is not unique across markets. `CBA` is an ASX listing **and** a US listing with a
//! different price, a different daily series and its own ATR, so a request that names a
//! symbol has to name the market with it. Three spellings of that answer arrive, and they mean
//! the same thing:
//!
//! * `exchange` — the vendor's code: `US`, `AU`/`ASX`, `L`/`LSE`, `TO`, `HK`
//! * `country_code` — the country: `US`, `AU`
//! * the suffix on the symbol itself: `CBA.AU`
//!
//! THE ORDER OF PRECEDENCE, and why it is this order:
//!
//! 1. **A stated `exchange`/`country_code`.** The caller said which market it wants, so that is
//!    authoritative. Stating both is allowed while they agree and refused when they do not.
//! 2. **The suffix, when every symbol carries the same one.** `["CBA.AU", "BHP.AU"]` is an
//!    ASX request that stated no exchange, and pricing it from the US tape is exactly the silent
//!    wrong number the exchange argument exists to prevent.
//! 3. **`US`, the house default.** The HTTP layer requires `country_code` on every
//!    symbol-bearing request, so an API caller cannot reach this step; it exists for internal
//!    callers, which may state no market at all.
//!
//! Two refusals rather than guesses. A suffix that contradicts a stated exchange is a
//! `exchange-conflict`, and a batch whose symbols name two different markets is
//! `mixed-exchanges`. One request is one market, because the rows are read and written under a
//! single exchange key.
//!
//! Only a suffix that is in the exchange table counts, so `BRK.B` is a dotted ticker rather than
//! a Belgian listing. Crypto has no exchange at all — `BTC-USD` is the same instrument whatever
//! tape answers — so its market is `CRYPTO` and a stated exchange is ignored rather than refused.

use std::collections::BTreeMap;

use crate::domain::asset_class::CRYPTO_MARKET;
use crate::domain::availability::COUNTRIES;
use crate::error::{py_repr, MarketDataError};

/// What a request means when nothing states a market. The same default the vendor's own
/// documentation uses, and the one every existing caller relies on.
pub const DEFAULT_EXCHANGE: &str = "US";

/// How many offending symbols a refusal quotes. Enough to recognise the mistake, short
/// enough that a 2,800-symbol request does not answer with a 2,800-symbol error.
const SAMPLE: usize = 3;

/// The EODHD exchange code for an exchange name, or `None` when it is not one.
///
/// A blank value means `US`, matching the source service's `normalise_exchange`. Refusing an
/// unknown exchange rather than defaulting to US is deliberate: silently pricing an Australian
/// listing from the US feed produces a wrong number that looks right.
pub fn normalise_exchange_code(text: Option<&str>) -> Option<String> {
    let key = text.unwrap_or("").trim().to_uppercase();
    if key.is_empty() {
        return Some(DEFAULT_EXCHANGE.to_string());
    }
    exchange_alias(&key).map(str::to_string)
}

/// Whether an upper-cased code names a known exchange.
pub fn is_exchange_alias(code: &str) -> bool {
    exchange_alias(code).is_some()
}

/// The vendor aliases the estate actually types.
///
/// `BATS`, `ARCA` and `IEX` are named here because a caller's own venue vocabulary does
/// (`services/watchlist_service.py` sends BATS), and refusing a legitimate venue would be a
/// regression. They resolve to the US *market*; which spelling travels to the vendor is the
/// caller's decision, and `/screener` appends the caller's value rather than this one.
fn exchange_alias(key: &str) -> Option<&'static str> {
    Some(match key {
        "US" | "NYSE" | "NASDAQ" | "AMEX" | "NYSEARCA" | "ARCA" | "BATS" | "IEX" => "US",
        "AU" | "ASX" | "AX" | "AUS" => "AU",
        "L" | "LSE" | "UK" | "GB" | "LON" => "L",
        "TO" | "TSX" | "CA" => "TO",
        "V" | "TSXV" => "V",
        "HK" | "HKG" => "HK",
        "F" | "DE" | "XETRA" | "FRANKFURT" | "GER" => "F",
        "PA" | "PARIS" | "FR" => "PA",
        "AS" | "AMS" => "AS",
        "SW" | "SWX" => "SW",
        "MI" | "MIL" => "MI",
        "MC" | "MAD" => "MC",
        "ST" | "STO" => "ST",
        "OL" | "OSL" => "OL",
        "CO" | "CPH" => "CO",
        "HE" | "HEL" => "HE",
        "NZ" | "NZX" => "NZ",
        "SI" | "SGX" => "SI",
        "TSE" | "JP" => "TSE",
        "KO" | "KRX" => "KO",
        "NSE" => "NSE",
        "BSE" => "BSE",
        "JSE" => "JSE",
        "SA" | "BOVESPA" => "SA",
        "MX" | "BMV" => "MX",
        _ => return None,
    })
}

/// One country or exchange code as the exchange code the caches are keyed by, or refuse.
///
/// `us`/`NYSE` -> `US`, `au`/`asx`/`AX` -> `AU`, `uk`/`lse`/`GB` -> `L`. The country table
/// the availability endpoints read wins where it has an entry, so `AU` means the same market
/// on every endpoint even if the vendor's alias table ever disagreed.
pub fn exchange_for(code: &str) -> Result<String, MarketDataError> {
    let key = code.trim().to_uppercase();
    if key.is_empty() {
        return Err(MarketDataError::bad_request(
            "unknown-exchange",
            "a blank exchange or country code is not a market",
        ));
    }
    if let Some(country) = COUNTRIES.iter().find(|country| country.key == key) {
        return Ok(country.exchange.to_string());
    }
    match normalise_exchange_code(Some(&key)) {
        Some(exchange) => Ok(exchange),
        None => Err(MarketDataError::bad_request(
            "unknown-exchange",
            format!(
                "{} is not a country or exchange code; use US, AU, L, TO, HK, F, PA, ...",
                py_repr(code)
            ),
        )),
    }
}

/// The exchange a symbol names in its own suffix (`CBA.AU` -> `AU`), or `None`.
///
/// Only a suffix that is in the exchange table counts. `BRK.B` is a class share and `AX` is the
/// ASX under another name, so the two are told apart by the table rather than by the dot.
pub fn suffix_exchange(symbol: &str) -> Option<String> {
    let text = symbol.trim().to_uppercase();
    let (root, tail) = text.rsplit_once('.')?;
    if root.is_empty() || tail.is_empty() {
        return None;
    }
    normalise_exchange_code(Some(tail))
}

/// The market a request means, as the code the caches and the vendor are keyed by.
///
/// `symbols` is what the suffix rule reads, so it must be the symbols as the caller sent
/// them — `CBA.AU`, not `CBA`. Returns an exchange code (`US`, `AU`, ...) or `CRYPTO`, and raises
/// a 400 [`MarketDataError`] for an unknown code, a contradiction, or a batch that spans two
/// markets.
pub fn resolve_exchange(
    exchange: Option<&str>,
    country_code: Option<&str>,
    symbols: &[&str],
    asset_class: Option<&str>,
) -> Result<String, MarketDataError> {
    let kind = asset_class.unwrap_or("").trim().to_lowercase();
    if kind == "crypto" {
        return Ok(CRYPTO_MARKET.to_string());
    }

    let stated: Vec<(&str, &str)> = [("exchange", exchange), ("country_code", country_code)]
        .into_iter()
        .filter_map(|(name, value)| {
            let value = value?;
            if value.trim().is_empty() {
                None
            } else {
                Some((name, value))
            }
        })
        .collect();

    let mut resolved: Vec<(&str, String)> = Vec::with_capacity(stated.len());
    for (name, value) in &stated {
        resolved.push((name, exchange_for(value)?));
    }
    let distinct: std::collections::BTreeSet<&str> =
        resolved.iter().map(|(_, value)| value.as_str()).collect();
    if distinct.len() > 1 {
        let stated_text = stated
            .iter()
            .map(|(name, value)| format!("{name} {}", py_repr(value)))
            .collect::<Vec<_>>()
            .join("; ");
        let markets = distinct.into_iter().collect::<Vec<_>>().join(", ");
        return Err(MarketDataError::bad_request(
            "exchange-conflict",
            format!("{stated_text} name different markets ({markets}); state one of them"),
        ));
    }

    // Which exchange each symbol names in its own suffix, with the symbols that said so, so a
    // refusal can quote the ones that caused it.
    let mut in_symbol: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for raw in symbols {
        if let Some(code) = suffix_exchange(raw) {
            in_symbol
                .entry(code)
                .or_default()
                .push(raw.trim().to_uppercase());
        }
    }
    let examples = || {
        let names: Vec<&str> = in_symbol
            .values()
            .flat_map(|names| names.iter().map(String::as_str))
            .collect();
        let shown = names
            .iter()
            .take(SAMPLE)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        if names.len() > SAMPLE {
            format!("{shown} and {} more", names.len() - SAMPLE)
        } else {
            shown
        }
    };

    if let Some((_, market)) = resolved.first() {
        if !in_symbol.is_empty() && !in_symbol.contains_key(market) {
            let suffixes = in_symbol.keys().cloned().collect::<Vec<_>>().join("/");
            return Err(MarketDataError::bad_request(
                "exchange-conflict",
                format!(
                    "{} was asked for, but {} name the {suffixes} market in the symbol suffix; \
                     send bare symbols with an exchange, or suffixed symbols without one",
                    py_repr(market),
                    examples()
                ),
            ));
        }
        return Ok(market.clone());
    }

    if in_symbol.len() == 1 {
        return Ok(in_symbol.keys().next().cloned().unwrap_or_default());
    }
    if in_symbol.len() > 1 {
        let markets = in_symbol.keys().cloned().collect::<Vec<_>>().join(", ");
        return Err(MarketDataError::bad_request(
            "mixed-exchanges",
            format!(
                "the symbols name different markets ({markets}) -- {}; one request is one \
                 market, so split the list",
                examples()
            ),
        ));
    }
    Ok(DEFAULT_EXCHANGE.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_country_code_and_a_exchange_code_name_the_same_market() {
        assert_eq!(exchange_for("au").unwrap(), "AU");
        assert_eq!(exchange_for(" AU ").unwrap(), "AU");
        assert_eq!(exchange_for("ASX").unwrap(), "AU");
        assert_eq!(exchange_for("uk").unwrap(), "L");
        assert_eq!(exchange_for("NYSE").unwrap(), "US");
    }

    #[test]
    fn the_us_venue_vocabulary_resolves_to_the_us_market() {
        // McTrader's per-venue US loop and its watchlist service name these; a screener that
        // refused BATS would be refusing a live caller's ordinary request.
        for venue in ["US", "NASDAQ", "NYSE", "AMEX", "NYSEARCA", "ARCA", "BATS", "IEX"] {
            assert_eq!(exchange_for(venue).unwrap(), "US", "{venue}");
        }
        for venue in ["AU", "ASX", "AX", "AUS"] {
            assert_eq!(exchange_for(venue).unwrap(), "AU", "{venue}");
        }
    }

    #[test]
    fn an_unknown_market_is_refused_by_name_not_defaulted_to_us() {
        let error = exchange_for("NOWHERE").unwrap_err();
        assert_eq!(error.reason, "unknown-exchange");
        assert_eq!(error.status, 400);
    }

    #[test]
    fn a_blank_market_is_not_a_market() {
        assert_eq!(exchange_for("   ").unwrap_err().reason, "unknown-exchange");
    }

    #[test]
    fn a_suffix_names_a_market_and_a_class_share_does_not() {
        assert_eq!(suffix_exchange("CBA.AU").as_deref(), Some("AU"));
        assert_eq!(suffix_exchange("CBA.AX").as_deref(), Some("AU"));
        assert_eq!(suffix_exchange("VOD.L").as_deref(), Some("L"));
        assert_eq!(suffix_exchange("BRK.B"), None);
        assert_eq!(suffix_exchange("AAPL"), None);
    }

    #[test]
    fn nothing_stated_and_bare_symbols_means_us() {
        assert_eq!(
            resolve_exchange(None, None, &["AAPL", "MSFT"], None).unwrap(),
            "US"
        );
        assert_eq!(resolve_exchange(None, None, &[], None).unwrap(), "US");
    }

    #[test]
    fn a_suffixed_symbol_states_the_market_it_names() {
        assert_eq!(
            resolve_exchange(None, None, &["CBA.AU", "BHP.AU"], None).unwrap(),
            "AU"
        );
    }

    #[test]
    fn a_country_code_picks_the_market_without_a_exchange_field() {
        assert_eq!(
            resolve_exchange(None, Some("AU"), &["CBA"], None).unwrap(),
            "AU"
        );
        assert_eq!(
            resolve_exchange(None, Some("us"), &["CBA"], None).unwrap(),
            "US"
        );
    }

    #[test]
    fn a_stated_market_wins_over_a_bare_symbol() {
        assert_eq!(
            resolve_exchange(Some("ASX"), None, &["CBA"], None).unwrap(),
            "AU"
        );
    }

    #[test]
    fn exchange_and_country_code_may_agree() {
        assert_eq!(
            resolve_exchange(Some("AU"), Some("au"), &["CBA"], None).unwrap(),
            "AU"
        );
    }

    #[test]
    fn exchange_and_country_code_that_disagree_are_refused() {
        let error = resolve_exchange(Some("US"), Some("AU"), &["CBA"], None).unwrap_err();
        assert_eq!(error.reason, "exchange-conflict");
        assert!(error.detail.contains("AU"));
        assert!(error.detail.contains("US"));
    }

    #[test]
    fn a_suffix_that_contradicts_the_stated_market_is_refused() {
        let error = resolve_exchange(Some("US"), None, &["CBA.AU"], None).unwrap_err();
        assert_eq!(error.reason, "exchange-conflict");
        assert!(error.detail.contains("CBA.AU"));
    }

    #[test]
    fn two_markets_in_one_batch_are_refused_with_the_offending_symbols() {
        let error = resolve_exchange(None, None, &["CBA.AU", "VOD.L"], None).unwrap_err();
        assert_eq!(error.reason, "mixed-exchanges");
        assert!(error.detail.contains("CBA.AU"));
        assert!(error.detail.contains("VOD.L"));
        assert!(error.detail.contains("split the list"));
    }

    #[test]
    fn a_bare_symbol_in_a_suffixed_batch_is_priced_at_that_market() {
        assert_eq!(
            resolve_exchange(None, None, &["AAPL", "CBA.AU"], None).unwrap(),
            "AU"
        );
    }

    #[test]
    fn crypto_has_its_own_market_and_no_exchange() {
        assert_eq!(
            resolve_exchange(Some("AU"), Some("AU"), &["BTC-USD"], Some("crypto")).unwrap(),
            "CRYPTO"
        );
    }
}
