//! `IntradayBar5Min` reads and writes.
//!
//! The table holds the vendor's **native 5-minute rows only**. Every larger timeframe is
//! derived on read ([`crate::domain::intraday`]): storing the aggregates too would duplicate
//! derived state and let a 5m row and its 4H bucket disagree after a re-pull.
//!
//! Writes are a single `MERGE` over a parameter-bounded `VALUES` block, keyed
//! `(Exchange, SymbolCode, TsUtc)`, so a refetch of the same session updates in place rather
//! than duplicating. The read is a newest-N-then-ascending query, because the endpoint always
//! wants the tail of the series in timestamp order.

use async_trait::async_trait;
use chrono::NaiveDateTime;

use super::connection::SqlServerStore;
use super::error::StoreError;
use super::keys::required_exchange_key;
use super::models::{IntradayBarWrite, StoredIntradayBar};
use super::query::{
    boolean, decimal, execute, integer, query_rows, required_text, timestamp, SqlParam,
};
use super::sql::{rows_per_statement, values_rows};

/// Parameters per staged row: the nine columns a writer states.
const INTRADAY_PARAMS_PER_ROW: usize = 9;

/// The source recorded on rows this endpoint writes.
pub const INTRADAY_SOURCE: &str = "eodhd";

/// Reads and writes the owned intraday tape.
#[async_trait]
pub trait IntradayRepository: Send + Sync {
    /// The newest `limit` native 5m rows for one series, oldest first.
    async fn get_intraday_bars(
        &self,
        exchange: &str,
        symbol: &str,
        limit: i64,
    ) -> Result<Vec<StoredIntradayBar>, StoreError>;

    async fn upsert_intraday(&self, rows: Vec<IntradayBarWrite>) -> Result<usize, StoreError>;

    /// Delete every row older than `before`, returning how many went.
    async fn prune_intraday(&self, before: NaiveDateTime) -> Result<usize, StoreError>;
}

#[async_trait]
impl IntradayRepository for SqlServerStore {
    async fn get_intraday_bars(
        &self,
        exchange: &str,
        symbol: &str,
        limit: i64,
    ) -> Result<Vec<StoredIntradayBar>, StoreError> {
        let limit = limit.max(0);
        if limit == 0 {
            return Ok(Vec::new());
        }
        let exchange = required_exchange_key(Some(exchange))?;
        let mut client = self.connection().await?;
        let rows = query_rows(
            &mut client,
            INTRADAY_READ_SQL,
            &[
                SqlParam::text(&exchange),
                SqlParam::text(symbol.trim().to_uppercase()),
                SqlParam::integer(limit),
            ],
        )
        .await?;
        rows.iter()
            .map(|row| {
                Ok(StoredIntradayBar {
                    symbol: required_text(row, 0, "SymbolCode")?.to_uppercase(),
                    exchange: required_text(row, 1, "Exchange")?,
                    ts_utc: timestamp(row, 2)?
                        .ok_or_else(|| StoreError::database("TsUtc was NULL"))?,
                    open: decimal(row, 3)?,
                    high: decimal(row, 4)?,
                    low: decimal(row, 5)?,
                    close: decimal(row, 6)?,
                    volume: integer(row, 7)?,
                    adjusted: boolean(row, 8)?.unwrap_or(false),
                    fetched_at: timestamp(row, 9)?,
                })
            })
            .collect()
    }

    async fn upsert_intraday(&self, rows: Vec<IntradayBarWrite>) -> Result<usize, StoreError> {
        if rows.is_empty() {
            return Ok(0);
        }
        let payload: Vec<Vec<SqlParam>> =
            rows.iter().map(intraday_params).collect::<Result<_, _>>()?;
        let attempted = payload.len();

        let mut client = self.connection().await?;
        client.begin_transaction().await?;
        for chunk in payload.chunks(rows_per_statement(INTRADAY_PARAMS_PER_ROW)) {
            let sql = merge_intraday_sql(chunk.len());
            let params: Vec<SqlParam> = chunk.iter().flatten().cloned().collect();
            if let Err(error) = execute(&mut client, &sql, &params).await {
                let _ = client.rollback_transaction().await;
                return Err(error);
            }
        }
        client.commit_transaction().await?;
        Ok(attempted)
    }

    async fn prune_intraday(&self, before: NaiveDateTime) -> Result<usize, StoreError> {
        let mut client = self.connection().await?;
        let deleted = execute(
            &mut client,
            "DELETE FROM dbo.IntradayBar5Min WHERE TsUtc < @P1",
            &[SqlParam::timestamp(before)],
        )
        .await?;
        usize::try_from(deleted).map_err(|_| {
            StoreError::database("the intraday prune reported a negative row count")
        })
    }
}

/// The tail of one series, ascending. `rn` is chosen descending so the newest rows survive the
/// bound; the outer sort restores chronological order for the reader.
const INTRADAY_READ_SQL: &str = "\
SELECT SymbolCode, Exchange, TsUtc, [Open], [High], [Low], [Close], Volume, Adjusted, FetchedUtc
  FROM (
      SELECT SymbolCode, Exchange, TsUtc, [Open], [High], [Low], [Close], Volume, Adjusted,
             FetchedUtc,
             ROW_NUMBER() OVER (ORDER BY TsUtc DESC) AS rn
        FROM dbo.IntradayBar5Min
       WHERE Exchange = @P1 AND SymbolCode = @P2
  ) ranked
 WHERE rn <= @P3
 ORDER BY TsUtc";

/// One row's nine parameters, in the column order [`merge_intraday_sql`] states.
fn intraday_params(row: &IntradayBarWrite) -> Result<Vec<SqlParam>, StoreError> {
    let exchange = required_exchange_key(row.exchange.as_deref())?;
    Ok(vec![
        SqlParam::text(&row.symbol),
        SqlParam::text(exchange),
        SqlParam::timestamp(row.ts_utc),
        SqlParam::float(row.open),
        SqlParam::float(row.high),
        SqlParam::float(row.low),
        SqlParam::float(row.close),
        SqlParam::integer(row.volume),
        SqlParam::bit(row.adjusted),
    ])
}

/// A `MERGE` for one parameter-bounded block of native rows.
fn merge_intraday_sql(rows: usize) -> String {
    format!(
        "MERGE dbo.IntradayBar5Min AS target \
         USING (VALUES {}) AS source \
             (SymbolCode, Exchange, TsUtc, [Open], [High], [Low], [Close], Volume, Adjusted) \
             ON target.Exchange = source.Exchange \
            AND target.SymbolCode = source.SymbolCode \
            AND target.TsUtc = source.TsUtc \
         WHEN MATCHED THEN UPDATE SET \
             [Open] = source.[Open], [High] = source.[High], [Low] = source.[Low], \
             [Close] = source.[Close], Volume = source.Volume, Adjusted = source.Adjusted, \
             FetchedUtc = SYSUTCDATETIME() \
         WHEN NOT MATCHED THEN INSERT \
             (SymbolCode, Exchange, TsUtc, [Open], [High], [Low], [Close], Volume, Adjusted, \
              FetchedUtc) \
         VALUES \
             (source.SymbolCode, source.Exchange, source.TsUtc, source.[Open], source.[High], \
              source.[Low], source.[Close], source.Volume, source.Adjusted, SYSUTCDATETIME());",
        values_rows(rows, INTRADAY_PARAMS_PER_ROW)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn write(exchange: Option<&str>) -> IntradayBarWrite {
        IntradayBarWrite {
            symbol: "AAPL".into(),
            exchange: exchange.map(str::to_string),
            ts_utc: NaiveDate::from_ymd_opt(2026, 11, 12)
                .unwrap()
                .and_hms_opt(14, 30, 0)
                .unwrap(),
            open: 231.4,
            high: 233.1,
            low: 230.9,
            close: 232.7,
            volume: 1_200_300,
            adjusted: false,
        }
    }

    #[test]
    fn the_merge_keys_on_exchange_symbol_and_instant() {
        let sql = merge_intraday_sql(2);
        assert!(sql.contains("MERGE dbo.IntradayBar5Min AS target"));
        assert!(sql.contains("target.Exchange = source.Exchange"));
        assert!(sql.contains("target.SymbolCode = source.SymbolCode"));
        assert!(sql.contains("target.TsUtc = source.TsUtc"));
        assert!(sql.contains("FetchedUtc = SYSUTCDATETIME()"));
        assert_eq!(sql.matches("@P").count(), 18);
    }

    #[test]
    fn a_merge_block_stays_under_the_parameter_ceiling() {
        let rows = rows_per_statement(INTRADAY_PARAMS_PER_ROW);
        assert_eq!(rows, 222);
        assert_eq!(merge_intraday_sql(rows).matches("@P").count(), rows * 9);
    }

    #[test]
    fn intraday_parameters_follow_the_column_order() {
        let params = intraday_params(&write(Some("us"))).unwrap();
        assert_eq!(params.len(), INTRADAY_PARAMS_PER_ROW);
        assert_eq!(params[0], SqlParam::text("AAPL"));
        assert_eq!(params[1], SqlParam::text("US"));
        assert_eq!(params[8], SqlParam::bit(false));
    }

    #[test]
    fn a_row_without_an_exchange_is_refused_before_connecting() {
        assert!(matches!(
            intraday_params(&write(None)).unwrap_err(),
            StoreError::Invalid(_)
        ));
    }

    #[test]
    fn the_read_takes_the_newest_rows_and_returns_them_ascending() {
        assert!(INTRADAY_READ_SQL.contains("ORDER BY TsUtc DESC"));
        assert!(INTRADAY_READ_SQL.contains("WHERE rn <= @P3"));
        assert!(INTRADAY_READ_SQL.trim_end().ends_with("ORDER BY TsUtc"));
    }
}
