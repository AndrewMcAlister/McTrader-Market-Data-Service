//! The EODHD screener's transport vocabulary: 14 fields, their operators, and one venue.
//!
//! This module owns what the vendor accepts, not what a caller should ask. Which floors,
//! sort and limit a screen uses is policy and stays in the caller; whether a filter can be
//! put on the wire at all is transport and lives here. The distinction is the whole point of
//! the endpoint: a misspelt field would otherwise still reach the vendor, and the vendor's
//! answer to some bad values is HTTP 200 with zero rows — a silent empty screen.
//!
//! The tables are restated from `D:\McTrader\market_data\eodhd_field_reference.py`, which is
//! the human authority for the vendor contract and carries the probe dates. Each side pins
//! the 14 names in a test, so a value added on one side only fails loudly rather than
//! drifting.
//!
//! The venue is part of the filters, not a parameter: EODHD ignores a bare top-level
//! `exchange`, so exactly one `["exchange", "=", venue]` triple is appended last.

use serde_json::{json, Value};

use crate::domain::exchanges::exchange_for;
use crate::error::{py_repr, MarketDataError};

/// The 14 fields the vendor will filter and sort on.
///
/// `SUPPORTED_FIELDS` in `eodhd_field_reference.py` is the same list. A field in the row
/// payload is not necessarily filterable — `currency_symbol` and `last_day_data_date` come back
/// in every row and reject as filters — which is why this is a positive list and not "anything
/// the vendor returns".
pub const SCREENER_FIELDS: &[&str] = &[
    "code",
    "name",
    "exchange",
    "sub_exchange",
    "sector",
    "industry",
    "market_capitalization",
    "earnings_share",
    "dividend_yield",
    "adjusted_close",
    "avgvol_1d",
    "avgvol_200d",
    "refund_1d_p",
    "refund_5d_p",
];

/// The text fields: `=` and `in` only. A comparison operator on text is HTTP 422.
const TEXT_FIELDS: &[&str] = &["code", "name", "exchange", "sub_exchange", "sector", "industry"];

/// The operators a numeric field accepts.
///
/// `in` is deliberately absent: EODHD's parser accepts it and then fails server-side with
/// HTTP 500, so allowing it would turn a caller's typo into a vendor outage.
pub const NUMERIC_OPS: &[&str] = &["=", "!=", ">", "<", ">=", "<="];

/// The operators a text field accepts.
pub const TEXT_OPS: &[&str] = &["=", "in"];

/// The verified vendor cap: 500 returned 500 rows.
pub const LIMIT_MAX: i64 = 500;

/// What the vendor does with an absent limit.
pub const LIMIT_DEFAULT: i64 = 100;

/// How many filters one request may carry. A screen of a market needs a handful; a bound keeps
/// one request from becoming an unreadable 64-kilobyte query.
pub const MAX_FILTERS: usize = 64;

/// Return windows longer than `refund_5d_p` do not exist as screener fields.
///
/// They are computed locally from whole-market closes, which is what [`crate::domain::screener`]
/// tells the caller when it refuses one.
pub const DERIVED_FIELDS: &[&str] = &["return_3m_p", "return_100d_p", "return_200d_p"];

/// Fields a caller may reasonably expect that reject as both filter and sort.
///
/// The second element is the guidance the 422 carries. `eodhd_field_reference.py`'s
/// `REJECTED_FIELDS` is the authority, with the probe evidence.
pub const REJECTED_FIELDS: &[(&str, &str)] = &[
    ("refund_1m_p", "no 1-month return field exists; refund_5d_p is the only multi-day return"),
    ("refund_3m_p", "no 3-month return field exists; use the derived return_3m_p"),
    ("refund_52w_p", "no 52-week return field exists; refund_5d_p is the only multi-day return"),
    ("revenue_growth", "no revenue-growth field in the REST screener"),
    ("pe_ttm", "no P/E field in the REST screener"),
    ("debt_equity", "no debt/equity field in the REST screener"),
    ("relative_volume", "no relative-volume field; derive it as avgvol_1d / avgvol_200d"),
    (
        "country",
        "not a screener field, and pairing it with `exchange` ANDs down to zero rows; use `exchange`",
    ),
    ("volume", "not a field; use avgvol_1d"),
    ("beta", "no beta field in the REST screener"),
    ("peg_ratio", "no PEG field in the REST screener"),
    ("price_book", "no price/book field in the REST screener"),
    ("price_sales", "no price/sales field in the REST screener"),
    ("return_equity", "no ROE field in the REST screener"),
    ("gross_margin", "no gross-margin field in the REST screener"),
    ("short_ratio", "no short-ratio field in the REST screener"),
    ("float_short", "no short-float field in the REST screener"),
    ("target_price", "no analyst target-price field in the REST screener"),
    ("currency_symbol", "returned in every row, but not filterable or sortable"),
    ("last_day_data_date", "returned in every row, but not filterable or sortable"),
    ("refund_1d", "returned in every row, but not filterable or sortable; use refund_1d_p"),
    ("refund_5d", "returned in every row, but not filterable or sortable; use refund_5d_p"),
];

/// Sector spellings that answer HTTP 200 with zero rows, and the spelling that works.
///
/// EODHD's `sector` and `industry` are Yahoo-style, not GICS. The failure is silent at the
/// vendor, so it has to be loud here.
pub const SECTOR_TRAPS: &[(&str, &str)] = &[
    ("Health Care", "Healthcare"),
    ("Materials", "Basic Materials"),
    ("Consumer Discretionary", "Consumer Cyclical"),
    ("Consumer Staples", "Consumer Defensive"),
    ("Communications", "Communication Services"),
    ("Telecommunication Services", "Communication Services"),
    ("Information Technology", "Technology"),
    ("Finance", "Financial Services"),
    ("", "a sector is required — an empty value matches nothing"),
];

/// How many rows a profile lookup asks for. One listing is expected; the extra rows make a
/// duplicate-code answer visible rather than silent.
pub const PROFILE_QUERY_LIMIT: i64 = 5;

/// Whether a field takes the text operators rather than the numeric ones.
fn text_field(field: &str) -> bool {
    TEXT_FIELDS.contains(&field)
}

/// A 422 whose body names the offending filter and, where it applies, the supported list.
fn refuse(reason: &str, detail: impl Into<String>) -> MarketDataError {
    MarketDataError::new(reason, detail, 422)
}

/// The list a "that field is not one of the 14" refusal names.
fn field_list() -> String {
    SCREENER_FIELDS.join(", ")
}

/// Validate every filter as transport, before a vendor unit is spent.
///
/// The order is shape, field, operator, value, then the two vendor traps (a numeric `in` and a
/// trapped sector spelling). Each refusal names the filter's position, because a caller sending
/// twelve filters cannot act on "a field is wrong".
pub fn validate_filters(filters: &[Value]) -> Result<(), MarketDataError> {
    if filters.len() > MAX_FILTERS {
        return Err(refuse(
            "unsupported-filter",
            format!(
                "at most {MAX_FILTERS} filters per request, got {}",
                filters.len()
            ),
        ));
    }
    for (index, filter) in filters.iter().enumerate() {
        validate_filter(index, filter)?;
    }
    Ok(())
}

/// One filter, as `[field, operator, value]`.
fn validate_filter(index: usize, filter: &Value) -> Result<(), MarketDataError> {
    let Some(parts) = filter.as_array() else {
        return Err(refuse(
            "unsupported-filter",
            format!(
                "filters[{index}] is {}, not a [field, operator, value] triple",
                json_type(filter)
            ),
        ));
    };
    if parts.len() != 3 {
        return Err(refuse(
            "unsupported-filter",
            format!(
                "filters[{index}] has {} element(s); a filter is exactly [field, operator, value]",
                parts.len()
            ),
        ));
    }
    let Some(field) = parts[0].as_str().map(str::trim).filter(|f| !f.is_empty()) else {
        return Err(refuse(
            "unsupported-filter",
            format!("filters[{index}] names no field: {}", parts[0]),
        ));
    };
    let Some(operator) = parts[1].as_str().map(str::trim).filter(|o| !o.is_empty()) else {
        return Err(refuse(
            "unsupported-filter",
            format!("filters[{index}] names no operator: {}", parts[1]),
        ));
    };
    let value = &parts[2];

    check_field(field)?;
    check_operator(index, field, operator)?;
    check_value(index, operator, value)?;
    check_sector_trap(index, field, value)?;
    Ok(())
}

/// A field must be one of the 14; derived and rejected names get their own sentence.
fn check_field(field: &str) -> Result<(), MarketDataError> {
    if SCREENER_FIELDS.contains(&field) {
        return Ok(());
    }
    if DERIVED_FIELDS.contains(&field) {
        return Err(refuse(
            "derived-field",
            format!(
                "{} is computed locally, not a vendor field; read whole-market closes from \
                 POST /bars/bulk and compute it there",
                py_repr(field)
            ),
        ));
    }
    if let Some((_, guidance)) = REJECTED_FIELDS.iter().find(|(name, _)| *name == field) {
        return Err(refuse(
            "unknown-field",
            format!(
                "{} is not a screener field: {guidance}. The 14 fields are: {}",
                py_repr(field),
                field_list()
            ),
        ));
    }
    Err(refuse(
        "unknown-field",
        format!(
            "{} is not a screener field. The 14 fields are: {}",
            py_repr(field),
            field_list()
        ),
    ))
}

/// Numeric fields take the comparisons; text fields take `=` and `in`.
fn check_operator(index: usize, field: &str, operator: &str) -> Result<(), MarketDataError> {
    if text_field(field) {
        if TEXT_OPS.contains(&operator) {
            return Ok(());
        }
        return Err(refuse(
            "unsupported-operator",
            format!(
                "filters[{index}]: {field} is a text field, so it accepts {} only; {} is refused \
                 (the vendor answers HTTP 422 for a comparison on text)",
                TEXT_OPS.join(" and "),
                py_repr(operator)
            ),
        ));
    }
    if operator == "in" {
        return Err(refuse(
            "unsupported-operator",
            format!(
                "filters[{index}]: `in` is refused on the numeric field {field}; the vendor \
                 answers HTTP 500 for it. Send one comparison per call, or use `in` on a text \
                 field such as exchange"
            ),
        ));
    }
    if NUMERIC_OPS.contains(&operator) {
        return Ok(());
    }
    Err(refuse(
        "unsupported-operator",
        format!(
            "filters[{index}]: {field} is numeric, so it accepts {} only; {} is refused",
            NUMERIC_OPS.join(", "),
            py_repr(operator)
        ),
    ))
}

/// `in` takes an array; every other operator takes a scalar. `null` is neither.
fn check_value(index: usize, operator: &str, value: &Value) -> Result<(), MarketDataError> {
    if operator == "in" {
        let Some(items) = value.as_array() else {
            return Err(refuse(
                "unsupported-filter",
                format!(
                    "filters[{index}]: `in` needs an array value, got {}",
                    json_type(value)
                ),
            ));
        };
        if items.is_empty() {
            return Err(refuse(
                "unsupported-filter",
                format!("filters[{index}]: `in` needs at least one value"),
            ));
        }
        if items.iter().any(|item| item.is_null()) {
            return Err(refuse(
                "unsupported-filter",
                format!("filters[{index}]: `in` may not carry a null value"),
            ));
        }
        return Ok(());
    }
    if value.is_null() {
        return Err(refuse(
            "unsupported-filter",
            format!("filters[{index}]: {operator} needs a value, got null"),
        ));
    }
    if value.is_array() || value.is_object() {
        return Err(refuse(
            "unsupported-filter",
            format!(
                "filters[{index}]: {operator} needs a scalar value, got {}; only `in` takes a list",
                json_type(value)
            ),
        ));
    }
    Ok(())
}

/// A sector spelling EODHD does not hold reads as "nothing matched today", so it is refused
/// here with the spelling that works.
fn check_sector_trap(index: usize, field: &str, value: &Value) -> Result<(), MarketDataError> {
    if field != "sector" {
        return Ok(());
    }
    let candidates: Vec<&str> = match value {
        Value::String(text) => vec![text.as_str()],
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
        _ => return Ok(()),
    };
    for candidate in candidates {
        if let Some((_, replacement)) = SECTOR_TRAPS.iter().find(|(trap, _)| *trap == candidate) {
            return Err(refuse(
                "sector-trap",
                format!(
                    "filters[{index}]: {} is not a sector EODHD holds — it answers HTTP 200 with \
                     zero rows. Use {replacement}",
                    py_repr(candidate)
                ),
            ));
        }
    }
    Ok(())
}

/// `<field>.<asc|desc>`, or the refusal that says so.
///
/// The direction is required because the vendor's default is not documented, and a screen that
/// silently reverses is worse than one refused. The answer is canonical (lower-cased
/// direction), so the response echoes what was sent on the wire.
pub fn parse_sort(raw: Option<&str>) -> Result<Option<String>, MarketDataError> {
    let Some(raw) = raw.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(None);
    };
    let Some((field, direction)) = raw.split_once('.') else {
        return Err(invalid_sort(raw, "it needs a direction, as in `<field>.asc` or `<field>.desc`"));
    };
    let field = field.trim();
    let direction = direction.trim().to_lowercase();
    if direction != "asc" && direction != "desc" {
        return Err(invalid_sort(
            raw,
            &format!("{} is not a direction; use asc or desc", py_repr(&direction)),
        ));
    }
    if DERIVED_FIELDS.contains(&field) {
        return Err(refuse(
            "invalid-sort",
            format!(
                "{} is computed locally, not a vendor field, so it cannot be sorted on; read \
                 whole-market closes from POST /bars/bulk and sort there",
                py_repr(field)
            ),
        ));
    }
    if !SCREENER_FIELDS.contains(&field) {
        return Err(invalid_sort(
            raw,
            &format!(
                "{} is not a screener field. The 14 fields are: {}",
                py_repr(field),
                field_list()
            ),
        ));
    }
    Ok(Some(format!("{field}.{direction}")))
}

fn invalid_sort(raw: &str, why: &str) -> MarketDataError {
    refuse(
        "invalid-sort",
        format!(
            "{} is not a sort; {why}",
            py_repr(raw.trim())
        ),
    )
}

/// The filters exactly as they go on the wire: every agreeing `exchange` entry lifted, and one
/// `["exchange", "=", venue]` appended last.
///
/// The venue appended is the caller's, not the resolved market: `resolve_exchange("NASDAQ",
/// "US", …)` answers `US`, and appending that would collapse every per-venue US screen into the
/// same US-wide screen. Precedence: the requested top-level exchange, else the single agreeing
/// value the filters lifted, else the resolved market.
///
/// Resolving a broker venue code to a vendor value stays with the caller (the McTrader plan);
/// this function only refuses what contradicts the one market the request named.
pub fn screener_wire_filters(
    filters: &[Value],
    requested: Option<&str>,
    market: &str,
) -> Result<Vec<Value>, MarketDataError> {
    let mut lifted: Option<String> = None;
    let mut wire: Vec<Value> = Vec::with_capacity(filters.len() + 1);
    for filter in filters {
        if filter.as_array().and_then(|parts| parts.first()) != Some(&json!("exchange")) {
            wire.push(filter.clone());
            continue;
        }
        let parts = filter
            .as_array()
            .ok_or_else(|| refuse("unsupported-filter", "an exchange filter is not an array"))?;
        let operator = parts.get(1).and_then(Value::as_str).unwrap_or_default();
        let value = parts.get(2).cloned().unwrap_or(Value::Null);
        let venues = matching_venues(&value);
        let venue = agreeing_venue(&venues, market, operator)?;
        if lifted.is_none() {
            lifted = Some(venue);
        }
    }
    let venue = requested
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_uppercase)
        .or(lifted)
        .unwrap_or_else(|| market.to_string());
    wire.push(json!(["exchange", "=", venue]));
    Ok(wire)
}

/// The venue values an `exchange` filter names, upper-cased.
fn matching_venues(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => vec![text.trim().to_uppercase()],
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(|text| text.trim().to_uppercase())
            .collect(),
        _ => Vec::new(),
    }
}

/// The one venue a set of names agrees on, refusing a contradiction or a multi-venue `in`.
fn agreeing_venue(
    venues: &[String],
    market: &str,
    operator: &str,
) -> Result<String, MarketDataError> {
    let mut resolved: Vec<(String, String)> = Vec::with_capacity(venues.len());
    for venue in venues {
        resolved.push((venue.clone(), exchange_for(venue)?));
    }
    let distinct: std::collections::BTreeSet<&str> =
        resolved.iter().map(|(_, code)| code.as_str()).collect();
    if distinct.len() > 1 {
        let markets = distinct.into_iter().collect::<Vec<_>>().join(", ");
        let values = venues.join(", ");
        return Err(MarketDataError::bad_request(
            "mixed-exchanges",
            format!(
                "filters: `exchange {operator} [{values}]` names different markets ({markets}); \
                 one request is one market, so query one venue per call"
            ),
        ));
    }
    let Some((venue, code)) = resolved.first() else {
        return Err(MarketDataError::bad_request(
            "mixed-exchanges",
            "filters: an `exchange` filter names no venue; state one market at the top level",
        ));
    };
    if code != market {
        return Err(MarketDataError::bad_request(
            "exchange-conflict",
            format!(
                "filters: `exchange {operator} {venue}` is the {code} market, but the request \
                 names {market}; state one market"
            ),
        ));
    }
    Ok(venue.clone())
}

/// The two filters a profile lookup is pinned to.
///
/// `code` alone is not unique: EODHD keeps one row per listing, so `WDS` on its own answers
/// with Woodside's ASX and US records.
pub fn profile_filters(code: &str, venue: &str) -> Vec<Value> {
    vec![
        json!(["code", "=", code.trim().to_uppercase()]),
        json!(["exchange", "=", venue.trim().to_uppercase()]),
    ]
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(filters: Value) -> Result<(), MarketDataError> {
        let filters = filters.as_array().cloned().unwrap_or_default();
        validate_filters(&filters)
    }

    #[test]
    fn the_fourteen_fields_are_the_ones_the_vendor_accepts() {
        // Pinned against `SUPPORTED_FIELDS` in McTrader's eodhd_field_reference.py.
        let expected = [
            "code",
            "name",
            "exchange",
            "sub_exchange",
            "sector",
            "industry",
            "market_capitalization",
            "earnings_share",
            "dividend_yield",
            "adjusted_close",
            "avgvol_1d",
            "avgvol_200d",
            "refund_1d_p",
            "refund_5d_p",
        ];
        assert_eq!(SCREENER_FIELDS, expected);
        assert_eq!(SCREENER_FIELDS.len(), 14);
    }

    #[test]
    fn the_operator_tables_are_the_vendors() {
        assert_eq!(NUMERIC_OPS, ["=", "!=", ">", "<", ">=", "<="]);
        assert_eq!(TEXT_OPS, ["=", "in"]);
        assert!(!NUMERIC_OPS.contains(&"in"));
        assert_eq!(LIMIT_MAX, 500);
    }

    #[test]
    fn a_well_formed_screen_is_accepted() {
        assert!(validate(json!([
            ["market_capitalization", ">", 300_000_000],
            ["refund_5d_p", ">", 5],
            ["exchange", "=", "US"],
            ["code", "in", ["AAPL", "MSFT"]],
            ["sector", "=", "Technology"],
        ]))
        .is_ok());
        assert!(validate(json!([])).is_ok());
    }

    #[test]
    fn a_filter_must_be_a_three_element_triple() {
        let error = validate(json!([["code", "="]])).unwrap_err();
        assert_eq!(error.reason, "unsupported-filter");
        assert_eq!(error.status, 422);
        assert!(error.detail.contains("filters[0]"), "{error}");

        let error = validate(json!(["code"])).unwrap_err();
        assert_eq!(error.reason, "unsupported-filter");
        assert!(error.detail.contains("filters[0]"), "{error}");
    }

    #[test]
    fn an_unknown_field_names_itself_and_the_list() {
        let error = validate(json!([["market_cap", ">", 1]])).unwrap_err();
        assert_eq!(error.reason, "unknown-field");
        assert!(error.detail.contains("market_cap"), "{error}");
        for field in SCREENER_FIELDS {
            assert!(error.detail.contains(field), "{field} missing");
        }
    }

    #[test]
    fn a_rejected_field_is_refused_with_its_guidance() {
        let error = validate(json!([["country", "=", "US"]])).unwrap_err();
        assert_eq!(error.reason, "unknown-field");
        assert!(error.detail.contains("not a screener field"), "{error}");
        assert!(error.detail.contains("use `exchange`"), "{error}");

        let error = validate(json!([["pe_ttm", "<", 20]])).unwrap_err();
        assert_eq!(error.reason, "unknown-field");
        assert!(error.detail.contains("no P/E field"), "{error}");
    }

    #[test]
    fn a_derived_field_is_refused_with_the_bulk_workaround() {
        let error = validate(json!([["return_3m_p", ">", 5]])).unwrap_err();
        assert_eq!(error.reason, "derived-field");
        assert_eq!(error.status, 422);
        assert!(error.detail.contains("/bars/bulk"), "{error}");
        assert!(validate(json!([["return_200d_p", ">", 5]])).is_err());
        assert!(validate(json!([["return_100d_p", ">", 5]])).is_err());
    }

    #[test]
    fn in_on_a_numeric_field_is_refused_because_the_vendor_500s() {
        let error = validate(json!([["refund_5d_p", "in", [1, 2]]])).unwrap_err();
        assert_eq!(error.reason, "unsupported-operator");
        assert!(error.detail.contains("HTTP 500"), "{error}");
    }

    #[test]
    fn a_comparison_on_a_text_field_is_refused() {
        let error = validate(json!([["sector", ">", "Technology"]])).unwrap_err();
        assert_eq!(error.reason, "unsupported-operator");
        assert!(error.detail.contains("text field"), "{error}");

        let error = validate(json!([["code", "!=", "AAPL"]])).unwrap_err();
        assert_eq!(error.reason, "unsupported-operator");
    }

    #[test]
    fn in_needs_a_non_empty_array_and_everything_else_a_scalar() {
        assert!(validate(json!([["code", "in", ["AAPL"]]])).is_ok());

        let error = validate(json!([["code", "in", "AAPL"]])).unwrap_err();
        assert_eq!(error.reason, "unsupported-filter");
        assert!(error.detail.contains("array"), "{error}");

        assert!(validate(json!([["code", "in", []]])).is_err());
        assert!(validate(json!([["code", "in", [null]]])).is_err());
        assert!(validate(json!([["avgvol_1d", ">", [1]]])).is_err());
        assert!(validate(json!([["avgvol_1d", ">", null]])).is_err());
    }

    #[test]
    fn a_sector_trap_is_refused_with_its_replacement() {
        let error = validate(json!([["sector", "=", "Information Technology"]])).unwrap_err();
        assert_eq!(error.reason, "sector-trap");
        assert!(error.detail.contains("Technology"), "{error}");

        let error = validate(json!([["sector", "=", ""]])).unwrap_err();
        assert_eq!(error.reason, "sector-trap");

        // A spelling EODHD holds is not a trap.
        assert!(validate(json!([["sector", "=", "Technology"]])).is_ok());
        // Another field's value is not a sector.
        assert!(validate(json!([["industry", "=", "Health Care"]])).is_ok());
    }

    #[test]
    fn more_than_sixty_four_filters_is_refused() {
        let filters: Vec<Value> = (0..=MAX_FILTERS)
            .map(|_| json!(["adjusted_close", ">", 1]))
            .collect();
        let error = validate(Value::Array(filters)).unwrap_err();
        assert_eq!(error.reason, "unsupported-filter");
        assert!(error.detail.contains("64"), "{error}");
    }

    #[test]
    fn a_sort_is_field_direction_and_nothing_else() {
        assert_eq!(parse_sort(None).unwrap(), None);
        assert_eq!(parse_sort(Some("  ")).unwrap(), None);
        assert_eq!(
            parse_sort(Some("refund_5d_p.desc")).unwrap(),
            Some("refund_5d_p.desc".to_string())
        );
        assert_eq!(
            parse_sort(Some(" avgvol_200d.DESC ")).unwrap(),
            Some("avgvol_200d.desc".to_string())
        );
        assert_eq!(
            parse_sort(Some("code.asc")).unwrap(),
            Some("code.asc".to_string())
        );
    }

    #[test]
    fn a_malformed_or_derived_sort_is_refused() {
        let error = parse_sort(Some("refund_5d_p")).unwrap_err();
        assert_eq!(error.reason, "invalid-sort");
        assert!(error.detail.contains("<field>.asc"), "{error}");

        let error = parse_sort(Some("refund_5d_p.sideways")).unwrap_err();
        assert_eq!(error.reason, "invalid-sort");

        let error = parse_sort(Some("market_cap.desc")).unwrap_err();
        assert_eq!(error.reason, "invalid-sort");
        assert!(error.detail.contains("market_capitalization"), "{error}");

        let error = parse_sort(Some("return_3m_p.desc")).unwrap_err();
        assert_eq!(error.reason, "invalid-sort");
        assert!(error.detail.contains("/bars/bulk"), "{error}");
    }

    #[test]
    fn the_venue_is_appended_once_and_last() {
        let filters = vec![
            json!(["market_capitalization", ">", 1]),
            json!(["exchange", "=", "NASDAQ"]),
        ];
        let wire = screener_wire_filters(&filters, Some("NASDAQ"), "US").unwrap();
        assert_eq!(wire.len(), 2);
        assert_eq!(wire[1], json!(["exchange", "=", "NASDAQ"]));
        assert_eq!(
            wire.iter()
                .filter(|filter| filter[0] == json!("exchange"))
                .count(),
            1
        );
    }

    #[test]
    fn the_lifted_filter_value_is_the_venue_when_no_top_level_one_was_sent() {
        let filters = vec![json!(["exchange", "=", "NASDAQ"])];
        let wire = screener_wire_filters(&filters, None, "US").unwrap();
        assert_eq!(wire, vec![json!(["exchange", "=", "NASDAQ"])]);
    }

    #[test]
    fn the_resolved_market_is_the_fallback_venue() {
        let wire = screener_wire_filters(&[], None, "AU").unwrap();
        assert_eq!(wire, vec![json!(["exchange", "=", "AU"])]);
    }

    #[test]
    fn the_requested_venue_is_upper_cased_and_the_filters_value_is_lifted() {
        let filters = vec![json!(["exchange", "=", "asx"])];
        let wire = screener_wire_filters(&filters, Some("asx"), "AU").unwrap();
        assert_eq!(wire, vec![json!(["exchange", "=", "ASX"])]);
    }

    #[test]
    fn an_exchange_filter_naming_another_market_is_refused() {
        let filters = vec![json!(["exchange", "=", "AU"])];
        let error = screener_wire_filters(&filters, None, "US").unwrap_err();
        assert_eq!(error.reason, "exchange-conflict");
        assert_eq!(error.status, 400);
        assert!(error.detail.contains("AU"), "{error}");
        assert!(error.detail.contains("US"), "{error}");
    }

    #[test]
    fn an_in_across_two_markets_is_refused() {
        let filters = vec![json!(["exchange", "in", ["US", "AU"]])];
        let error = screener_wire_filters(&filters, None, "US").unwrap_err();
        assert_eq!(error.reason, "mixed-exchanges");
        assert_eq!(error.status, 400);
        assert!(error.detail.contains("AU"), "{error}");
        assert!(error.detail.contains("US"), "{error}");
    }

    #[test]
    fn an_in_naming_one_market_is_lifted_to_the_single_venue() {
        let filters = vec![json!(["exchange", "in", ["NASDAQ"]])];
        let wire = screener_wire_filters(&filters, None, "US").unwrap();
        assert_eq!(wire, vec![json!(["exchange", "=", "NASDAQ"])]);
    }

    #[test]
    fn a_venue_outside_the_alias_table_is_unknown() {
        let filters = vec![json!(["exchange", "=", "NEO"])];
        let error = screener_wire_filters(&filters, None, "US").unwrap_err();
        assert_eq!(error.reason, "unknown-exchange");
        assert_eq!(error.status, 400);
    }

    #[test]
    fn a_profile_is_pinned_to_the_code_and_the_venue() {
        assert_eq!(
            profile_filters(" wds ", "au"),
            vec![json!(["code", "=", "WDS"]), json!(["exchange", "=", "AU"])]
        );
        assert_eq!(PROFILE_QUERY_LIMIT, 5);
    }
}
