//! The request and query shapes, field for field with the source Pydantic models.
//!
//! Defaults are applied where FastAPI applied them, so `{"symbols": ["TSLA"]}` means the same
//! thing here as it did there. Constraints that Pydantic declared — one symbol at minimum,
//! sessions at least one, a limit between 1 and 200 — are checked by [`Validation`] so the
//! refusal is a 422 shaped like FastAPI's rather than a panic or a silent clamp.

use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{json, Value};

use super::errors::ApiError;

/// The FastAPI-shaped refusal for a value outside a declared bound.
pub fn validation(location: &[&str], kind: &str, message: &str, input: Value) -> ApiError {
    ApiError::validation(location, kind, message, input)
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

fn default_asset_class() -> String {
    "shares".to_string()
}

/// `POST /prices`.
#[derive(Debug, Clone, Deserialize)]
pub struct PricesRequest {
    pub symbols: Vec<String>,
    #[serde(default)]
    pub venue: Option<String>,
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
    pub venue: Option<String>,
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
    pub venue: Option<String>,
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
    pub venue: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub max_age_seconds: Option<f64>,
    #[serde(default)]
    pub asset_class: Option<String>,
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
#[derive(Debug, Clone, Deserialize)]
pub struct SessionQuery {
    #[serde(default = "default_broker_id")]
    pub broker_id: i64,
    #[serde(default = "default_asset_class")]
    pub asset_class: String,
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
    #[serde(default = "default_market")]
    pub country_code: String,
}

/// `GET /todays-big-movers`.
#[derive(Debug, Clone, Deserialize)]
pub struct MoversQuery {
    #[serde(default = "default_market")]
    pub country_code: String,
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
        assert!(body.venue.is_none());
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
}
