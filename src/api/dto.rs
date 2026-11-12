//! The request and query shapes, field for field with the source Pydantic models.
//!
//! Defaults are applied where FastAPI applied them, so `{"symbols": ["TSLA"]}` means the same
//! thing here as it did there. Constraints that Pydantic declared — one symbol at minimum,
//! sessions at least one, a limit between 1 and 200 — are checked by [`Validation`] so the
//! refusal is a 422 shaped like FastAPI's rather than a panic or a silent clamp.

use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::py_repr;
use crate::domain::asset_class::AssetClass;
use crate::domain::availability::{find_country, Country, COUNTRIES};
use crate::domain::intraday::{Timeframe, TIMEFRAMES};
use crate::domain::screener::{LIMIT_DEFAULT, LIMIT_MAX};
use crate::products::BulkDateKey;

use super::errors::ApiError;

/// The FastAPI-shaped refusal for a value outside a declared bound.
pub fn validation(location: &[&str], kind: &str, message: &str, input: Value) -> ApiError {
    ApiError::validation(location, kind, message, input)
}

/// The country a request names, or the FastAPI-shaped refusal for a missing or unknown one.
///
/// `country_code` is required on every symbol-bearing call: a ticker alone does not name a
/// market (`CBA` is an ASX listing *and* a US one), and answering a request that named no
/// market from the US tape is exactly the silent wrong number the field exists to prevent.
/// The accepted set is the country table the availability endpoints already read, so `US` and
/// `AU` are the only answers today and adding a country there adds it here.
pub fn require_country(
    value: Option<&str>,
    location: &[&str],
) -> Result<&'static Country, ApiError> {
    let text = value.map(str::trim).unwrap_or_default();
    if text.is_empty() {
        return Err(validation(
            location,
            "missing",
            "Field required",
            Value::Null,
        ));
    }
    find_country(text).ok_or_else(|| {
        let expected = COUNTRIES
            .iter()
            .map(|country| format!("'{}'", country.key))
            .collect::<Vec<_>>()
            .join(" or ");
        validation(
            location,
            "literal_error",
            &format!("Input should be {expected}"),
            json!(text),
        )
    })
}

/// At least one symbol, as `min_length=1` required.
pub fn require_symbols(symbols: &[String]) -> Result<(), ApiError> {
    if symbols.is_empty() {
        return Err(validation(
            &["body", "symbols"],
            "too_short",
            "List should have at least 1 item after validation, not 0",
            json!([]),
        ));
    }
    Ok(())
}

/// `sessions>=1`, as declared on `BarsRequest`.
pub fn require_sessions(sessions: i64) -> Result<(), ApiError> {
    if sessions < 1 {
        return Err(validation(
            &["body", "sessions"],
            "greater_than_equal",
            "Input should be greater than or equal to 1",
            json!(sessions),
        ));
    }
    Ok(())
}

/// An integer inside `low..=high`, refusals named the way FastAPI names them.
pub fn require_between(low: i64, high: i64, value: i64, location: &[&str]) -> Result<(), ApiError> {
    if value < low {
        return Err(validation(
            location,
            "greater_than_equal",
            &format!("Input should be greater than or equal to {low}"),
            json!(value),
        ));
    }
    if value > high {
        return Err(validation(
            location,
            "less_than_equal",
            &format!("Input should be less than or equal to {high}"),
            json!(value),
        ));
    }
    Ok(())
}

/// A non-negative integer (`offset`).
pub fn require_non_negative_integer(value: i64, location: &[&str]) -> Result<(), ApiError> {
    if value < 0 {
        return Err(validation(
            location,
            "greater_than_equal",
            "Input should be greater than or equal to 0",
            json!(value),
        ));
    }
    Ok(())
}

/// `asset_class` on the four symbol routes: absent means shares, a typo is refused.
///
/// The library's [`AssetClass::parse`] folds an unknown spelling to shares, which is right for
/// an internal caller that already holds a class and wrong for a request: an index priced as a
/// share is a different instrument, not a rounding difference. So the boundary asks the strict
/// parser and answers `422 unknown-asset-class` with the vocabulary it accepts.
///
/// A present-but-blank value is treated as absent, because absent is the documented default and
/// `?asset_class=` is how a client spells "not stated". The refusal is the `{"error", "detail"}`
/// pair rather than FastAPI's shape: a caller branches on `unknown-asset-class` and reads the
/// accepted list from `detail`.
pub fn require_asset_class(raw: Option<&str>) -> Result<Option<String>, ApiError> {
    let Some(text) = raw.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(None);
    };
    match AssetClass::parse_strict(text) {
        Some(class) => Ok(Some(class.as_str().to_string())),
        None => Err(ApiError::error(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "unknown-asset-class",
            format!(
                "{} is not an asset class; use one of {}",
                py_repr(text),
                AssetClass::accepted_spellings()
            ),
        )),
    }
}

/// `POST /profile`'s symbol cap: the batch cost bound, 25 units per pass.
pub fn require_profile_symbols(symbols: &[String]) -> Result<(), ApiError> {
    require_symbol_count(symbols, 1, PROFILE_SYMBOLS_MAX, &["body", "symbols"])
}

/// The most symbols one profile request may name. `25 × 5 units` is 125, the batch bound the
/// plan states; a larger pass is several requests, which is the caller's decision to make.
pub const PROFILE_SYMBOLS_MAX: usize = 25;

/// A symbol list between `low` and `high` entries.
pub fn require_symbol_count(
    symbols: &[String],
    low: usize,
    high: usize,
    location: &[&str],
) -> Result<(), ApiError> {
    if symbols.len() < low {
        return Err(validation(
            location,
            "too_short",
            &format!("List should have at least {low} item after validation, not 0"),
            json!(symbols),
        ));
    }
    if symbols.len() > high {
        return Err(validation(
            location,
            "too_long",
            &format!("List should have at most {high} items after validation, not {}", symbols.len()),
            json!(symbols),
        ));
    }
    Ok(())
}

/// `POST /bars/intraday`'s timeframe, as one of the five the route serves.
pub fn require_timeframe(raw: Option<&str>, location: &[&str]) -> Result<Timeframe, ApiError> {
    let Some(text) = raw.map(str::trim).filter(|text| !text.is_empty()) else {
        return Err(validation(location, "missing", "Field required", Value::Null));
    };
    Timeframe::parse(text).ok_or_else(|| {
        let expected = TIMEFRAMES
            .iter()
            .map(|timeframe| format!("'{}'", timeframe.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        validation(
            location,
            "literal_error",
            &format!("Input should be one of {expected}"),
            json!(text),
        )
    })
}

/// `POST /bars/bulk`'s date keys: 1..10 of `"latest"` or `YYYY-MM-DD`, duplicates collapsed.
///
/// Collapsing first means a caller who repeats one date is not refused for asking twice about
/// the same session; the bound is on distinct questions, which is what the vendor calls cost.
pub fn require_bulk_dates(raw: &[String]) -> Result<Vec<BulkDateKey>, ApiError> {
    let mut keys: Vec<BulkDateKey> = Vec::with_capacity(raw.len());
    for (index, value) in raw.iter().enumerate() {
        let text = value.trim();
        let key = if text.eq_ignore_ascii_case(BULK_LATEST) {
            BulkDateKey::Latest
        } else {
            NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .map(BulkDateKey::Session)
                .map_err(|_| {
                    validation(
                        &["body", "dates", &index.to_string()],
                        "invalid-date",
                        "Input should be 'latest' or a YYYY-MM-DD date",
                        json!(value),
                    )
                })?
        };
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    if keys.is_empty() {
        return Err(validation(
            &["body", "dates"],
            "too_short",
            "List should have at least 1 item after validation, not 0",
            json!(raw),
        ));
    }
    if keys.len() > BULK_DATES_MAX {
        return Err(validation(
            &["body", "dates"],
            "too_long",
            &format!(
                "List should have at most {BULK_DATES_MAX} distinct dates after validation, not {}",
                keys.len()
            ),
            json!(raw),
        ));
    }
    Ok(keys)
}

/// The literal `POST /bars/bulk` accepts in place of a date.
pub const BULK_LATEST: &str = "latest";

/// The most sessions one bulk request may name: ten pulls is already 1,000 units.
pub const BULK_DATES_MAX: usize = 10;

/// The screener's bounds, re-exported so the handler states them once.
pub const SCREENER_LIMIT_DEFAULT: i64 = LIMIT_DEFAULT;
pub const SCREENER_LIMIT_MAX: i64 = LIMIT_MAX;

/// One intraday bar is a moment; 500 of them is already more than a session holds.
pub const INTRADAY_LIMIT_MAX: i64 = 500;

/// What an intraday request returns when it names no limit: one US session of 5-minute bars.
pub const INTRADAY_LIMIT_DEFAULT: i64 = 78;

/// `limit` between 1 and 200, as declared on `/todays-big-movers`.
pub fn require_limit(limit: i64) -> Result<(), ApiError> {
    if limit < 1 {
        return Err(validation(
            &["query", "limit"],
            "greater_than_equal",
            "Input should be greater than or equal to 1",
            json!(limit),
        ));
    }
    if limit > 200 {
        return Err(validation(
            &["query", "limit"],
            "less_than_equal",
            "Input should be less than or equal to 200",
            json!(limit),
        ));
    }
    Ok(())
}

/// A non-negative float query bound (`min_price`, `min_dollar_volume`).
pub fn require_non_negative(field: &str, value: f64) -> Result<(), ApiError> {
    if value.is_finite() && value >= 0.0 {
        return Ok(());
    }
    Err(validation(
        &["query", field],
        "greater_than_equal",
        "Input should be greater than or equal to 0",
        json!(value),
    ))
}

/// FastAPI's boolean query vocabulary, case-insensitively. An unrecognised value falls back
/// to the default rather than failing the request; the source suite never pinned that edge.
pub fn query_bool(value: Option<&str>, default: bool) -> bool {
    let Some(text) = value.map(str::trim) else {
        return default;
    };
    match text.to_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" => true,
        "0" | "false" | "f" | "no" | "n" | "off" => false,
        _ => default,
    }
}

fn default_market() -> String {
    "US".to_string()
}

fn default_sessions() -> i64 {
    15
}

fn default_true() -> bool {
    true
}

fn default_limit() -> i64 {
    25
}

fn default_min_price() -> f64 {
    1.0
}

fn default_broker_id() -> i64 {
    1
}

/// `POST /prices`.
#[derive(Debug, Clone, Deserialize)]
pub struct PricesRequest {
    pub symbols: Vec<String>,
    #[serde(default)]
    pub exchange: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub max_age_seconds: Option<f64>,
    #[serde(default)]
    pub asset_class: Option<String>,
}

/// `POST /atr`.
#[derive(Debug, Clone, Deserialize)]
pub struct AtrRequestBody {
    pub symbols: Vec<String>,
    #[serde(default)]
    pub period: Option<i64>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub smoothing: Option<String>,
    #[serde(default)]
    pub exchange: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub price: Option<f64>,
    #[serde(default)]
    pub multiplier: Option<f64>,
    #[serde(default)]
    pub direction: Option<String>,
    #[serde(default)]
    pub asset_class: Option<String>,
}

/// `POST /bars`.
#[derive(Debug, Clone, Deserialize)]
pub struct BarsRequestBody {
    pub symbols: Vec<String>,
    #[serde(default = "default_sessions")]
    pub sessions: i64,
    #[serde(default)]
    pub exchange: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default = "default_true")]
    pub allow_fetch: bool,
    #[serde(default)]
    pub asset_class: Option<String>,
}

/// `POST /secrets/eodhd`.
#[derive(Debug, Clone, Deserialize)]
pub struct EodhdKeyRequest {
    pub api_key: String,
}

/// `POST /refresh`.
#[derive(Debug, Clone, Deserialize)]
pub struct RefreshRequestBody {
    #[serde(default = "default_market")]
    pub market: String,
    #[serde(default)]
    pub session_date: Option<NaiveDate>,
    #[serde(default)]
    pub force: bool,
    #[serde(default = "default_true")]
    pub include_indicators: bool,
}

impl Default for RefreshRequestBody {
    fn default() -> Self {
        Self {
            market: default_market(),
            session_date: None,
            force: false,
            include_indicators: true,
        }
    }
}

/// `GET /price/{symbol}`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PriceQuery {
    #[serde(default)]
    pub exchange: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub max_age_seconds: Option<f64>,
    #[serde(default)]
    pub asset_class: Option<String>,
}

/// `POST /screener`.
///
/// `filters` stays a JSON array of values: the vendor's triple vocabulary is validated by
/// [`crate::domain::screener`], which is also what names the offending filter in a refusal.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScreenerRequest {
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub exchange: Option<String>,
    #[serde(default)]
    pub filters: Vec<Value>,
    #[serde(default)]
    pub sort: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// `POST /profile`.
#[derive(Debug, Clone, Deserialize)]
pub struct ProfilesRequest {
    pub symbols: Vec<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub exchange: Option<String>,
}

/// `POST /bars/intraday`.
#[derive(Debug, Clone, Deserialize)]
pub struct IntradayBody {
    pub symbols: Vec<String>,
    #[serde(default)]
    pub timeframe: Option<String>,
    /// One US session of 5-minute bars when omitted.
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default = "default_true")]
    pub allow_fetch: bool,
    #[serde(default)]
    pub exchange: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
}

/// `POST /bars/bulk`.
#[derive(Debug, Clone, Deserialize)]
pub struct BulkBody {
    #[serde(default = "default_market")]
    pub market: String,
    #[serde(default)]
    pub dates: Vec<String>,
    #[serde(default = "default_true")]
    pub allow_fetch: bool,
}

/// `GET /market/clock`.
#[derive(Debug, Clone, Deserialize)]
pub struct ClockQuery {
    #[serde(default = "default_broker_id")]
    pub broker_id: i64,
    #[serde(default)]
    pub time: Option<String>,
    #[serde(default)]
    pub at: Option<String>,
}

/// `GET /market/session`.
///
/// The rule resolved is always the `shares` rule: the request no longer names an asset class,
/// because the vendor taxonomy behind it proved unreliable. The answer still reports the class
/// it used, so a caller can see that the rule was shares.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionQuery {
    #[serde(default = "default_broker_id")]
    pub broker_id: i64,
    #[serde(default)]
    pub at: Option<String>,
}

/// `GET /trading_sessions`.
#[derive(Debug, Clone, Deserialize)]
pub struct TradingSessionsQuery {
    #[serde(default = "default_broker_id")]
    pub broker_id: i64,
    #[serde(default)]
    pub time: Option<String>,
    #[serde(default)]
    pub use_broker_timezone: Option<String>,
}

/// `GET /todays-data-is-available`.
#[derive(Debug, Clone, Deserialize)]
pub struct CountryQuery {
    /// Required; `None` is what the handler refuses as a 422 rather than silently reading US.
    #[serde(default)]
    pub country_code: Option<String>,
}

/// `GET /todays-big-movers`.
#[derive(Debug, Clone, Deserialize)]
pub struct MoversQuery {
    /// Required; see [`CountryQuery::country_code`].
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default = "default_min_price")]
    pub min_price: f64,
    #[serde(default)]
    pub min_dollar_volume: f64,
}

/// `POST /schedule/run`.
#[derive(Debug, Clone, Deserialize)]
pub struct ScheduleRunQuery {
    #[serde(default = "default_market")]
    pub market: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_defaults_match_the_published_contract() {
        let body: BarsRequestBody = serde_json::from_value(json!({"symbols": ["TSLA"]})).unwrap();
        assert_eq!(body.sessions, 15);
        assert!(body.allow_fetch);
        assert!(body.exchange.is_none());
    }

    #[test]
    fn refresh_defaults_match_the_published_contract() {
        let body: RefreshRequestBody = serde_json::from_value(json!({})).unwrap();
        assert_eq!(body.market, "US");
        assert!(!body.force);
        assert!(body.include_indicators);
        assert!(body.session_date.is_none());
    }

    #[test]
    fn the_boolean_query_vocabulary_is_case_insensitive() {
        assert!(query_bool(Some("TRUE"), false));
        assert!(query_bool(Some("1"), false));
        assert!(!query_bool(Some("off"), true));
        assert!(query_bool(None, true));
    }

    #[test]
    fn an_empty_symbol_list_is_a_fastapi_shaped_422() {
        let error = require_symbols(&[]).unwrap_err();
        assert_eq!(error.status, 422);
        assert_eq!(error.body["detail"][0]["loc"], json!(["body", "symbols"]));
        assert!(require_symbols(&["AAPL".to_string()]).is_ok());
    }

    #[test]
    fn the_movers_bounds_are_enforced() {
        assert!(require_limit(0).is_err());
        assert!(require_limit(201).is_err());
        assert!(require_limit(25).is_ok());
        assert!(require_non_negative("min_price", 0.0).is_ok());
        assert!(require_non_negative("min_price", -1.0).is_err());
    }

    #[test]
    fn a_missing_country_code_is_a_422_naming_the_field() {
        let error = require_country(None, &["query", "country_code"]).unwrap_err();
        assert_eq!(error.status, 422);
        assert_eq!(
            error.body["detail"][0]["loc"],
            json!(["query", "country_code"])
        );
        assert_eq!(error.body["detail"][0]["type"], "missing");

        let blank = require_country(Some("   "), &["body", "country_code"]).unwrap_err();
        assert_eq!(blank.body["detail"][0]["type"], "missing");
    }

    #[test]
    fn a_country_code_is_trimmed_case_insensitively_and_narrowed_to_the_table() {
        assert_eq!(
            require_country(Some(" au "), &["body", "country_code"])
                .unwrap()
                .key,
            "AU"
        );
        assert_eq!(
            require_country(Some("US"), &["body", "country_code"])
                .unwrap()
                .key,
            "US"
        );
    }

    #[test]
    fn an_unknown_country_code_lists_the_countries_that_are_served() {
        let error = require_country(Some("NZ"), &["body", "country_code"]).unwrap_err();
        assert_eq!(error.status, 422);
        assert_eq!(error.body["detail"][0]["type"], "literal_error");
        let message = error.body["detail"][0]["msg"].as_str().unwrap();
        assert!(message.contains("'US'"), "{message}");
        assert!(message.contains("'AU'"), "{message}");
        assert_eq!(error.body["detail"][0]["input"], json!("NZ"));
    }
}
