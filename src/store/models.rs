//! The rows the store reads and writes.
//!
//! These types are deliberately plain data: no SQL, no connections, no policy. A repository
//! turns database rows into them, and the products layer never sees a `tiberius::Row`.
//!
//! Nullability mirrors the schema rather than flattening it. `decimal(18,6)` columns are
//! `Option<f64>` even where the table says `NOT NULL`, because the source service read every
//! decimal through a helper that could return `None`; a caller can therefore tell "the
//! database contradicted its own schema" from "the value was zero".

use std::collections::BTreeMap;

use chrono::{NaiveDate, NaiveDateTime};

/// A cached price row, as `get_prices` returns it.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedPrice {
    pub symbol: String,
    pub venue: String,
    pub price: Option<f64>,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub previous_close: Option<f64>,
    pub change_pct: Option<f64>,
    pub as_of: Option<NaiveDateTime>,
    pub fetched_at: Option<NaiveDateTime>,
    pub provider: Option<String>,
}

/// A price row to upsert. `venue` is optional only so the store can refuse it explicitly:
/// the source service required a venue on write rather than assuming US.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceRow {
    pub symbol: String,
    pub venue: Option<String>,
    pub price: f64,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub previous_close: Option<f64>,
    pub change_pct: Option<f64>,
    pub volume: Option<i64>,
    pub as_of: Option<NaiveDateTime>,
    pub fetched_at: Option<NaiveDateTime>,
    pub provider: Option<String>,
}

/// One completed daily bar.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredBar {
    pub session: NaiveDate,
    pub open: Option<f64>,
    pub high: Option<f64>,
    pub low: Option<f64>,
    pub close: Option<f64>,
    pub volume: Option<i64>,
    pub source: Option<String>,
    pub adjusted: bool,
    pub raw_close: Option<f64>,
}

/// One daily series: a ticker at one venue.
///
/// The pair is the cache key for bars and ATR, because `CBA` at the ASX and `CBA` in New York
/// are different instruments with different histories.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SeriesKey {
    pub symbol: String,
    pub exchange: String,
}

/// One daily bar to upsert.
#[derive(Debug, Clone, PartialEq)]
pub struct DailyBarWrite {
    pub symbol: String,
    pub exchange: Option<String>,
    pub session: NaiveDate,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub raw_close: Option<f64>,
    pub volume: Option<i64>,
    pub source: Option<String>,
    /// `None` means the writer did not state it; the source service defaulted that to
    /// adjusted, while an explicit false is preserved.
    pub adjusted: Option<bool>,
}

/// One session aggregated from the `BarHourly` warehouse table.
///
/// The warehouse's own column types are nullable and decimal; they are carried as options so
/// a half-written row is refused by the caller rather than failing the whole read.
#[derive(Debug, Clone, PartialEq)]
pub struct WarehouseSession {
    pub symbol: String,
    pub session: NaiveDate,
    pub open: Option<f64>,
    pub high: Option<f64>,
    pub low: Option<f64>,
    pub close: Option<f64>,
    pub volume: Option<i64>,
}

/// One cached indicator row.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredIndicator {
    pub symbol: String,
    /// ATR values keyed by period. Only the five schema periods are populated.
    pub values: BTreeMap<i64, Option<f64>>,
    pub reference_price: Option<f64>,
    pub sessions_used: Option<i32>,
    pub as_of_session: Option<NaiveDate>,
    pub method: Option<String>,
    pub smoothing: Option<String>,
    pub updated_at: Option<NaiveDateTime>,
}

impl StoredIndicator {
    /// The ATR for one period, if the row carries it.
    pub fn value(&self, period: i64) -> Option<f64> {
        self.values.get(&period).copied().flatten()
    }
}

/// One indicator row to upsert.
#[derive(Debug, Clone, PartialEq)]
pub struct IndicatorWrite {
    pub symbol: String,
    pub exchange: Option<String>,
    pub values: BTreeMap<i64, Option<f64>>,
    pub reference_price: Option<f64>,
    pub sessions_used: Option<i32>,
    pub as_of_session: Option<NaiveDate>,
    pub method: Option<String>,
    pub smoothing: Option<String>,
}

impl IndicatorWrite {
    pub fn value(&self, period: i64) -> Option<f64> {
        self.values.get(&period).copied().flatten()
    }
}

/// One row of the winners or losers list.
#[derive(Debug, Clone, PartialEq)]
pub struct Mover {
    pub symbol: String,
    pub price: f64,
    pub change_pct: f64,
    pub volume: i64,
    pub previous_close: Option<f64>,
    pub as_of_utc: Option<NaiveDateTime>,
    pub source: Option<String>,
    pub dollar_volume: Option<f64>,
}

/// The two ranked lists `big_movers` returns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Movers {
    pub winners: Vec<Mover>,
    pub losers: Vec<Mover>,
}

/// Symbols landed per recent session, per exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSymbolCount {
    pub session: NaiveDate,
    pub exchange: String,
    pub symbols: i64,
}

/// One row of `dbo.Exchanges`, as the market clock reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeRow {
    pub id: i32,
    pub code: Option<String>,
    pub name: Option<String>,
    pub country: Option<String>,
    pub currency: Option<String>,
    pub timezone: Option<String>,
    pub opens: Option<String>,
    pub closes: Option<String>,
}

/// One request type's share of the day's spend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageByRequestType {
    pub request_type: String,
    pub calls: i64,
    pub symbols: i64,
    pub cost: i64,
}

/// One caller's share of the day's spend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageByCaller {
    pub caller: String,
    pub calls: i64,
    pub cost: i64,
}

/// The day's spend and the breakdowns that explain it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageSummary {
    pub date: NaiveDate,
    pub calls: i64,
    pub symbols: i64,
    pub cost: i64,
    pub rows: i64,
    pub by_request_type: Vec<UsageByRequestType>,
    pub by_caller: Vec<UsageByCaller>,
}

/// Row counts for `/stats`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreStats {
    pub price_rows: i64,
    pub daily_rows: i64,
    pub daily_symbols: i64,
    pub indicator_rows: i64,
    pub latest_session: Option<NaiveDate>,
    pub latest_price_at: Option<NaiveDateTime>,
    pub daily_by_exchange: Vec<ExchangeBarCount>,
}

/// Bars landed per exchange, for `/stats`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeBarCount {
    pub exchange: String,
    pub bars: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn indicator(values: &[(i64, Option<f64>)]) -> StoredIndicator {
        StoredIndicator {
            symbol: "AAPL".into(),
            values: values.iter().copied().collect(),
            reference_price: None,
            sessions_used: None,
            as_of_session: None,
            method: None,
            smoothing: None,
            updated_at: None,
        }
    }

    #[test]
    fn a_series_key_is_the_symbol_and_the_venue_together() {
        let us = SeriesKey {
            symbol: "CBA".into(),
            exchange: "US".into(),
        };
        let au = SeriesKey {
            symbol: "CBA".into(),
            exchange: "AU".into(),
        };
        assert_ne!(us, au);
        assert_eq!(us, us.clone());
    }

    #[test]
    fn a_missing_period_and_a_null_period_are_both_none() {
        let row = indicator(&[(14, Some(1.5)), (20, None)]);
        assert_eq!(row.value(14), Some(1.5));
        assert_eq!(row.value(20), None);
        assert_eq!(row.value(50), None);
    }

    #[test]
    fn an_indicator_write_reads_the_same_way() {
        let write = IndicatorWrite {
            symbol: "AAPL".into(),
            exchange: None,
            values: [(14, Some(2.0))].into_iter().collect(),
            reference_price: None,
            sessions_used: None,
            as_of_session: None,
            method: None,
            smoothing: None,
        };
        assert_eq!(write.value(14), Some(2.0));
        assert_eq!(write.value(20), None);
    }
}
