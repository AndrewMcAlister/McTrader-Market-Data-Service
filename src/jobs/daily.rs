//! One bulk pull into `DailySnapshot`, then rebuild `IndicatorCache`.
//!
//! # What the pull costs, and why it is shaped this way
//!
//! `/eod-bulk-last-day` is 100 units flat and answers with both `close` and `adjusted_close`.
//! The adjusted close is what is stored as `Close`, because it matches `BarHourly`'s
//! convention and avoiding it is how an ATR learns to read a split as a catastrophic move.
//! The raw close is kept in its own column: it is the price that printed, which structure and
//! orders read.
//!
//! # The delisted-listings trap
//!
//! A dated pull answers with ~50,270 codes including listings that have since stopped trading,
//! against ~27,714 for the current session. Nothing is filtered out here, deliberately: a
//! delisted instrument's bars *are* real history for the sessions it traded, and its last
//! session is visible in `DailySnapshot`, so a stale ATR is detectable. The trap belongs to
//! the reader — never compute a market-wide statistic from a dated pull without intersecting
//! with a live listing set — not to the loader.
//!
//! # The session date comes from the rows
//!
//! A bulk call for a date the market was shut answers with the previous session. Filing that
//! under the requested date would silently invent a duplicate bar, so the date is always the
//! newest one the vendor actually returned.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::NaiveDate;
use serde::Serialize;
use serde_json::Value;

use crate::config::Settings;
use crate::domain::atr;
use crate::providers::base::{number, text};
use crate::providers::eodhd_client::{bare_symbol, normalise_exchange};
use crate::refresh::{RefreshFailure, RefreshJobs, RefreshStore, RefreshVendor};
use crate::store::keys::required_exchange_key;
use crate::store::models::{DailyBarWrite, IndicatorWrite, SeriesKey, StoredBar};
use crate::store::{DailyBarRepository, IndicatorRepository};

/// The source stamped on rows this job writes.
pub const BULK_SOURCE: &str = "eod-bulk";

/// Symbols per indicator read, from the source service: one query per batch, sized well under
/// the parameter ceiling because each symbol is one parameter.
pub const REBUILD_CHUNK: usize = 200;

/// What one session's pull produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionSummary {
    pub session: String,
    pub received: usize,
    pub written: usize,
    pub rows_without_adjusted_close: usize,
}

/// What a pull over one or more sessions produced.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PullSummary {
    pub sessions: Vec<SessionSummary>,
    pub rows_written: usize,
}

/// What an indicator rebuild produced.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RebuildSummary {
    pub symbols_considered: usize,
    pub indicators_written: usize,
}

/// One bulk row as a `DailySnapshot` row, preferring `adjusted_close`.
///
/// `market` has no default: this is the function that stamps the exchange, and a default here
/// would file an ASX pull as US without anyone deciding to.
pub fn row_from_bulk(
    raw: &Value,
    session_date: NaiveDate,
    market: &str,
    symbol: Option<&str>,
) -> Option<DailyBarWrite> {
    let code = match symbol {
        Some(symbol) if !symbol.is_empty() => symbol.to_string(),
        _ => bare_symbol(&raw.get("code").and_then(text).unwrap_or_default()),
    };
    if code.is_empty() {
        return None;
    }

    let adjusted_close = field(raw, "adjusted_close");
    let raw_close = field(raw, "close");
    let close = adjusted_close.or(raw_close)?;
    let high = field(raw, "high")?;
    let low = field(raw, "low")?;
    let open = field(raw, "open")?;
    if close <= 0.0 || high <= 0.0 || low <= 0.0 {
        return None;
    }

    Some(DailyBarWrite {
        symbol: code,
        // The exchange is stamped on the row rather than assumed at read time: the ASX and the
        // US both file 2026-10-30, and a symbol-only key is how one overwrote the other.
        exchange: required_exchange_key(Some(market)).ok(),
        session: session_date,
        open,
        high,
        low,
        close,
        // The bulk feed carries both closes. Close stays the adjusted one for series
        // continuity; RawClose is the price that printed.
        raw_close,
        volume: Some(field(raw, "volume").map(|value| value as i64).unwrap_or(0)),
        source: Some(BULK_SOURCE.to_string()),
        // True only when adjusted_close was present: claiming adjustment for a raw close would
        // let a split through as a real move.
        adjusted: Some(adjusted_close.is_some()),
    })
}

/// A numeric field of a bulk row, treating absent and unparseable as absent.
fn field(raw: &Value, name: &str) -> Option<f64> {
    raw.get(name).and_then(number)
}

/// Pull one or more sessions of whole-market daily bars into `DailySnapshot`.
///
/// An explicit date that the exchange did not trade resolves to the session the vendor actually
/// answered with, and the mismatch is logged rather than hidden.
pub async fn pull_bulk<S>(
    store: &S,
    vendor: &dyn RefreshVendor,
    market: &str,
    dates: Option<Vec<NaiveDate>>,
) -> Result<PullSummary, RefreshFailure>
where
    S: DailyBarRepository + ?Sized,
{
    // Normalised once, here: "ASX" and "AU" are the same exchange, an unknown one is refused
    // before a unit is spent, and the value written to the row is the one reads filter on.
    let market = normalise_exchange(Some(market))?;
    let wanted_dates: Vec<Option<NaiveDate>> = match dates {
        Some(dates) => dates.into_iter().map(Some).collect(),
        None => vec![None],
    };

    let mut summary = PullSummary::default();
    for wanted in wanted_dates {
        let rows = vendor.bulk_last_day(&market, wanted).await?;
        if rows.is_empty() {
            tracing::warn!(
                "bulk pull for {} returned nothing",
                wanted.map_or_else(|| "latest".to_string(), |date| date.to_string())
            );
            continue;
        }

        let Some(resolved) = resolved_session(&rows)? else {
            tracing::warn!(
                "bulk pull for {} carried no dates",
                wanted.map_or_else(|| "latest".to_string(), |date| date.to_string())
            );
            continue;
        };
        if let Some(wanted) = wanted {
            if wanted != resolved {
                tracing::warn!(
                    "asked for {wanted}, vendor answered with {resolved} (not a trading day)"
                );
            }
        }

        let mut payload = Vec::with_capacity(rows.len());
        let mut unadjusted = 0usize;
        for raw in &rows {
            let symbol = bare_symbol(&raw.get("code").and_then(text).unwrap_or_default());
            let Some(row) = row_from_bulk(raw, resolved, &market, Some(&symbol)) else {
                continue;
            };
            if row.adjusted == Some(false) {
                unadjusted += 1;
            }
            payload.push(row);
        }

        let written = store.upsert_daily(payload).await?;
        tracing::info!(
            "{market} session {resolved}: {} received, {written} written, \
             {unadjusted} without adjusted_close",
            rows.len()
        );
        summary.sessions.push(SessionSummary {
            session: resolved.to_string(),
            received: rows.len(),
            written,
            rows_without_adjusted_close: unadjusted,
        });
        summary.rows_written += written;
    }
    Ok(summary)
}

/// The newest date the vendor actually returned, or `None` when every row omitted one.
///
/// The date is read as text and compared as text first, which is what the source service did;
/// ISO-8601 dates sort chronologically. A newest date that is not a date is a vendor
/// contradiction and is refused rather than skipped.
///
/// Public because `POST /bars/bulk` must file a pull under the session the vendor answered
/// with, not the session that was asked for — one rule, one implementation.
pub fn resolved_session(rows: &[Value]) -> Result<Option<NaiveDate>, RefreshFailure> {
    let newest = rows
        .iter()
        .filter_map(|row| text(row.get("date")?))
        .map(|date| date.chars().take(10).collect::<String>())
        .filter(|date| !date.is_empty())
        .max();
    let Some(newest) = newest else {
        return Ok(None);
    };
    NaiveDate::parse_from_str(&newest, "%Y-%m-%d")
        .map(Some)
        .map_err(|_| {
            RefreshFailure::Invalid(format!("bulk row date {newest:?} is not an ISO-8601 date"))
        })
}

/// Recompute `IndicatorCache` for every series with enough daily history.
///
/// The work list is the store's, so the `HAVING COUNT(*)` predicate is stated once beside the
/// table it reads rather than in this job.
pub async fn rebuild_indicators<S>(
    store: &S,
    settings: &Settings,
) -> Result<RebuildSummary, RefreshFailure>
where
    S: DailyBarRepository + IndicatorRepository + ?Sized,
{
    let periods = settings.atr_periods.clone();
    let longest = periods.iter().copied().max().unwrap_or(0);
    let need = settings.min_sessions.max(longest + 1);

    let series = store.symbols_with_min_sessions(need).await?;
    tracing::info!(
        "{} symbol(s) have >= {need} sessions for ATR({longest})",
        series.len()
    );

    // Per exchange: a series must not mix an ASX bar with a US one, and the same ticker can
    // exist on both. The store returns the work list ordered by exchange then symbol.
    let mut by_exchange: BTreeMap<&str, Vec<&SeriesKey>> = BTreeMap::new();
    for key in &series {
        by_exchange
            .entry(key.exchange.as_str())
            .or_default()
            .push(key);
    }

    let mut written = 0usize;
    for (exchange, keys) in &by_exchange {
        let count = rebuild_for(store, settings, keys, exchange, &periods, need).await?;
        written += count;
        tracing::info!("  {exchange}: {count} indicator row(s)");
    }
    tracing::info!("IndicatorCache rebuilt for {written} symbol(s)");
    Ok(RebuildSummary {
        symbols_considered: series.len(),
        indicators_written: written,
    })
}

/// Rebuild one exchange's series, one parameter-bounded batch at a time.
async fn rebuild_for<S>(
    store: &S,
    settings: &Settings,
    keys: &[&SeriesKey],
    exchange: &str,
    periods: &[i64],
    need: i64,
) -> Result<usize, RefreshFailure>
where
    S: DailyBarRepository + IndicatorRepository + ?Sized,
{
    let mut written = 0usize;
    for batch in keys.chunks(REBUILD_CHUNK) {
        let symbols: Vec<String> = batch.iter().map(|key| key.symbol.clone()).collect();
        let bars_by_symbol = store
            .get_daily_bars_many(
                &symbols,
                periods.iter().copied().max().unwrap_or(0) + 1,
                exchange,
            )
            .await?;
        let mut rows = Vec::new();
        for (symbol, bars) in bars_by_symbol {
            if let Some(row) = indicator_row(&symbol, exchange, &bars, periods, need, settings)? {
                rows.push(row);
            }
        }
        if !rows.is_empty() {
            written += store.upsert_indicators(rows).await?;
        }
    }
    Ok(written)
}

/// One symbol's indicator row, or `None` when it cannot produce the first requested period.
fn indicator_row(
    symbol: &str,
    exchange: &str,
    bars: &[StoredBar],
    periods: &[i64],
    need: i64,
    settings: &Settings,
) -> Result<Option<IndicatorWrite>, RefreshFailure> {
    if (bars.len() as i64) < need {
        return Ok(None);
    }
    let Some((highs, lows, closes)) = ohlc(bars) else {
        tracing::warn!("{exchange}/{symbol}: a bar had no OHLC value; skipping its ATR");
        return Ok(None);
    };

    let mut values = BTreeMap::new();
    let mut first = None;
    for (index, period) in periods.iter().enumerate() {
        let value = atr::average_true_range(
            &highs,
            &lows,
            &closes,
            *period,
            &settings.atr_method,
            &settings.atr_smoothing,
        )
        .map_err(|error| RefreshFailure::Invalid(error.message))?;
        if index == 0 {
            first = value;
        }
        values.insert(*period, value);
    }
    // The first requested period is the gate the source service used: a row whose headline
    // ATR is unknown would be a cache entry that answers nothing.
    if first.is_none() {
        return Ok(None);
    }

    Ok(Some(IndicatorWrite {
        symbol: symbol.to_string(),
        exchange: Some(exchange.to_string()),
        values,
        reference_price: closes.last().copied(),
        sessions_used: i32::try_from(bars.len()).ok(),
        as_of_session: bars.last().map(|bar| bar.session),
        method: Some(settings.atr_method.clone()),
        smoothing: Some(settings.atr_smoothing.clone()),
    }))
}

/// The three aligned series ATR needs, or `None` when a stored bar is missing one.
fn ohlc(bars: &[StoredBar]) -> Option<(Vec<f64>, Vec<f64>, Vec<f64>)> {
    let highs = bars
        .iter()
        .map(|bar| bar.high)
        .collect::<Option<Vec<_>>>()?;
    let lows = bars.iter().map(|bar| bar.low).collect::<Option<Vec<_>>>()?;
    let closes = bars
        .iter()
        .map(|bar| bar.close)
        .collect::<Option<Vec<_>>>()?;
    Some((highs, lows, closes))
}

/// The production [`RefreshJobs`]: the functions above, behind the policy's trait.
#[derive(Debug, Clone, Copy, Default)]
pub struct LibraryJobs;

#[async_trait]
impl RefreshJobs for LibraryJobs {
    async fn pull(
        &self,
        store: &dyn RefreshStore,
        vendor: &dyn RefreshVendor,
        market: &str,
        dates: Option<Vec<NaiveDate>>,
    ) -> Result<PullSummary, RefreshFailure> {
        pull_bulk(store, vendor, market, dates).await
    }

    async fn rebuild_indicators(
        &self,
        store: &dyn RefreshStore,
        settings: &Settings,
    ) -> Result<RebuildSummary, RefreshFailure> {
        rebuild_indicators(store, settings).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 3).unwrap()
    }

    fn bulk_row() -> Value {
        json!({
            "code": "AAPL.US",
            "date": "2026-11-03",
            "open": 270.1,
            "high": 272.5,
            "low": 268.9,
            "close": 271.0,
            "adjusted_close": 270.5,
            "volume": 51_000_000,
        })
    }

    #[test]
    fn a_bulk_row_prefers_the_adjusted_close_and_keeps_the_raw_one() {
        let row = row_from_bulk(&bulk_row(), session(), "US", None).unwrap();
        assert_eq!(row.symbol, "AAPL");
        assert_eq!(row.close, 270.5);
        assert_eq!(row.raw_close, Some(271.0));
        assert_eq!(row.adjusted, Some(true));
        assert_eq!(row.volume, Some(51_000_000));
        assert_eq!(row.exchange.as_deref(), Some("US"));
        assert_eq!(row.source.as_deref(), Some(BULK_SOURCE));
    }

    #[test]
    fn a_row_without_an_adjusted_close_is_marked_unadjusted() {
        let mut raw = bulk_row();
        raw.as_object_mut().unwrap().remove("adjusted_close");
        let row = row_from_bulk(&raw, session(), "US", None).unwrap();
        assert_eq!(row.close, 271.0);
        assert_eq!(row.adjusted, Some(false));
    }

    #[test]
    fn a_row_with_no_usable_price_is_dropped() {
        let mut raw = bulk_row();
        raw["close"] = json!(0.0);
        raw.as_object_mut().unwrap().remove("adjusted_close");
        assert!(row_from_bulk(&raw, session(), "US", None).is_none());

        let mut raw = bulk_row();
        raw.as_object_mut().unwrap().remove("high");
        assert!(row_from_bulk(&raw, session(), "US", None).is_none());
    }

    #[test]
    fn a_named_symbol_wins_over_the_vendor_code() {
        let row = row_from_bulk(&bulk_row(), session(), "AU", Some("BHP")).unwrap();
        assert_eq!(row.symbol, "BHP");
        assert_eq!(row.exchange.as_deref(), Some("AU"));
    }

    #[test]
    fn an_empty_code_is_dropped_rather_than_written_under_a_blank_symbol() {
        let mut raw = bulk_row();
        raw["code"] = json!("");
        assert!(row_from_bulk(&raw, session(), "US", None).is_none());
    }

    #[test]
    fn the_newest_date_in_the_response_is_the_session() {
        let rows = vec![
            json!({"date": "2026-11-02T00:00:00"}),
            json!({"date": "2026-11-03T00:00:00"}),
            json!({"date": "2026-11-01"}),
        ];
        assert_eq!(
            resolved_session(&rows).unwrap(),
            Some(NaiveDate::from_ymd_opt(2026, 11, 3).unwrap())
        );
    }

    #[test]
    fn a_response_with_no_dates_has_no_session() {
        assert_eq!(resolved_session(&[json!({"code": "AAPL"})]).unwrap(), None);
    }

    #[test]
    fn a_malformed_newest_date_is_refused() {
        let error = resolved_session(&[json!({"date": "not-a-date"})]).unwrap_err();
        assert!(matches!(error, RefreshFailure::Invalid(_)));
    }
}
