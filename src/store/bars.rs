//! `DailySnapshot` reads and writes.
//!
//! Writes go through a session-scoped `#stage` table and a single `MERGE`, exactly as the
//! source service did: a merge per row would be ~50,000 statements and one long write lock,
//! and readers block behind an uncommitted writer. The set-based form holds the lock for
//! seconds.
//!
//! Unlike the source service, which made a fresh connection per call, the Rust store borrows
//! from a pool, so `#stage` can outlive one call. It is therefore dropped and recreated at the
//! start of every write. The `CREATE` must go through [`tiberius::Client::simple_query`]: a
//! plain batch persists the temp table for the session, while a parameterised call
//! (`sp_executesql`) scopes it to that dynamic batch and drops it on return.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::NaiveDate;

use super::connection::SqlServerStore;
use super::error::{SqlConnection, StoreError};
use super::keys::{normalise_symbols, read_exchange_key, required_exchange_key};
use super::models::{DailyBarWrite, SeriesKey, SessionSymbolCount, StoredBar};
use super::query::{
    boolean, date, decimal, execute, integer, query_rows, required_integer, required_text, text,
    SqlParam,
};
use super::sql::{placeholders, rows_per_statement, values_rows};

/// Read chunk size from the source service, chosen to stay under the parameter ceiling.
pub const BAR_READ_CHUNK: usize = 500;

/// The source recorded when a writer does not name one.
pub const DEFAULT_DAILY_SOURCE: &str = "eod-bulk";

/// Parameters per staged row: the eleven columns of `#stage`.
const DAILY_PARAMS_PER_ROW: usize = 11;

/// The staging table. Session-scoped, dropped and recreated for every write.
const CREATE_STAGE_SQL: &str = "\
IF OBJECT_ID('tempdb..#stage') IS NOT NULL DROP TABLE #stage;
CREATE TABLE #stage (
    SymbolCode   varchar(24)   NOT NULL,
    Exchange     varchar(8)    NOT NULL,
    SnapshotDate date          NOT NULL,
    [Open]       decimal(18,6) NOT NULL,
    [High]       decimal(18,6) NOT NULL,
    [Low]        decimal(18,6) NOT NULL,
    [Close]      decimal(18,6) NOT NULL,
    [RawClose]   decimal(18,6) NULL,
    Volume       bigint        NOT NULL,
    Source       varchar(16)   NOT NULL,
    IsAdjusted   bit           NOT NULL
);";

/// Merge the whole stage into `DailySnapshot` once.
const MERGE_DAILY_FROM_STAGE: &str = "\
MERGE dbo.DailySnapshot AS target
USING #stage AS source
    ON target.SymbolCode = source.SymbolCode
   AND target.Exchange = source.Exchange
   AND target.SnapshotDate = source.SnapshotDate
WHEN MATCHED THEN UPDATE SET
    [Open] = source.[Open], [High] = source.[High], [Low] = source.[Low],
    [Close] = source.[Close],
    -- A source with no raw close (the BarHourly seed, or a replayed benchmark row) must not
    -- erase one already collected. COALESCE keeps the known value; a fresh pull overwrites it.
    [RawClose] = COALESCE(source.[RawClose], target.[RawClose]),
    Volume = source.Volume, Source = source.Source,
    IsAdjusted = source.IsAdjusted, LoadedUtc = SYSUTCDATETIME()
WHEN NOT MATCHED THEN INSERT
    (SymbolCode, Exchange, SnapshotDate, [Open], [High], [Low], [Close], [RawClose], Volume,
     Source, IsAdjusted)
VALUES
    (source.SymbolCode, source.Exchange, source.SnapshotDate, source.[Open],
     source.[High], source.[Low], source.[Close], source.[RawClose], source.Volume,
     source.Source, source.IsAdjusted);";

/// Reads and writes completed daily bars.
#[async_trait]
pub trait DailyBarRepository: Send + Sync {
    async fn get_daily_bars(
        &self,
        symbol: &str,
        sessions: i64,
        exchange: &str,
    ) -> Result<Vec<StoredBar>, StoreError>;

    async fn get_daily_bars_many(
        &self,
        symbols: &[String],
        sessions: i64,
        exchange: &str,
    ) -> Result<HashMap<String, Vec<StoredBar>>, StoreError>;

    async fn daily_bar_depth(
        &self,
        symbols: &[String],
        exchange: &str,
        since: Option<NaiveDate>,
    ) -> Result<HashMap<String, i64>, StoreError>;

    async fn upsert_daily(&self, rows: Vec<DailyBarWrite>) -> Result<usize, StoreError>;

    async fn latest_session(&self) -> Result<Option<NaiveDate>, StoreError>;

    async fn session_symbol_counts(
        &self,
        limit: i64,
    ) -> Result<Vec<SessionSymbolCount>, StoreError>;

    /// Every series holding at least `minimum` sessions, ordered by exchange then symbol.
    ///
    /// This is the indicator rebuild's work list. It is a store query rather than a scan of
    /// `DailySnapshot` in the job because the grouping and the count belong with the table.
    async fn symbols_with_min_sessions(&self, minimum: i64) -> Result<Vec<SeriesKey>, StoreError>;
}

#[async_trait]
impl DailyBarRepository for SqlServerStore {
    async fn get_daily_bars(
        &self,
        symbol: &str,
        sessions: i64,
        exchange: &str,
    ) -> Result<Vec<StoredBar>, StoreError> {
        let key = symbol.trim().to_uppercase();
        let mut bars = self
            .get_daily_bars_many(std::slice::from_ref(&key), sessions, exchange)
            .await?;
        Ok(bars.remove(&key).unwrap_or_default())
    }

    async fn get_daily_bars_many(
        &self,
        symbols: &[String],
        sessions: i64,
        exchange: &str,
    ) -> Result<HashMap<String, Vec<StoredBar>>, StoreError> {
        let wanted = normalise_symbols(symbols);
        if wanted.is_empty() {
            return Ok(HashMap::new());
        }
        let exchange = read_exchange_key(Some(exchange));
        let mut out: HashMap<String, Vec<StoredBar>> = wanted
            .iter()
            .map(|symbol| (symbol.clone(), Vec::new()))
            .collect();
        let mut client = self.connection().await?;
        for chunk in wanted.chunks(BAR_READ_CHUNK) {
            let sessions_index = chunk.len() + 2;
            let sql = format!(
                "SELECT SymbolCode, SnapshotDate, [Open], [High], [Low], [Close], \
                        Volume, Source, IsAdjusted, [RawClose] \
                   FROM ( \
                       SELECT SymbolCode, SnapshotDate, [Open], [High], [Low], [Close], \
                              Volume, Source, IsAdjusted, [RawClose], \
                              ROW_NUMBER() OVER (PARTITION BY SymbolCode \
                                                 ORDER BY SnapshotDate DESC) AS rn \
                         FROM dbo.DailySnapshot \
                        WHERE Exchange = @P1 AND SymbolCode IN ({}) \
                   ) ranked \
                  WHERE rn <= @P{sessions_index} \
                  ORDER BY SymbolCode, SnapshotDate",
                placeholders(2, chunk.len()),
            );
            let mut params = Vec::with_capacity(chunk.len() + 2);
            params.push(SqlParam::text(&exchange));
            params.extend(chunk.iter().map(SqlParam::text));
            params.push(SqlParam::integer(sessions));
            let rows = query_rows(&mut client, &sql, &params).await?;
            for row in &rows {
                let symbol = required_text(row, 0, "SymbolCode")?.to_uppercase();
                let bar = StoredBar {
                    session: date(row, 1)?
                        .ok_or_else(|| StoreError::database("SnapshotDate was NULL"))?,
                    open: decimal(row, 2)?,
                    high: decimal(row, 3)?,
                    low: decimal(row, 4)?,
                    close: decimal(row, 5)?,
                    volume: integer(row, 6)?,
                    source: text(row, 7)?,
                    adjusted: boolean(row, 8)?.unwrap_or(false),
                    raw_close: decimal(row, 9)?,
                };
                if let Some(bars) = out.get_mut(&symbol) {
                    bars.push(bar);
                }
            }
        }
        Ok(out)
    }

    async fn daily_bar_depth(
        &self,
        symbols: &[String],
        exchange: &str,
        since: Option<NaiveDate>,
    ) -> Result<HashMap<String, i64>, StoreError> {
        let wanted = normalise_symbols(symbols);
        if wanted.is_empty() {
            return Ok(HashMap::new());
        }
        let exchange = read_exchange_key(Some(exchange));
        let mut out: HashMap<String, i64> =
            wanted.iter().map(|symbol| (symbol.clone(), 0)).collect();
        let mut client = self.connection().await?;
        for chunk in wanted.chunks(BAR_READ_CHUNK) {
            let mut sql = format!(
                "SELECT SymbolCode, COUNT(*) FROM dbo.DailySnapshot \
                  WHERE Exchange = @P1 AND SymbolCode IN ({})",
                placeholders(2, chunk.len())
            );
            let mut params = Vec::with_capacity(chunk.len() + 2);
            params.push(SqlParam::text(&exchange));
            params.extend(chunk.iter().map(SqlParam::text));
            if let Some(since) = since {
                let index = chunk.len() + 2;
                sql.push_str(&format!(" AND SnapshotDate >= @P{index}"));
                params.push(SqlParam::date(since));
            }
            sql.push_str(" GROUP BY SymbolCode");
            let rows = query_rows(&mut client, &sql, &params).await?;
            for row in &rows {
                let symbol = required_text(row, 0, "SymbolCode")?.to_uppercase();
                if let Some(depth) = out.get_mut(&symbol) {
                    *depth = required_integer(row, 1, "COUNT(*)")?;
                }
            }
        }
        Ok(out)
    }

    async fn upsert_daily(&self, rows: Vec<DailyBarWrite>) -> Result<usize, StoreError> {
        if rows.is_empty() {
            return Ok(0);
        }
        let payload: Vec<Vec<SqlParam>> =
            rows.iter().map(daily_params).collect::<Result<_, _>>()?;
        let attempted = payload.len();

        let mut client = self.connection().await?;
        client.begin_transaction().await?;
        if let Err(error) = run_simple(&mut client, CREATE_STAGE_SQL).await {
            let _ = client.rollback_transaction().await;
            return Err(error);
        }
        for chunk in payload.chunks(rows_per_statement(DAILY_PARAMS_PER_ROW)) {
            let sql = stage_insert_sql(chunk.len());
            let params: Vec<SqlParam> = chunk.iter().flatten().cloned().collect();
            if let Err(error) = execute(&mut client, &sql, &params).await {
                let _ = client.rollback_transaction().await;
                return Err(error);
            }
        }
        if let Err(error) = run_simple(&mut client, MERGE_DAILY_FROM_STAGE).await {
            let _ = client.rollback_transaction().await;
            return Err(error);
        }
        client.commit_transaction().await?;
        Ok(attempted)
    }

    async fn latest_session(&self) -> Result<Option<NaiveDate>, StoreError> {
        let mut client = self.connection().await?;
        let rows = query_rows(
            &mut client,
            "SELECT MAX(SnapshotDate) FROM dbo.DailySnapshot",
            &[],
        )
        .await?;
        match rows.first() {
            Some(row) => date(row, 0),
            None => Ok(None),
        }
    }

    async fn session_symbol_counts(
        &self,
        limit: i64,
    ) -> Result<Vec<SessionSymbolCount>, StoreError> {
        let mut client = self.connection().await?;
        let rows = query_rows(
            &mut client,
            "SELECT SnapshotDate, Exchange, COUNT(*) AS symbols \
               FROM dbo.DailySnapshot \
              WHERE SnapshotDate IN ( \
                  SELECT TOP (@P1) SnapshotDate FROM dbo.DailySnapshot \
                  GROUP BY SnapshotDate ORDER BY SnapshotDate DESC) \
              GROUP BY SnapshotDate, Exchange \
              ORDER BY SnapshotDate DESC, Exchange",
            &[SqlParam::integer(limit)],
        )
        .await?;
        rows.iter()
            .map(|row| {
                Ok(SessionSymbolCount {
                    session: date(row, 0)?
                        .ok_or_else(|| StoreError::database("SnapshotDate was NULL"))?,
                    exchange: required_text(row, 1, "Exchange")?,
                    symbols: required_integer(row, 2, "symbols")?,
                })
            })
            .collect()
    }

    async fn symbols_with_min_sessions(&self, minimum: i64) -> Result<Vec<SeriesKey>, StoreError> {
        let mut client = self.connection().await?;
        let rows = query_rows(
            &mut client,
            SYMBOLS_WITH_MIN_SESSIONS_SQL,
            &[SqlParam::integer(minimum.max(0))],
        )
        .await?;
        rows.iter()
            .map(|row| {
                Ok(SeriesKey {
                    symbol: required_text(row, 0, "SymbolCode")?.to_uppercase(),
                    exchange: required_text(row, 1, "Exchange")?.to_uppercase(),
                })
            })
            .collect()
    }
}

/// Series with enough history for the longest configured ATR. `HAVING COUNT(*)` is the same
/// predicate the daily job used to run itself, stated once beside the table it reads.
const SYMBOLS_WITH_MIN_SESSIONS_SQL: &str = "\
SELECT SymbolCode, Exchange
  FROM dbo.DailySnapshot
 GROUP BY SymbolCode, Exchange
HAVING COUNT(*) >= @P1
 ORDER BY Exchange, SymbolCode";

/// Run a batch that takes no parameters and drain every result set it leaves open.
async fn run_simple(client: &mut SqlConnection, sql: &str) -> Result<(), StoreError> {
    client.simple_query(sql).await?.into_results().await?;
    Ok(())
}

/// One row's eleven parameters, in the column order [`stage_insert_sql`] states.
fn daily_params(row: &DailyBarWrite) -> Result<Vec<SqlParam>, StoreError> {
    let exchange = required_exchange_key(row.exchange.as_deref())?;
    Ok(vec![
        SqlParam::text(&row.symbol),
        SqlParam::text(exchange),
        SqlParam::date(row.session),
        SqlParam::float(row.open),
        SqlParam::float(row.high),
        SqlParam::float(row.low),
        SqlParam::float(row.close),
        SqlParam::optional_float(row.raw_close),
        SqlParam::integer(row.volume.unwrap_or(0)),
        SqlParam::text_or(row.source.as_deref(), DEFAULT_DAILY_SOURCE),
        SqlParam::bit(row.adjusted.unwrap_or(true)),
    ])
}

/// A multi-row `INSERT` into `#stage` for one parameter-bounded block.
fn stage_insert_sql(rows: usize) -> String {
    format!(
        "INSERT INTO #stage (SymbolCode, Exchange, SnapshotDate, [Open], [High], [Low], \
                             [Close], [RawClose], Volume, Source, IsAdjusted) \
         VALUES {};",
        values_rows(rows, DAILY_PARAMS_PER_ROW)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(exchange: Option<&str>, adjusted: Option<bool>) -> DailyBarWrite {
        DailyBarWrite {
            symbol: "AAPL".into(),
            exchange: exchange.map(str::to_string),
            session: NaiveDate::from_ymd_opt(2026, 11, 3).unwrap(),
            open: 100.0,
            high: 101.0,
            low: 99.0,
            close: 100.5,
            raw_close: Some(100.4),
            volume: None,
            source: None,
            adjusted,
        }
    }

    #[test]
    fn the_merge_keeps_an_existing_raw_close_and_the_exchange_key() {
        assert!(MERGE_DAILY_FROM_STAGE.contains("USING #stage AS source"));
        assert!(MERGE_DAILY_FROM_STAGE.contains("target.Exchange = source.Exchange"));
        assert!(MERGE_DAILY_FROM_STAGE.contains("target.SnapshotDate = source.SnapshotDate"));
        assert!(MERGE_DAILY_FROM_STAGE
            .contains("[RawClose] = COALESCE(source.[RawClose], target.[RawClose])"));
    }

    #[test]
    fn the_stage_batch_is_recreated_rather_than_reused() {
        assert!(CREATE_STAGE_SQL
            .starts_with("IF OBJECT_ID('tempdb..#stage') IS NOT NULL DROP TABLE #stage;"));
        assert!(CREATE_STAGE_SQL.contains("CREATE TABLE #stage"));
    }

    #[test]
    fn a_stage_insert_numbers_every_row_continuously() {
        let sql = stage_insert_sql(2);
        assert!(sql.contains("(@P1,@P2,@P3,@P4,@P5,@P6,@P7,@P8,@P9,@P10,@P11)"));
        assert!(sql.contains("(@P12,"));
        assert!(sql.trim_end().ends_with(';'));
    }

    #[test]
    fn a_stage_insert_stays_under_the_parameter_ceiling() {
        let rows = rows_per_statement(DAILY_PARAMS_PER_ROW);
        assert_eq!(rows, 181);
        assert_eq!(stage_insert_sql(rows).matches("@P").count(), rows * 11);
    }

    #[test]
    fn daily_parameters_apply_the_source_defaults() {
        let params = daily_params(&write(Some("us"), None)).unwrap();
        assert_eq!(params.len(), DAILY_PARAMS_PER_ROW);
        assert_eq!(params[1], SqlParam::text("US"));
        assert_eq!(params[8], SqlParam::integer(0));
        assert_eq!(params[9], SqlParam::text(DEFAULT_DAILY_SOURCE));
        assert_eq!(params[10], SqlParam::bit(true));
    }

    #[test]
    fn an_explicit_false_adjusted_flag_is_not_defaulted_away() {
        let params = daily_params(&write(Some("US"), Some(false))).unwrap();
        assert_eq!(params[10], SqlParam::bit(false));
    }

    #[test]
    fn a_daily_row_without_an_exchange_is_refused_before_connecting() {
        assert!(matches!(
            daily_params(&write(None, None)).unwrap_err(),
            StoreError::Invalid(_)
        ));
    }

    #[test]
    fn the_indicator_work_list_counts_sessions_per_exchange() {
        assert!(SYMBOLS_WITH_MIN_SESSIONS_SQL.contains("GROUP BY SymbolCode, Exchange"));
        assert!(SYMBOLS_WITH_MIN_SESSIONS_SQL.contains("HAVING COUNT(*) >= @P1"));
        assert!(SYMBOLS_WITH_MIN_SESSIONS_SQL.contains("ORDER BY Exchange, SymbolCode"));
    }
}
