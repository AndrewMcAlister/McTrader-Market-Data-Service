//! What a provider is, and the two things one has to be able to answer.
//!
//! A provider is not an adapter class hierarchy here on purpose. The service needs exactly
//! two answers — a latest quote and daily bars — and every provider in it (EODHD, Yahoo,
//! Binance) can supply both. Narrowing the contract to those two keeps a third and fourth
//! provider cheap to add and keeps the router's fallback logic honest: if a provider cannot
//! answer, it says so, and the router moves on.
//!
//! `supports` is the routing hook. It answers "could you serve this instrument at all", which
//! is a different question from "did the call work": Binance can never serve an index, so
//! asking it is a wasted request and a wrong-looking failure. A `None` from a symbol render
//! already answers this for symbol-shaped mismatches; `supports` covers the rest.

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde_json::Value;

use crate::domain::asset_class::AssetClass;
use crate::error::ProviderFailure;

/// One instrument's latest price, as any provider can express it.
///
/// `previous_close` is carried because the morning scan's whole question is the move since
/// the prior close, and a price without its reference cannot answer it. `as_of` is the
/// provider's own observation time, not the fetch time; providers that cannot supply one
/// leave it `None` rather than substituting the clock, which would make a stale price look
/// current.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderQuote {
    pub symbol: String,
    pub price: f64,
    pub provider: String,
    pub previous_close: Option<f64>,
    pub as_of: Option<NaiveDateTime>,
    pub currency: Option<String>,
    pub raw_ticker: Option<String>,
    /// Volume traded so far in the session, zero when the provider sent none. It belongs on
    /// the quote because a mover is only a mover relative to how much actually traded.
    pub volume: i64,
}

impl ProviderQuote {
    /// The move since the previous close, computed the same way for every provider.
    pub fn change_pct(&self) -> Option<f64> {
        let previous = self.previous_close?;
        if previous <= 0.0 {
            return None;
        }
        Some((self.price / previous - 1.0) * 100.0)
    }
}

/// One completed daily session, as any provider can express it.
///
/// `adjusted` says whether the value is split-adjusted. It travels with the bar rather than
/// being inferred at the call site because this database already carries both conventions
/// and documents the trap: `Bar5Min` is unadjusted while `BarHourly` is, and a split shows
/// as a jump (JDZG, 2026-03-23: $0.0928 against $20.925 the same day). An ATR mixing the two
/// is silently wrong.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderBar {
    pub session: NaiveDate,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
    pub adjusted: bool,
    /// The unadjusted close. `close` may be adjusted (EODHD's `adjusted_close`); this is the
    /// price that printed, which candle bodies and structure read. `None` means the provider
    /// cannot express it — not zero, and not "same as close" (that must be said explicitly).
    pub raw_close: Option<f64>,
}

/// A market-data source. Two operations, plus a name for provenance.
#[async_trait]
pub trait Provider: Send + Sync {
    /// The value that reaches `provider` in every response. Must be stable: callers use it
    /// to decide whether two numbers came from the same tape.
    fn name(&self) -> &'static str;

    /// Whether this provider can serve `asset_class` at all.
    fn supports(&self, asset_class: AssetClass) -> bool;

    /// The latest price, or `None` when the instrument is not priceable here.
    async fn quote(
        &self,
        symbol: &str,
        asset_class: AssetClass,
        exchange: Option<&str>,
    ) -> Result<Option<ProviderQuote>, ProviderFailure>;

    /// Completed daily bars, oldest first. Empty when the provider cannot answer.
    async fn bars(
        &self,
        symbol: &str,
        asset_class: AssetClass,
        sessions: usize,
        exchange: Option<&str>,
    ) -> Result<Vec<ProviderBar>, ProviderFailure>;
}

/// The whole-market snapshot capability, which only EODHD has.
///
/// It is a separate trait rather than a method on [`Provider`] because no Yahoo or Binance
/// equivalent exists; pretending the capability is portable would put a "not supported"
/// branch in every other implementation.
#[async_trait]
pub trait WholeMarketQuotes: Send + Sync {
    async fn whole_market_quotes(
        &self,
        exchange: Option<&str>,
    ) -> Result<Vec<ProviderQuote>, ProviderFailure>;
}

/// A float from a JSON value, or `None` when it is not a number.
///
/// The vendors send numbers as strings often enough that `"105000.5"` has to parse; `null`,
/// arrays and objects are not numbers. Non-finite values are treated as absent rather than
/// propagating `inf` into a price.
pub(crate) fn number(value: &Value) -> Option<f64> {
    let parsed = match value {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        Value::Bool(value) => {
            if *value {
                1.0
            } else {
                0.0
            }
        }
        _ => return None,
    };
    parsed.is_finite().then_some(parsed)
}

/// Like [`number`], but a zero falls through to the next candidate, matching Python's
/// `_number(value) or fallback`.
pub(crate) fn nonzero(value: &Value) -> Option<f64> {
    number(value).filter(|value| *value != 0.0)
}

/// Python truthiness for the few vendor fields that are tested with `if`.
pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

/// Epoch seconds as naive UTC, which is the shape every stored timestamp uses.
///
/// The providers carry the vendor's observation time, not the fetch time. A missing,
/// non-positive or unrepresentable stamp yields `None` rather than substituting the clock.
pub(crate) fn epoch_naive(value: &Value) -> Option<NaiveDateTime> {
    let seconds = number(value)?;
    if seconds <= 0.0 {
        return None;
    }
    naive_from_seconds(seconds)
}

/// Epoch seconds as naive UTC, without the "must be positive" rule some callers apply.
pub(crate) fn naive_from_seconds(seconds: f64) -> Option<NaiveDateTime> {
    if !seconds.is_finite() {
        return None;
    }
    let whole = seconds.trunc() as i64;
    let mut nanos = (seconds.fract() * 1_000_000_000.0).round() as u32;
    let mut whole = whole;
    if nanos >= 1_000_000_000 {
        whole += 1;
        nanos = 0;
    }
    DateTime::<Utc>::from_timestamp(whole, nanos).map(|stamp| stamp.naive_utc())
}

/// The UTC date an epoch stamp names.
pub(crate) fn epoch_date(value: &Value) -> Option<NaiveDate> {
    epoch_naive(value).map(|stamp| stamp.date())
}

/// The first `limit` characters of vendor text, for an error detail.
pub(crate) fn truncate_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// A vendor volume as a whole number of shares, matching `int(value or 0)`.
///
/// A missing or unparseable volume is zero rather than an error: volume is provenance here,
/// not the answer, and refusing a quote because its size is absent would lose the price.
pub(crate) fn volume(value: &Value) -> i64 {
    number(value).map(|value| value as i64).unwrap_or(0)
}

/// Python's `str()` for the scalar a vendor field can hold, or `None` for null/containers.
pub(crate) fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(value) => Some(if *value { "True" } else { "False" }.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_parse_from_numbers_and_strings_only() {
        assert_eq!(number(&json!(1.5)), Some(1.5));
        assert_eq!(number(&json!(" 2.25 ")), Some(2.25));
        assert_eq!(number(&json!(true)), Some(1.0));
        assert_eq!(number(&json!(null)), None);
        assert_eq!(number(&json!([1.0])), None);
        assert_eq!(number(&json!("not a number")), None);
    }

    #[test]
    fn a_zero_previous_close_falls_through_but_a_real_one_does_not() {
        assert_eq!(nonzero(&json!(0)), None);
        assert_eq!(nonzero(&json!(98.5)), Some(98.5));
    }

    #[test]
    fn change_percent_is_measured_from_the_previous_close() {
        let mut quote = ProviderQuote {
            symbol: "TEST".into(),
            price: 105.0,
            provider: "test".into(),
            previous_close: Some(100.0),
            as_of: None,
            currency: None,
            raw_ticker: None,
            volume: 0,
        };
        let change = quote.change_pct().unwrap();
        assert!((change - 5.0).abs() < 1e-12, "got {change}");
        quote.previous_close = Some(0.0);
        assert_eq!(quote.change_pct(), None);
        quote.previous_close = None;
        assert_eq!(quote.change_pct(), None);
    }

    #[test]
    fn epoch_seconds_become_naive_utc_and_bad_stamps_are_absent() {
        // 2025-09-01 12:00:00 UTC
        let stamp = json!(1_756_728_000i64);
        assert_eq!(
            epoch_naive(&stamp),
            NaiveDate::from_ymd_opt(2025, 9, 1).map(|date| date.and_hms_opt(12, 0, 0).unwrap())
        );
        assert_eq!(epoch_naive(&json!(0)), None);
        assert_eq!(epoch_naive(&json!(-1)), None);
        assert_eq!(epoch_naive(&json!(null)), None);
    }
}
