//! The warehouse tables the free backfills read: `BarHourly` and the 5-minute tape.
//!
//! The service does not own either table. `BarHourly` is the paid-for, split-adjusted hourly
//! history the estate already holds; `Bar5Min` (renamed from `BarReal5Min` on 2026-11-02) is
//! the unadjusted intraday tape. Both backfills write only `DailySnapshot`, so this module is
//! a reader plus one narrowly scoped `UPDATE`, and it is deliberately not part of
//! [`super::MarketDataStore`]: a service replica without the warehouse can still serve prices,
//! bars and ATR.
//!
//! # Why the tape name is an enum, not a string
//!
//! SQL Server cannot bind a table name as a parameter, so the statement has to be formatted.
//! [`FiveMinuteTape`] is the allow-list: the only strings that can reach the `FROM` clause are
//! the two names the estate has used, each of which was first confirmed with `OBJECT_ID`.

use async_trait::async_trait;
use chrono::NaiveDate;

use super::connection::SqlServerStore;
use super::error::StoreError;
use super::keys::read_exchange_key;
use super::models::WarehouseSession;
use super::query::{date, float, integer, query_rows, required_text, SqlParam};
use super::sql::placeholders;

/// The two names the estate's 5-minute tape has had. `Bar5Min` is probed first because it is
/// the current one; a database from before the rename answers with `BarReal5Min`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FiveMinuteTape {
    Bar5Min,
    BarReal5Min,
}

impl FiveMinuteTape {
    pub const PROBE_ORDER: [Self; 2] = [Self::Bar5Min, Self::BarReal5Min];

    /// The table name, from a closed set so it cannot carry a caller's text into SQL.
    pub fn table_name(self) -> &'static str {
        match self {
            Self::Bar5Min => "Bar5Min",
            Self::BarReal5Min => "BarReal5Min",
        }
    }
}

/// One session's OHLCV, aggregated from the hourly warehouse bars.
///
/// The aggregates are cast to `float` in the `SELECT`: the columns are `decimal(18,6)`, and a
/// date-level aggregate of a decimal needs no precision beyond the six places the daily table
/// stores. Volume stays `bigint`.
const BARHOURLY_AGGREGATE_SQL: &str = "\
WITH ranked AS (
    SELECT SymbolCode,
           CAST(BarTimeUtc AS DATE) AS SessionDate,
           BarTimeUtc, [Open], [High], [Low], [Close], Volume,
           ROW_NUMBER() OVER (PARTITION BY SymbolCode, CAST(BarTimeUtc AS DATE)
                              ORDER BY BarTimeUtc ASC)  AS rn_first,
           ROW_NUMBER() OVER (PARTITION BY SymbolCode, CAST(BarTimeUtc AS DATE)
                              ORDER BY BarTimeUtc DESC) AS rn_last
      FROM dbo.BarHourly
     WHERE BarTimeUtc >= @P1
),
aggregated AS (
    SELECT SymbolCode, SessionDate,
           MAX(CASE WHEN rn_first = 1 THEN [Open] END) AS [Open],
           MAX([High]) AS [High],
           MIN([Low])  AS [Low],
           MAX(CASE WHEN rn_last = 1 THEN [Close] END) AS [Close],
           SUM(Volume) AS Volume
      FROM ranked
     GROUP BY SymbolCode, SessionDate
)
SELECT SymbolCode, SessionDate,
       CAST([Open] AS float), CAST([High] AS float), CAST([Low] AS float),
       CAST([Close] AS float), Volume
  FROM aggregated
 WHERE [Open] IS NOT NULL AND [Close] IS NOT NULL
 ORDER BY SymbolCode, SessionDate";

/// Which of the two possible tape names exists, in one round trip.
const TAPE_PROBE_SQL: &str = "SELECT OBJECT_ID(@P1, 'U'), OBJECT_ID(@P2, 'U')";

/// The symbols worth touching. The list is read from `DailySnapshot` rather than the
/// warehouse: 1.7M rows aggregate in seconds, while a `DISTINCT` over the 81M-row tape is a
/// full scan of it.
const RAW_CLOSE_CANDIDATES_SQL: &str = "\
SELECT DISTINCT SymbolCode
  FROM dbo.DailySnapshot
 WHERE Exchange = @P1
   AND RawClose IS NULL
   AND SnapshotDate >= @P2
   AND SnapshotDate <= @P3";

/// The last *traded* bar of the session, by UTC date. The tape is unadjusted, so this is the
/// close that printed; a zero close (a halt bar) is skipped rather than written as the price.
const RAW_CLOSE_FROM_BARS_TEMPLATE: &str = "\
UPDATE d
   SET RawClose = b.[Close]
  FROM dbo.DailySnapshot d
  CROSS APPLY (SELECT TOP (1) x.[Close]
                 FROM dbo.{table} x
                WHERE x.SymbolCode = d.SymbolCode
                  AND x.Exchange = d.Exchange
                  AND x.BarTimeUtc >= CAST(d.SnapshotDate AS datetime2)
                  AND x.BarTimeUtc <  DATEADD(day, 1, CAST(d.SnapshotDate AS datetime2))
                  AND x.[Close] > 0
                ORDER BY x.BarTimeUtc DESC) b
 WHERE d.Exchange = @P1
   AND d.RawClose IS NULL
   AND d.SnapshotDate >= @P2
   AND d.SnapshotDate <= @P3
   AND d.SymbolCode IN ({symbols})";

/// Reads `BarHourly` and the 5-minute tape for the free backfills.
#[async_trait]
pub trait WarehouseRepository: Send + Sync {
    async fn barhourly_sessions(
        &self,
        since: NaiveDate,
    ) -> Result<Vec<WarehouseSession>, StoreError>;

    async fn five_minute_tape(&self) -> Result<Option<FiveMinuteTape>, StoreError>;

    async fn raw_close_candidates(
        &self,
        exchange: &str,
        since: NaiveDate,
        until: NaiveDate,
    ) -> Result<Vec<String>, StoreError>;

    /// Fill `RawClose` for `symbols`, one statement per symbol batch. Rows already filled and
    /// rows outside `since..=until` are untouched, which is what makes a re-run safe.
    async fn fill_raw_close(
        &self,
        exchange: &str,
        tape: FiveMinuteTape,
        since: NaiveDate,
        until: NaiveDate,
        symbols: &[String],
    ) -> Result<u64, StoreError>;
}

#[async_trait]
impl WarehouseRepository for SqlServerStore {
    async fn barhourly_sessions(
        &self,
        since: NaiveDate,
    ) -> Result<Vec<WarehouseSession>, StoreError> {
        let mut client = self.connection().await?;
        let rows = query_rows(
            &mut client,
            BARHOURLY_AGGREGATE_SQL,
            &[SqlParam::date(since)],
        )
        .await?;
        rows.iter()
            .map(|row| {
                Ok(WarehouseSession {
                    symbol: required_text(row, 0, "SymbolCode")?.trim().to_uppercase(),
                    session: date(row, 1)?
                        .ok_or_else(|| StoreError::database("SessionDate was NULL"))?,
                    open: float(row, 2)?,
                    high: float(row, 3)?,
                    low: float(row, 4)?,
                    close: float(row, 5)?,
                    volume: integer(row, 6)?,
                })
            })
            .collect()
    }

    async fn five_minute_tape(&self) -> Result<Option<FiveMinuteTape>, StoreError> {
        let names: Vec<SqlParam> = FiveMinuteTape::PROBE_ORDER
            .iter()
            .map(|tape| SqlParam::text(tape.table_name()))
            .collect();
        let mut client = self.connection().await?;
        let rows = query_rows(&mut client, TAPE_PROBE_SQL, &names).await?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let [first, second] = FiveMinuteTape::PROBE_ORDER;
        if integer(row, 0)?.is_some() {
            return Ok(Some(first));
        }
        if integer(row, 1)?.is_some() {
            return Ok(Some(second));
        }
        Ok(None)
    }

    async fn raw_close_candidates(
        &self,
        exchange: &str,
        since: NaiveDate,
        until: NaiveDate,
    ) -> Result<Vec<String>, StoreError> {
        let exchange = read_exchange_key(Some(exchange));
        let mut client = self.connection().await?;
        let rows = query_rows(
            &mut client,
            RAW_CLOSE_CANDIDATES_SQL,
            &[
                SqlParam::text(&exchange),
                SqlParam::date(since),
                SqlParam::date(until),
            ],
        )
        .await?;
        rows.iter()
            .map(|row| Ok(required_text(row, 0, "SymbolCode")?.trim().to_uppercase()))
            .collect()
    }

    async fn fill_raw_close(
        &self,
        exchange: &str,
        tape: FiveMinuteTape,
        since: NaiveDate,
        until: NaiveDate,
        symbols: &[String],
    ) -> Result<u64, StoreError> {
        if symbols.is_empty() {
            return Ok(0);
        }
        let sql = raw_close_fill_sql(tape, symbols.len());
        let exchange = read_exchange_key(Some(exchange));
        let mut params = Vec::with_capacity(symbols.len() + 3);
        params.push(SqlParam::text(&exchange));
        params.push(SqlParam::date(since));
        params.push(SqlParam::date(until));
        params.extend(symbols.iter().map(SqlParam::text));

        let mut client = self.connection().await?;
        super::query::execute(&mut client, &sql, &params).await
    }
}

/// The fill statement for one symbol batch.
fn raw_close_fill_sql(tape: FiveMinuteTape, symbols: usize) -> String {
    RAW_CLOSE_FROM_BARS_TEMPLATE
        .replace("{table}", tape.table_name())
        .replace("{symbols}", &placeholders(4, symbols))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::query::MAX_PARAMS_PER_STATEMENT;

    #[test]
    fn the_aggregate_reads_hourly_bars_and_casts_at_the_boundary() {
        assert!(BARHOURLY_AGGREGATE_SQL.contains("FROM dbo.BarHourly"));
        assert!(BARHOURLY_AGGREGATE_SQL.contains("CAST([Open] AS float)"));
        assert!(BARHOURLY_AGGREGATE_SQL.contains("CAST([Close] AS float)"));
        assert!(BARHOURLY_AGGREGATE_SQL.contains("SUM(Volume)"));
        assert!(BARHOURLY_AGGREGATE_SQL.contains("ORDER BY SymbolCode, SessionDate"));
    }

    #[test]
    fn the_aggregate_takes_the_first_open_and_last_close_of_the_session() {
        assert!(BARHOURLY_AGGREGATE_SQL.contains("MAX(CASE WHEN rn_first = 1 THEN [Open] END)"));
        assert!(BARHOURLY_AGGREGATE_SQL.contains("MAX(CASE WHEN rn_last = 1 THEN [Close] END)"));
        assert!(
            BARHOURLY_AGGREGATE_SQL.contains("WHERE [Open] IS NOT NULL AND [Close] IS NOT NULL")
        );
    }

    #[test]
    fn the_tape_probe_names_the_current_table_before_the_old_one() {
        assert_eq!(
            FiveMinuteTape::PROBE_ORDER
                .iter()
                .map(|tape| tape.table_name())
                .collect::<Vec<_>>(),
            vec!["Bar5Min", "BarReal5Min"]
        );
        assert_eq!(TAPE_PROBE_SQL.matches("@P").count(), 2);
    }

    #[test]
    fn the_candidates_are_only_unfilled_rows_in_the_requested_window() {
        for fragment in [
            "Exchange = @P1",
            "RawClose IS NULL",
            "SnapshotDate >= @P2",
            "SnapshotDate <= @P3",
            "SELECT DISTINCT SymbolCode",
        ] {
            assert!(RAW_CLOSE_CANDIDATES_SQL.contains(fragment), "{fragment}");
        }
    }

    #[test]
    fn the_fill_never_overwrites_a_raw_close_or_leaves_the_requested_window() {
        let sql = raw_close_fill_sql(FiveMinuteTape::Bar5Min, 2);
        for fragment in [
            "UPDATE d",
            "SET RawClose = b.[Close]",
            "FROM dbo.Bar5Min x",
            "x.[Close] > 0",
            "d.RawClose IS NULL",
            "d.Exchange = @P1",
            "d.SnapshotDate >= @P2",
            "d.SnapshotDate <= @P3",
            "d.SymbolCode IN (@P4,@P5)",
        ] {
            assert!(sql.contains(fragment), "{fragment} missing from {sql}");
        }
    }

    #[test]
    fn the_fill_uses_the_old_tape_when_that_is_the_one_probed() {
        let sql = raw_close_fill_sql(FiveMinuteTape::BarReal5Min, 1);
        assert!(sql.contains("FROM dbo.BarReal5Min x"));
        assert!(!sql.contains("dbo.Bar5Min"));
    }

    #[test]
    fn a_symbol_batch_stays_under_the_parameter_ceiling() {
        let chunk = 500usize;
        let sql = raw_close_fill_sql(FiveMinuteTape::Bar5Min, chunk);
        assert!(chunk + 3 <= MAX_PARAMS_PER_STATEMENT);
        assert_eq!(sql.matches("@P").count(), chunk + 3);
    }
}
