//! The two free backfills: history from `BarHourly`, raw closes from the 5-minute tape.
//!
//! # Why these exist instead of asking the vendor
//!
//! ATR needs roughly fifty sessions before it can answer. Paying `/eod/` for each of ~27,700
//! listings to get there is 27,700 units; `dbo.BarHourly` already holds the same history,
//! split-adjusted, and aggregating it costs nothing. The same argument applies to
//! `RawClose`: the unadjusted 5-minute tape carries the price that printed, so the last
//! traded bar of a session *is* the raw close the vendor would return.
//!
//! # What each function may write
//!
//! `seed_from_barhourly` writes whole sessions with `source='barhourly'` and no raw close;
//! `fill_raw_close` writes only `DailySnapshot.RawClose` and only where it is still NULL.
//! Neither touches a price the vendor supplied.
//!
//! # Why the padding matters
//!
//! A gap in the daily series is worse than a missing session: ATR's true range would span the
//! gap and read it as one enormous move. That is why the command expands `--since` into every
//! weekday and why these backfills are re-runnable rather than one-shot.

use std::collections::BTreeSet;

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use serde::Serialize;

use crate::domain::sessions::latest_completed_session;
use crate::providers::eodhd_client::normalise_exchange;
use crate::refresh::RefreshFailure;
use crate::store::keys::required_exchange_key;
use crate::store::models::{DailyBarWrite, WarehouseSession};
use crate::store::{DailyBarRepository, WarehouseRepository};

/// The source stamped on rows seeded from the warehouse.
pub const SEED_SOURCE: &str = "barhourly";

/// Rows per upsert, from the source job: one transaction per 20,000-row batch, so the write
/// lock is held for seconds rather than for the whole seed.
pub const SEED_CHUNK: usize = 20_000;

/// Symbols per raw-close statement. Three more parameters ride along (exchange and the two
/// dates), well under SQL Server's 2,100-parameter ceiling.
pub const RAW_CLOSE_CHUNK: usize = 500;

/// What a seed wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SeedSummary {
    pub rows_written: usize,
    pub since: NaiveDate,
}

/// What a raw-close fill wrote, or why it did nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RawCloseSummary {
    pub updated: u64,
    pub symbols: usize,
    pub since: NaiveDate,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<NaiveDate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

/// Aggregate `BarHourly` into `DailySnapshot`. Costs no vendor units.
///
/// `today` is injected so the window is a parameter rather than the wall clock, and `market`
/// is normalised once here: the warehouse is the US tape today, but the row's exchange is
/// stamped from the market the caller named rather than assumed.
pub async fn seed_from_barhourly<S>(
    store: &S,
    market: &str,
    days: i64,
    today: NaiveDate,
    chunk: usize,
) -> Result<SeedSummary, RefreshFailure>
where
    S: WarehouseRepository + DailyBarRepository + ?Sized,
{
    let market = normalise_exchange(Some(market))?;
    let exchange = required_exchange_key(Some(&market))?;
    let since = days_before(today, days)?;

    let sessions = store.barhourly_sessions(since).await?;
    tracing::info!(
        "BarHourly aggregated {} symbol-sessions since {since} ({exchange})",
        sessions.len()
    );

    let payload: Vec<DailyBarWrite> = sessions
        .iter()
        .filter_map(|session| seed_row(session, &exchange))
        .collect();
    let mut written = 0usize;
    for batch in payload.chunks(chunk.max(1)) {
        written += store.upsert_daily(batch.to_vec()).await?;
        tracing::info!("  written {written}/{}", payload.len());
    }

    Ok(SeedSummary {
        rows_written: written,
        since,
    })
}

/// One warehouse session as a `DailySnapshot` row, or `None` when it cannot be served.
///
/// `RawClose` is deliberately absent: `BarHourly` is split-adjusted only, so writing its
/// close there would erase the distinction the column exists to keep.
pub fn seed_row(session: &WarehouseSession, exchange: &str) -> Option<DailyBarWrite> {
    let symbol = session.symbol.trim().to_uppercase();
    if symbol.is_empty() {
        return None;
    }
    let open = session.open?;
    let high = session.high?;
    let low = session.low?;
    let close = session.close?;
    // The source checked only the close: a zero high or low is odd but real history, while a
    // zero close is a missing price that would poison every following true range.
    if close <= 0.0 {
        return None;
    }
    Some(DailyBarWrite {
        symbol,
        exchange: Some(exchange.to_string()),
        session: session.session,
        open,
        high,
        low,
        close,
        raw_close: None,
        volume: Some(session.volume.unwrap_or(0)),
        source: Some(SEED_SOURCE.to_string()),
        // BarHourly is split-adjusted, so this is not a claim about a raw close.
        adjusted: Some(true),
    })
}

/// Fill `DailySnapshot.RawClose` from the unadjusted 5-minute tape. Costs no vendor units.
///
/// `until` is the latest completed session the exchange's own clock allows; `days` reaches back
/// from there. Rows already carrying a raw close are never overwritten.
pub async fn fill_raw_close<S>(
    store: &S,
    market: &str,
    days: i64,
    now: DateTime<Utc>,
) -> Result<RawCloseSummary, RefreshFailure>
where
    S: WarehouseRepository + ?Sized,
{
    let market = normalise_exchange(Some(market))?;
    let exchange = required_exchange_key(Some(&market))?;
    let until = latest_completed_session(Some(&market), now);
    let since = days_before(until, days.max(1))?;

    let Some(tape) = store.five_minute_tape().await? else {
        tracing::warn!(
            "no 5-minute tape (Bar5Min or BarReal5Min) in this database; nothing to fill from"
        );
        return Ok(RawCloseSummary {
            updated: 0,
            symbols: 0,
            since,
            until: None,
            table: None,
            skipped: Some("no-5min-tape".to_string()),
        });
    };

    let symbols = store.raw_close_candidates(&exchange, since, until).await?;
    tracing::info!(
        "{} symbol(s) with a missing raw close since {since} ({exchange}), filling from dbo.{}",
        symbols.len(),
        tape.table_name()
    );

    let mut updated = 0u64;
    for (index, batch) in symbols.chunks(RAW_CLOSE_CHUNK).enumerate() {
        updated += store
            .fill_raw_close(&exchange, tape, since, until, batch)
            .await?;
        let done = ((index + 1) * RAW_CLOSE_CHUNK).min(symbols.len());
        tracing::info!(
            "  raw close filled {done}/{} symbols, {updated} row(s)",
            symbols.len()
        );
    }

    Ok(RawCloseSummary {
        updated,
        symbols: symbols.len(),
        since,
        until: Some(until),
        table: Some(tape.table_name().to_string()),
        skipped: None,
    })
}

/// Every weekday from `since` through `today`, inclusive.
///
/// Weekends are skipped because a dated bulk call for a shut market answers with the previous
/// session: asking for it would spend 100 units to re-file rows already stored. The source
/// job expanded `--since` exactly this way.
pub fn weekday_dates(since: NaiveDate, today: NaiveDate) -> Vec<NaiveDate> {
    let mut dates = Vec::new();
    let mut cursor = since;
    while cursor <= today {
        if cursor.weekday().num_days_from_monday() < 5 {
            dates.push(cursor);
        }
        match cursor.succ_opt() {
            Some(next) => cursor = next,
            None => break,
        }
    }
    dates
}

/// Remove any duplicate dates a `--since` expansion would otherwise repeat, preserving order.
///
/// `--date` and `--since` can name the same session, and each repeat is another 100-unit call.
pub fn distinct_dates(dates: impl IntoIterator<Item = NaiveDate>) -> Vec<NaiveDate> {
    let mut seen = BTreeSet::new();
    dates
        .into_iter()
        .filter(|date| seen.insert(*date))
        .collect()
}

/// `day - days`, refusing an arithmetic overflow rather than panicking in a scheduled job.
fn days_before(day: NaiveDate, days: i64) -> Result<NaiveDate, RefreshFailure> {
    let duration = Duration::try_days(days)
        .ok_or_else(|| RefreshFailure::Invalid(format!("{days} days is out of range")))?;
    day.checked_sub_signed(duration).ok_or_else(|| {
        RefreshFailure::Invalid(format!("{days} days before {day} is outside the calendar"))
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use chrono::TimeZone;

    use super::*;
    use crate::store::error::StoreError;
    use crate::store::models::{SeriesKey, SessionSymbolCount, StoredBar};
    use crate::store::FiveMinuteTape;

    fn day(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    fn warehouse_session(symbol: &str, close: Option<f64>) -> WarehouseSession {
        WarehouseSession {
            symbol: symbol.into(),
            session: day(2026, 11, 3),
            open: Some(100.0),
            high: Some(101.0),
            low: Some(99.0),
            close,
            volume: Some(1_000),
        }
    }

    #[derive(Default)]
    struct FakeWarehouse {
        sessions: Vec<WarehouseSession>,
        tape: Option<FiveMinuteTape>,
        candidates: Vec<String>,
        writes: Mutex<Vec<DailyBarWrite>>,
        write_batches: Mutex<Vec<usize>>,
        fill_batches: Mutex<Vec<usize>>,
    }

    #[async_trait]
    impl WarehouseRepository for FakeWarehouse {
        async fn barhourly_sessions(
            &self,
            _since: NaiveDate,
        ) -> Result<Vec<WarehouseSession>, StoreError> {
            Ok(self.sessions.clone())
        }

        async fn five_minute_tape(&self) -> Result<Option<FiveMinuteTape>, StoreError> {
            Ok(self.tape)
        }

        async fn raw_close_candidates(
            &self,
            _exchange: &str,
            _since: NaiveDate,
            _until: NaiveDate,
        ) -> Result<Vec<String>, StoreError> {
            Ok(self.candidates.clone())
        }

        async fn fill_raw_close(
            &self,
            _exchange: &str,
            _tape: FiveMinuteTape,
            _since: NaiveDate,
            _until: NaiveDate,
            symbols: &[String],
        ) -> Result<u64, StoreError> {
            self.fill_batches.lock().unwrap().push(symbols.len());
            Ok(symbols.len() as u64)
        }
    }

    #[async_trait]
    impl DailyBarRepository for FakeWarehouse {
        async fn get_daily_bars(
            &self,
            _symbol: &str,
            _sessions: i64,
            _exchange: &str,
        ) -> Result<Vec<StoredBar>, StoreError> {
            unimplemented!("the backfill never reads bars")
        }

        async fn get_daily_bars_many(
            &self,
            _symbols: &[String],
            _sessions: i64,
            _exchange: &str,
        ) -> Result<std::collections::HashMap<String, Vec<StoredBar>>, StoreError> {
            unimplemented!("the backfill never reads bars")
        }

        async fn daily_bar_depth(
            &self,
            _symbols: &[String],
            _exchange: &str,
            _since: Option<NaiveDate>,
        ) -> Result<std::collections::HashMap<String, i64>, StoreError> {
            unimplemented!("the backfill never measures depth")
        }

        async fn upsert_daily(&self, rows: Vec<DailyBarWrite>) -> Result<usize, StoreError> {
            self.write_batches.lock().unwrap().push(rows.len());
            self.writes.lock().unwrap().extend(rows.clone());
            Ok(rows.len())
        }

        async fn latest_session(&self) -> Result<Option<NaiveDate>, StoreError> {
            Ok(None)
        }

        async fn session_symbol_counts(
            &self,
            _limit: i64,
        ) -> Result<Vec<SessionSymbolCount>, StoreError> {
            Ok(Vec::new())
        }

        async fn symbols_with_min_sessions(
            &self,
            _minimum: i64,
        ) -> Result<Vec<SeriesKey>, StoreError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn a_seed_row_is_adjusted_history_without_a_raw_close() {
        let mut session = warehouse_session(" aapl ", Some(100.5));
        session.volume = None;
        let row = seed_row(&session, "US").unwrap();
        assert_eq!(row.symbol, "AAPL");
        assert_eq!(row.exchange.as_deref(), Some("US"));
        assert_eq!(row.close, 100.5);
        assert_eq!(row.raw_close, None);
        assert_eq!(row.volume, Some(0));
        assert_eq!(row.source.as_deref(), Some(SEED_SOURCE));
        assert_eq!(row.adjusted, Some(true));
    }

    #[test]
    fn a_seed_row_needs_every_price_and_a_positive_close() {
        assert!(seed_row(&warehouse_session("AAPL", None), "US").is_none());
        assert!(seed_row(&warehouse_session("AAPL", Some(0.0)), "US").is_none());
        assert!(seed_row(&warehouse_session("AAPL", Some(-1.0)), "US").is_none());

        let mut missing_high = warehouse_session("AAPL", Some(100.0));
        missing_high.high = None;
        assert!(seed_row(&missing_high, "US").is_none());

        let mut blank = warehouse_session("  ", Some(100.0));
        blank.symbol = "  ".into();
        assert!(seed_row(&blank, "US").is_none());
    }

    #[tokio::test]
    async fn a_seed_writes_only_usable_rows_with_the_market_stamped() {
        let fake = FakeWarehouse {
            sessions: vec![
                warehouse_session("AAPL", Some(100.0)),
                warehouse_session("BROKEN", Some(0.0)),
                warehouse_session("MSFT", Some(200.0)),
            ],
            ..FakeWarehouse::default()
        };
        let summary = seed_from_barhourly(&fake, "us", 90, day(2026, 11, 4), 10)
            .await
            .unwrap();
        assert_eq!(summary.rows_written, 2);
        assert_eq!(summary.since, day(2026, 8, 6));
        let writes = fake.writes.lock().unwrap();
        assert_eq!(writes.len(), 2);
        assert!(writes
            .iter()
            .all(|row| row.exchange.as_deref() == Some("US")));
    }

    #[tokio::test]
    async fn a_seed_commits_in_batches_rather_than_one_long_transaction() {
        let fake = FakeWarehouse {
            sessions: (0..5)
                .map(|index| warehouse_session(&format!("S{index}"), Some(100.0)))
                .collect(),
            ..FakeWarehouse::default()
        };
        seed_from_barhourly(&fake, "US", 90, day(2026, 11, 4), 2)
            .await
            .unwrap();
        assert_eq!(*fake.write_batches.lock().unwrap(), vec![2, 2, 1]);
    }

    #[tokio::test]
    async fn a_database_without_a_tape_reports_a_skip_rather_than_failing() {
        let fake = FakeWarehouse::default();
        let summary = fill_raw_close(&fake, "US", 800, utc(2026, 11, 4, 12))
            .await
            .unwrap();
        assert_eq!(summary.updated, 0);
        assert_eq!(summary.symbols, 0);
        assert_eq!(summary.skipped.as_deref(), Some("no-5min-tape"));
        assert_eq!(summary.until, None);
        assert_eq!(summary.table, None);
    }

    #[tokio::test]
    async fn the_raw_close_fill_reaches_back_from_the_latest_completed_session() {
        let fake = FakeWarehouse {
            tape: Some(FiveMinuteTape::Bar5Min),
            candidates: vec!["AAPL".into(), "MSFT".into()],
            ..FakeWarehouse::default()
        };
        let summary = fill_raw_close(&fake, "US", 800, utc(2026, 11, 4, 12))
            .await
            .unwrap();
        assert_eq!(summary.until, Some(day(2026, 11, 3)));
        // 800 calendar days before 2026-11-03.
        assert_eq!(summary.since, day(2024, 8, 25));
        assert_eq!(summary.table.as_deref(), Some("Bar5Min"));
        assert_eq!(summary.skipped, None);
        assert_eq!(summary.updated, 2);
        assert_eq!(*fake.fill_batches.lock().unwrap(), vec![2]);
    }

    #[tokio::test]
    async fn a_large_candidate_list_is_filled_in_parameter_sized_batches() {
        let fake = FakeWarehouse {
            tape: Some(FiveMinuteTape::BarReal5Min),
            candidates: (0..501).map(|index| format!("S{index}")).collect(),
            ..FakeWarehouse::default()
        };
        let summary = fill_raw_close(&fake, "US", 1, utc(2026, 11, 4, 12))
            .await
            .unwrap();
        assert_eq!(summary.updated, 501);
        assert_eq!(*fake.fill_batches.lock().unwrap(), vec![500, 1]);
    }

    #[test]
    fn the_weekday_expansion_includes_both_endpoints_and_skips_the_weekend() {
        // 2026-11-02 is a Monday; the range crosses one Saturday and one Sunday.
        let dates = weekday_dates(day(2026, 11, 2), day(2026, 11, 9));
        let expected: Vec<NaiveDate> = [2, 3, 4, 5, 6, 9]
            .into_iter()
            .map(|value| day(2026, 11, value))
            .collect();
        assert_eq!(dates, expected);
    }

    #[test]
    fn a_reversed_range_or_a_weekend_day_yields_nothing() {
        assert!(weekday_dates(day(2026, 11, 9), day(2026, 11, 2)).is_empty());
        assert!(weekday_dates(day(2026, 11, 7), day(2026, 11, 7)).is_empty());
        assert_eq!(
            weekday_dates(day(2026, 11, 4), day(2026, 11, 4)),
            vec![day(2026, 11, 4)]
        );
    }

    #[test]
    fn an_explicit_date_list_is_deduplicated_in_order() {
        let dates = distinct_dates([
            day(2026, 11, 3),
            day(2026, 11, 2),
            day(2026, 11, 3),
            day(2026, 11, 2),
        ]);
        assert_eq!(dates, vec![day(2026, 11, 3), day(2026, 11, 2)]);
    }

    fn utc(year: i32, month: u32, d: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, d, hour, 0, 0).unwrap()
    }
}
