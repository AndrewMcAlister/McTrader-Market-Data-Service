//! Liveness, row counts, and the exchange reference the clock reads.
//!
//! `ping` is deliberately not `stats`: health checks run often, and counting rows across
//! `DailySnapshot` on every one of them both costs real time and turns a health check into a
//! load source. `SELECT 1` still proves the connection, the login and the database.

use async_trait::async_trait;

use super::connection::SqlServerStore;
use super::error::StoreError;
use super::models::{ExchangeBarCount, ExchangeRow, StoreStats};
use super::query::{date, integer, query_rows, required_integer, required_text, text, timestamp};

/// One round trip that touches no table.
const PING_SQL: &str = "SELECT 1";

/// Row counts for `/stats` and the job's log line.
///
/// `daily_symbols` counts (symbol, exchange) pairs, not bare tickers: one ticker on two
/// exchanges is two series, and the count that matters is the number of series.
const STATS_SQL: &str = "\
SELECT
    (SELECT COUNT(*) FROM dbo.SymbolPriceCache),
    (SELECT COUNT(*) FROM dbo.DailySnapshot),
    (SELECT COUNT(*) FROM
        (SELECT DISTINCT SymbolCode, Exchange FROM dbo.DailySnapshot) AS s),
    (SELECT COUNT(*) FROM dbo.IndicatorCache),
    (SELECT MAX(SnapshotDate) FROM dbo.DailySnapshot),
    (SELECT MAX(LastUpdatedUtc) FROM dbo.SymbolPriceCache)";

const STATS_BY_EXCHANGE_SQL: &str = "\
SELECT Exchange, COUNT(*) AS bars
  FROM dbo.DailySnapshot
 GROUP BY Exchange
 ORDER BY Exchange";

/// The exchange reference table. A copy of `McTrader_Live.dbo.Exchanges`; the clock's primary
/// source, with `data/clock.json` as the fallback when the database is unavailable.
const EXCHANGES_SQL: &str = "\
SELECT Id, Code, Name, Country, Currency, Timezone, Opens, Closes
  FROM dbo.Exchanges
 ORDER BY Id";

/// Liveness and row counts.
#[async_trait]
pub trait OpsRepository: Send + Sync {
    async fn ping(&self) -> Result<(), StoreError>;

    async fn stats(&self) -> Result<StoreStats, StoreError>;
}

/// The clock's view of `dbo.Exchanges`.
#[async_trait]
pub trait ExchangeRepository: Send + Sync {
    async fn exchange_rows(&self) -> Result<Vec<ExchangeRow>, StoreError>;
}

#[async_trait]
impl OpsRepository for SqlServerStore {
    async fn ping(&self) -> Result<(), StoreError> {
        let mut client = self.connection().await?;
        client.simple_query(PING_SQL).await?.into_results().await?;
        Ok(())
    }

    async fn stats(&self) -> Result<StoreStats, StoreError> {
        let mut client = self.connection().await?;
        let rows = query_rows(&mut client, STATS_SQL, &[]).await?;
        let row = rows
            .first()
            .ok_or_else(|| StoreError::database("the stats query returned no row"))?;
        let price_rows = required_integer(row, 0, "SymbolPriceCache COUNT")?;
        let daily_rows = required_integer(row, 1, "DailySnapshot COUNT")?;
        let daily_symbols = required_integer(row, 2, "distinct series COUNT")?;
        let indicator_rows = required_integer(row, 3, "IndicatorCache COUNT")?;
        let latest_session = date(row, 4)?;
        let latest_price_at = timestamp(row, 5)?;

        let daily_by_exchange = query_rows(&mut client, STATS_BY_EXCHANGE_SQL, &[])
            .await?
            .iter()
            .map(|row| {
                Ok(ExchangeBarCount {
                    exchange: required_text(row, 0, "Exchange")?,
                    bars: required_integer(row, 1, "bars")?,
                })
            })
            .collect::<Result<_, StoreError>>()?;

        Ok(StoreStats {
            price_rows,
            daily_rows,
            daily_symbols,
            indicator_rows,
            latest_session,
            latest_price_at,
            daily_by_exchange,
        })
    }
}

#[async_trait]
impl ExchangeRepository for SqlServerStore {
    async fn exchange_rows(&self) -> Result<Vec<ExchangeRow>, StoreError> {
        let mut client = self.connection().await?;
        let rows = query_rows(&mut client, EXCHANGES_SQL, &[]).await?;
        rows.iter()
            .map(|row| {
                let id = integer(row, 0)?
                    .and_then(|value| i32::try_from(value).ok())
                    .ok_or_else(|| {
                        StoreError::database("Exchange Id is missing or out of range")
                    })?;
                Ok(ExchangeRow {
                    id,
                    code: text(row, 1)?,
                    name: text(row, 2)?,
                    country: text(row, 3)?,
                    // The source service normalised a missing currency to an empty string.
                    currency: Some(text(row, 4)?.unwrap_or_default().trim().to_string()),
                    timezone: text(row, 5)?,
                    opens: text(row, 6)?,
                    closes: text(row, 7)?,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stats_query_counts_series_rather_than_bare_tickers() {
        assert!(STATS_SQL.contains("FROM dbo.SymbolPriceCache"));
        assert!(STATS_SQL.contains("FROM dbo.DailySnapshot"));
        assert!(STATS_SQL.contains("SELECT DISTINCT SymbolCode, Exchange FROM dbo.DailySnapshot"));
        assert!(STATS_SQL.contains("FROM dbo.IndicatorCache"));
        assert!(STATS_SQL.contains("MAX(LastUpdatedUtc)"));
    }

    #[test]
    fn the_exchange_query_orders_by_id_for_a_stable_clock() {
        assert!(EXCHANGES_SQL.contains("FROM dbo.Exchanges"));
        assert!(EXCHANGES_SQL.contains("ORDER BY Id"));
        for column in [
            "Id", "Code", "Name", "Country", "Currency", "Timezone", "Opens", "Closes",
        ] {
            assert!(EXCHANGES_SQL.contains(column), "{column} is not selected");
        }
    }

    #[test]
    fn the_ping_touches_no_table() {
        assert_eq!(PING_SQL, "SELECT 1");
    }

    #[test]
    fn bars_are_grouped_by_exchange_in_name_order() {
        assert!(STATS_BY_EXCHANGE_SQL.contains("GROUP BY Exchange"));
        assert!(STATS_BY_EXCHANGE_SQL.contains("ORDER BY Exchange"));
    }
}
