//! The vendor-spend tables: `dbo.RequestTypes` read, `dbo.EODHDUsage` written.
//!
//! `record_usage` is UPDATE-then-INSERT rather than `MERGE`. `MERGE` has a well-known race
//! under concurrency, and the INSERT can lose a race here too, so a duplicate-key failure
//! retries the UPDATE once. At this volume a collision is rare; losing the row because of one
//! would be the accounting hole this table exists to close.

use async_trait::async_trait;
use chrono::NaiveDate;

use crate::usage::{today_local, TariffRow};

use super::connection::SqlServerStore;
use super::error::{is_duplicate_key_error, StoreError};
use super::models::{UsageByCaller, UsageByRequestType, UsageSummary};
use super::query::{
    bind, execute, integer, query_rows, required_integer, required_text, text, SqlParam,
};

/// Add one batch of calls to an existing ledger row.
const UPDATE_USAGE: &str = "\
UPDATE dbo.EODHDUsage
   SET Calls = Calls + @P1,
       Symbols = Symbols + @P2,
       CostTotal = CostTotal + @P3,
       FirstSeen = CASE WHEN SYSDATETIME() < FirstSeen
                        THEN SYSDATETIME() ELSE FirstSeen END,
       LastSeen = CASE WHEN SYSDATETIME() > LastSeen
                       THEN SYSDATETIME() ELSE LastSeen END
 WHERE UsageDate = @P4 AND Caller = @P5 AND RequestType = @P6";

/// Create today's ledger row when the UPDATE matched nothing.
const INSERT_USAGE: &str = "\
INSERT INTO dbo.EODHDUsage
    (UsageDate, Caller, RequestType, Calls, Symbols, CostTotal, FirstSeen, LastSeen)
VALUES (@P1, @P2, @P3, @P4, @P5, @P6, SYSDATETIME(), SYSDATETIME())";

/// The measured tariff. Read once at startup and cached by the caller.
const REQUEST_TYPES_SQL: &str = "\
SELECT RequestType, CostBasis, CostEach
  FROM dbo.RequestTypes
 ORDER BY RequestType";

/// How many calls of one type the ledger records for the day.
const BULK_PULL_COUNT_SQL: &str = "\
SELECT ISNULL(SUM(Calls), 0)
  FROM dbo.EODHDUsage
 WHERE UsageDate = @P1 AND RequestType = @P2";

/// The day's totals, and the two breakdowns that explain them.
const USAGE_TOTALS_SQL: &str = "\
SELECT ISNULL(SUM(Calls), 0), ISNULL(SUM(Symbols), 0), ISNULL(SUM(CostTotal), 0), COUNT(*)
  FROM dbo.EODHDUsage
 WHERE UsageDate = @P1";

const USAGE_BY_TYPE_SQL: &str = "\
SELECT RequestType, SUM(Calls), SUM(Symbols), SUM(CostTotal)
  FROM dbo.EODHDUsage
 WHERE UsageDate = @P1
 GROUP BY RequestType
 ORDER BY SUM(CostTotal) DESC, RequestType";

const USAGE_BY_CALLER_SQL: &str = "\
SELECT Caller, SUM(Calls), SUM(CostTotal)
  FROM dbo.EODHDUsage
 WHERE UsageDate = @P1
 GROUP BY Caller
 ORDER BY SUM(CostTotal) DESC, Caller";

/// Reads the tariff and reads/writes the daily spend ledger.
#[async_trait]
pub trait UsageRepository: Send + Sync {
    async fn request_types(&self) -> Result<Vec<TariffRow>, StoreError>;

    async fn record_usage(
        &self,
        request_type: &str,
        caller: &str,
        calls: i64,
        symbols: i64,
        cost: i64,
        usage_date: Option<NaiveDate>,
    ) -> Result<(), StoreError>;

    async fn bulk_pull_count(
        &self,
        request_type: &str,
        usage_date: Option<NaiveDate>,
    ) -> Result<i64, StoreError>;

    async fn usage_summary(
        &self,
        usage_date: Option<NaiveDate>,
    ) -> Result<UsageSummary, StoreError>;
}

#[async_trait]
impl UsageRepository for SqlServerStore {
    async fn request_types(&self) -> Result<Vec<TariffRow>, StoreError> {
        let mut client = self.connection().await?;
        let rows = query_rows(&mut client, REQUEST_TYPES_SQL, &[]).await?;
        rows.iter()
            .map(|row| {
                Ok(TariffRow {
                    request_type: required_text(row, 0, "RequestType")?,
                    cost_basis: text(row, 1)?,
                    cost_each: integer(row, 2)?,
                })
            })
            .collect()
    }

    async fn record_usage(
        &self,
        request_type: &str,
        caller: &str,
        calls: i64,
        symbols: i64,
        cost: i64,
        usage_date: Option<NaiveDate>,
    ) -> Result<(), StoreError> {
        let day = usage_date.unwrap_or_else(today_local);
        let update_params = [
            SqlParam::integer(calls),
            SqlParam::integer(symbols),
            SqlParam::integer(cost),
            SqlParam::date(day),
            SqlParam::text(caller),
            SqlParam::text(request_type),
        ];
        let insert_params = [
            SqlParam::date(day),
            SqlParam::text(caller),
            SqlParam::text(request_type),
            SqlParam::integer(calls),
            SqlParam::integer(symbols),
            SqlParam::integer(cost),
        ];

        let mut client = self.connection().await?;
        client.begin_transaction().await?;
        let updated = match execute(&mut client, UPDATE_USAGE, &update_params).await {
            Ok(updated) => updated,
            Err(error) => {
                let _ = client.rollback_transaction().await;
                return Err(error);
            }
        };
        if updated == 0 {
            let bound = bind(&insert_params);
            match client.execute(INSERT_USAGE, &bound).await {
                Ok(_) => {}
                Err(error) if is_duplicate_key_error(&error) => {
                    // Another thread inserted the same key between the UPDATE and the INSERT.
                    if let Err(retry) = execute(&mut client, UPDATE_USAGE, &update_params).await {
                        let _ = client.rollback_transaction().await;
                        return Err(retry);
                    }
                }
                Err(error) => {
                    let _ = client.rollback_transaction().await;
                    return Err(error.into());
                }
            }
        }
        client.commit_transaction().await?;
        Ok(())
    }

    async fn bulk_pull_count(
        &self,
        request_type: &str,
        usage_date: Option<NaiveDate>,
    ) -> Result<i64, StoreError> {
        let day = usage_date.unwrap_or_else(today_local);
        let mut client = self.connection().await?;
        let rows = query_rows(
            &mut client,
            BULK_PULL_COUNT_SQL,
            &[SqlParam::date(day), SqlParam::text(request_type)],
        )
        .await?;
        match rows.first() {
            Some(row) => required_integer(row, 0, "SUM(Calls)"),
            None => Ok(0),
        }
    }

    async fn usage_summary(
        &self,
        usage_date: Option<NaiveDate>,
    ) -> Result<UsageSummary, StoreError> {
        let day = usage_date.unwrap_or_else(today_local);
        let mut client = self.connection().await?;
        let params = [SqlParam::date(day)];

        let totals = query_rows(&mut client, USAGE_TOTALS_SQL, &params).await?;
        let row = totals
            .first()
            .ok_or_else(|| StoreError::database("the usage totals query returned no row"))?;
        let calls = required_integer(row, 0, "SUM(Calls)")?;
        let symbols = required_integer(row, 1, "SUM(Symbols)")?;
        let cost = required_integer(row, 2, "SUM(CostTotal)")?;
        let rows = required_integer(row, 3, "COUNT(*)")?;

        let by_type = query_rows(&mut client, USAGE_BY_TYPE_SQL, &params)
            .await?
            .iter()
            .map(|row| {
                Ok(UsageByRequestType {
                    request_type: text(row, 0)?.unwrap_or_default(),
                    calls: integer(row, 1)?.unwrap_or(0),
                    symbols: integer(row, 2)?.unwrap_or(0),
                    cost: integer(row, 3)?.unwrap_or(0),
                })
            })
            .collect::<Result<_, StoreError>>()?;

        let by_caller = query_rows(&mut client, USAGE_BY_CALLER_SQL, &params)
            .await?
            .iter()
            .map(|row| {
                Ok(UsageByCaller {
                    caller: text(row, 0)?.unwrap_or_default(),
                    calls: integer(row, 1)?.unwrap_or(0),
                    cost: integer(row, 2)?.unwrap_or(0),
                })
            })
            .collect::<Result<_, StoreError>>()?;

        Ok(UsageSummary {
            date: day,
            calls,
            symbols,
            cost,
            rows,
            by_request_type: by_type,
            by_caller,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_update_adds_to_every_counter_and_keeps_the_first_and_last_seen() {
        assert!(UPDATE_USAGE.contains("Calls = Calls + @P1"));
        assert!(UPDATE_USAGE.contains("Symbols = Symbols + @P2"));
        assert!(UPDATE_USAGE.contains("CostTotal = CostTotal + @P3"));
        assert!(UPDATE_USAGE.contains("WHEN SYSDATETIME() < FirstSeen"));
        assert!(UPDATE_USAGE.contains("WHEN SYSDATETIME() > LastSeen"));
        assert!(UPDATE_USAGE.contains("UsageDate = @P4 AND Caller = @P5 AND RequestType = @P6"));
    }

    #[test]
    fn the_insert_names_every_column_a_new_ledger_row_needs() {
        assert!(INSERT_USAGE.contains("(UsageDate, Caller, RequestType, Calls, Symbols, CostTotal"));
        assert!(INSERT_USAGE.contains("SYSDATETIME(), SYSDATETIME()"));
    }

    #[test]
    fn the_tariff_is_read_in_a_stable_order() {
        assert!(REQUEST_TYPES_SQL.contains("FROM dbo.RequestTypes"));
        assert!(REQUEST_TYPES_SQL.contains("ORDER BY RequestType"));
    }

    #[test]
    fn freshness_is_a_sum_over_calls_rather_than_a_row_count() {
        assert!(BULK_PULL_COUNT_SQL.contains("ISNULL(SUM(Calls), 0)"));
        assert!(BULK_PULL_COUNT_SQL.contains("UsageDate = @P1"));
    }

    #[test]
    fn the_summary_breaks_spend_down_by_type_and_caller() {
        assert!(USAGE_BY_TYPE_SQL.contains("GROUP BY RequestType"));
        assert!(USAGE_BY_TYPE_SQL.contains("ORDER BY SUM(CostTotal) DESC, RequestType"));
        assert!(USAGE_BY_CALLER_SQL.contains("GROUP BY Caller"));
        assert!(USAGE_BY_CALLER_SQL.contains("ORDER BY SUM(CostTotal) DESC, Caller"));
    }
}
