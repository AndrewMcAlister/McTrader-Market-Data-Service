//! `SymbolPriceCache` reads and writes.
//!
//! The cache is keyed by `(SymbolCode, Exchange)`, so an exchange is part of every question rather
//! than an attribute of the answer. Writes are one `MERGE` per parameter-bounded block of
//! rows: the source service used `executemany`, which batched thousands of statements into
//! one round trip; TDS has no equivalent, so the block size is derived from SQL Server's
//! 2,100-parameter ceiling.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{NaiveDateTime, Utc};

use super::connection::SqlServerStore;
use super::error::StoreError;
use super::keys::{normalise_symbols, required_exchange_key};
use super::models::{CachedPrice, Mover, Movers, PriceRow};
use super::query::{
    decimal, execute, query_rows, required_float, required_integer, required_text, text, timestamp,
    SqlParam,
};
use super::sql::{rows_per_statement, values_rows};

/// Read chunk size from the source service. A whole-market scan is ~2,800 symbols, which
/// would otherwise cross SQL Server's parameter ceiling.
pub const PRICE_READ_CHUNK: usize = 1_000;

/// Parameters per price row: the eleven columns of `SymbolPriceCache` a writer states.
const PRICE_PARAMS_PER_ROW: usize = 11;

/// The provider recorded when a caller does not name one.
pub const DEFAULT_PRICE_PROVIDER: &str = "eodhd";

/// Reads and writes the price cache.
#[async_trait]
pub trait PriceRepository: Send + Sync {
    async fn get_prices(
        &self,
        symbols: &[String],
        exchange: &str,
    ) -> Result<HashMap<String, CachedPrice>, StoreError>;

    async fn upsert_prices(&self, rows: Vec<PriceRow>) -> Result<usize, StoreError>;

    async fn big_movers(
        &self,
        exchange: &str,
        limit: i64,
        min_price: f64,
        min_dollar_volume: f64,
        since: Option<NaiveDateTime>,
    ) -> Result<Movers, StoreError>;

    async fn latest_exchange_price(
        &self,
        exchange: &str,
    ) -> Result<Option<NaiveDateTime>, StoreError>;

    async fn top_symbols_by_dollar_volume(
        &self,
        exchange: &str,
        limit: i64,
        min_breadth: i64,
    ) -> Result<Vec<String>, StoreError>;
}

#[async_trait]
impl PriceRepository for SqlServerStore {
    async fn get_prices(
        &self,
        symbols: &[String],
        exchange: &str,
    ) -> Result<HashMap<String, CachedPrice>, StoreError> {
        let wanted = normalise_symbols(symbols);
        if wanted.is_empty() {
            return Ok(HashMap::new());
        }
        let mut client = self.connection().await?;
        let mut out = HashMap::with_capacity(wanted.len());
        for chunk in wanted.chunks(PRICE_READ_CHUNK) {
            let sql = format!(
                "SELECT SymbolCode, Exchange, Price, Bid, Ask, PreviousClose, ChangePct, \
                        AsOfUtc, LastUpdatedUtc, Source \
                   FROM dbo.SymbolPriceCache \
                  WHERE Exchange = @P1 AND SymbolCode IN ({})",
                super::sql::placeholders(2, chunk.len())
            );
            let mut params = Vec::with_capacity(chunk.len() + 1);
            params.push(SqlParam::text(exchange));
            params.extend(chunk.iter().map(SqlParam::text));
            let rows = query_rows(&mut client, &sql, &params).await?;
            for row in &rows {
                let symbol = required_text(row, 0, "SymbolCode")?;
                let key = symbol.to_uppercase();
                out.insert(
                    key.clone(),
                    CachedPrice {
                        symbol: key,
                        exchange: required_text(row, 1, "Exchange")?,
                        price: decimal(row, 2)?,
                        bid: decimal(row, 3)?,
                        ask: decimal(row, 4)?,
                        previous_close: decimal(row, 5)?,
                        change_pct: decimal(row, 6)?,
                        as_of: timestamp(row, 7)?,
                        fetched_at: timestamp(row, 8)?,
                        provider: text(row, 9)?,
                    },
                );
            }
        }
        Ok(out)
    }

    async fn upsert_prices(&self, rows: Vec<PriceRow>) -> Result<usize, StoreError> {
        if rows.is_empty() {
            return Ok(0);
        }
        let now = Utc::now().naive_utc();
        let payload: Vec<Vec<SqlParam>> = rows
            .iter()
            .map(|row| price_params(row, now))
            .collect::<Result<_, _>>()?;
        let attempted = payload.len();

        let mut client = self.connection().await?;
        client.begin_transaction().await?;
        for chunk in payload.chunks(rows_per_statement(PRICE_PARAMS_PER_ROW)) {
            let sql = merge_prices_sql(chunk.len());
            let params: Vec<SqlParam> = chunk.iter().flatten().cloned().collect();
            if let Err(error) = execute(&mut client, &sql, &params).await {
                let _ = client.rollback_transaction().await;
                return Err(error);
            }
        }
        client.commit_transaction().await?;
        Ok(attempted)
    }

    async fn big_movers(
        &self,
        exchange: &str,
        limit: i64,
        min_price: f64,
        min_dollar_volume: f64,
        since: Option<NaiveDateTime>,
    ) -> Result<Movers, StoreError> {
        let limit = limit.max(0);
        let mut client = self.connection().await?;
        let mut movers = Movers::default();
        for (target, direction, order) in [
            (&mut movers.winners, ">=", "DESC"),
            (&mut movers.losers, "<", "ASC"),
        ] {
            let sql = movers_sql(direction, order, since.is_some());
            let mut params = vec![
                SqlParam::integer(limit),
                SqlParam::text(exchange),
                SqlParam::float(min_price),
                SqlParam::float(min_dollar_volume),
            ];
            if let Some(since) = since {
                params.push(SqlParam::timestamp(since));
            }
            let rows = query_rows(&mut client, &sql, &params).await?;
            for row in &rows {
                target.push(Mover {
                    symbol: required_text(row, 0, "SymbolCode")?,
                    price: required_float(row, 1, "Price")?,
                    change_pct: required_float(row, 2, "ChangePct")?,
                    volume: required_integer(row, 3, "Volume")?,
                    previous_close: decimal(row, 4)?,
                    as_of_utc: timestamp(row, 5)?,
                    source: text(row, 6)?,
                    dollar_volume: decimal(row, 7)?,
                });
            }
        }
        Ok(movers)
    }

    async fn latest_exchange_price(
        &self,
        exchange: &str,
    ) -> Result<Option<NaiveDateTime>, StoreError> {
        let mut client = self.connection().await?;
        let rows = query_rows(
            &mut client,
            "SELECT MAX(COALESCE(AsOfUtc, LastUpdatedUtc)) \
               FROM dbo.SymbolPriceCache WHERE Exchange = @P1",
            &[SqlParam::text(exchange)],
        )
        .await?;
        match rows.first() {
            Some(row) => timestamp(row, 0),
            None => Ok(None),
        }
    }

    async fn top_symbols_by_dollar_volume(
        &self,
        exchange: &str,
        limit: i64,
        min_breadth: i64,
    ) -> Result<Vec<String>, StoreError> {
        let limit = limit.max(0);
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut client = self.connection().await?;
        let rows = query_rows(
            &mut client,
            TOP_SYMBOLS_SQL,
            &[
                SqlParam::text(exchange),
                SqlParam::integer(min_breadth.max(1)),
                SqlParam::integer(limit),
                SqlParam::text(exchange),
            ],
        )
        .await?;
        rows.iter()
            .map(|row| required_text(row, 0, "SymbolCode"))
            .collect()
    }
}

/// One row's eleven parameters, in the column order [`merge_prices_sql`] states.
fn price_params(row: &PriceRow, now: NaiveDateTime) -> Result<Vec<SqlParam>, StoreError> {
    let exchange = required_exchange_key(row.exchange.as_deref())?;
    Ok(vec![
        SqlParam::text(&row.symbol),
        SqlParam::text(exchange),
        SqlParam::float(row.price),
        SqlParam::optional_float(row.bid),
        SqlParam::optional_float(row.ask),
        SqlParam::optional_float(row.previous_close),
        SqlParam::optional_float(row.change_pct),
        SqlParam::optional_integer(row.volume),
        SqlParam::optional_timestamp(row.as_of),
        SqlParam::timestamp(row.fetched_at.unwrap_or(now)),
        SqlParam::text_or(row.provider.as_deref(), DEFAULT_PRICE_PROVIDER),
    ])
}

/// A `MERGE` for one parameter-bounded block of rows.
fn merge_prices_sql(rows: usize) -> String {
    format!(
        "MERGE dbo.SymbolPriceCache AS target \
         USING (VALUES {}) AS source \
             (SymbolCode, Exchange, Price, Bid, Ask, PreviousClose, ChangePct, Volume, \
              AsOfUtc, LastUpdatedUtc, Source) \
             ON target.SymbolCode = source.SymbolCode AND target.Exchange = source.Exchange \
         WHEN MATCHED THEN UPDATE SET \
             Price = source.Price, Bid = source.Bid, Ask = source.Ask, \
             PreviousClose = source.PreviousClose, ChangePct = source.ChangePct, \
             Volume = source.Volume, \
             AsOfUtc = source.AsOfUtc, LastUpdatedUtc = source.LastUpdatedUtc, \
             Source = source.Source \
         WHEN NOT MATCHED THEN INSERT \
             (SymbolCode, Exchange, Price, Bid, Ask, PreviousClose, ChangePct, Volume, AsOfUtc, \
              LastUpdatedUtc, Source) \
         VALUES \
             (source.SymbolCode, source.Exchange, source.Price, source.Bid, source.Ask, \
              source.PreviousClose, source.ChangePct, source.Volume, source.AsOfUtc, \
              source.LastUpdatedUtc, source.Source);",
        values_rows(rows, PRICE_PARAMS_PER_ROW)
    )
}

/// Winners and losers share one query; only the comparison and the sort differ.
fn movers_sql(direction: &str, order: &str, since: bool) -> String {
    let freshness = if since {
        " AND COALESCE(AsOfUtc, LastUpdatedUtc) >= @P5"
    } else {
        ""
    };
    format!(
        "SELECT TOP (@P1) \
                SymbolCode, Price, ChangePct, Volume, PreviousClose, AsOfUtc, \
                Source, CAST(Price * Volume AS decimal(38, 2)) AS DollarVolume \
           FROM dbo.SymbolPriceCache \
          WHERE Exchange = @P2 \
            AND Price >= @P3 \
            AND ChangePct IS NOT NULL \
            AND Volume IS NOT NULL AND Volume > 0 \
            AND Price * Volume >= @P4 \
            AND ChangePct {direction} 0{freshness} \
       ORDER BY ChangePct {order}"
    )
}

/// The newest broad session's most-traded codes, by dollar volume.
const TOP_SYMBOLS_SQL: &str = "\
WITH broad AS ( \
    SELECT SnapshotDate FROM dbo.DailySnapshot \
     WHERE Exchange = @P1 \
     GROUP BY SnapshotDate \
    HAVING COUNT(*) >= @P2 \
), newest AS ( \
    SELECT TOP (1) SnapshotDate FROM broad ORDER BY SnapshotDate DESC \
) \
SELECT TOP (@P3) SymbolCode \
  FROM dbo.DailySnapshot d \
  JOIN newest n ON n.SnapshotDate = d.SnapshotDate \
 WHERE d.Exchange = @P4 \
   AND d.[Close] > 0 \
   AND d.Volume > 0 \
 ORDER BY d.[Close] * d.Volume DESC";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_merge_names_every_column_and_keeps_the_exchange_key() {
        let sql = merge_prices_sql(2);
        assert!(sql.contains("MERGE dbo.SymbolPriceCache AS target"));
        assert!(sql.contains("ON target.SymbolCode = source.SymbolCode"));
        assert!(sql.contains("AND target.Exchange = source.Exchange"));
        assert!(sql.contains("(@P1,@P2,@P3,@P4,@P5,@P6,@P7,@P8,@P9,@P10,@P11)"));
        assert!(sql.contains("(@P12,"));
        assert!(sql.trim_end().ends_with(';'));
    }

    #[test]
    fn a_merge_block_stays_under_the_parameter_ceiling() {
        let rows = rows_per_statement(PRICE_PARAMS_PER_ROW);
        assert_eq!(rows, 181);
        assert_eq!(merge_prices_sql(rows).matches("@P").count(), rows * 11);
    }

    #[test]
    fn price_parameters_follow_the_column_order() {
        let row = PriceRow {
            symbol: "AAPL".into(),
            exchange: Some("us".into()),
            price: 100.0,
            bid: Some(99.9),
            ask: None,
            previous_close: Some(98.0),
            change_pct: Some(2.0),
            volume: Some(5_000),
            as_of: None,
            fetched_at: None,
            provider: None,
        };
        let params = price_params(
            &row,
            NaiveDateTime::parse_from_str("2026-11-04 03:00:00", "%Y-%m-%d %H:%M:%S").unwrap(),
        )
        .unwrap();
        assert_eq!(params.len(), PRICE_PARAMS_PER_ROW);
        assert_eq!(params[0], SqlParam::text("AAPL"));
        assert_eq!(params[1], SqlParam::text("US"));
        assert_eq!(params[2], SqlParam::float(100.0));
        assert_eq!(
            params[9],
            SqlParam::timestamp(
                NaiveDateTime::parse_from_str("2026-11-04 03:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
            )
        );
        assert_eq!(params[10], SqlParam::text(DEFAULT_PRICE_PROVIDER));
    }

    #[test]
    fn a_price_row_without_a_exchange_is_refused_before_connecting() {
        let row = PriceRow {
            symbol: "AAPL".into(),
            exchange: None,
            price: 100.0,
            bid: None,
            ask: None,
            previous_close: None,
            change_pct: None,
            volume: None,
            as_of: None,
            fetched_at: None,
            provider: None,
        };
        assert!(matches!(
            price_params(&row, Utc::now().naive_utc()).unwrap_err(),
            StoreError::Invalid(_)
        ));
    }

    #[test]
    fn movers_add_the_freshness_parameter_only_when_asked() {
        let without = movers_sql(">=", "DESC", false);
        let with = movers_sql(">=", "DESC", true);
        assert!(!without.contains("@P5"));
        assert!(with.contains(">= @P5"));
        assert!(without.contains("ChangePct >= 0"));
        assert!(without.contains("ORDER BY ChangePct DESC"));
        assert!(movers_sql("<", "ASC", false).contains("ORDER BY ChangePct ASC"));
    }
}
