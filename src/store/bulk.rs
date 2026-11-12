//! `BulkSnapshotPull`: the completeness marker for a whole-market daily pull.
//!
//! A row here says "this market was fully pulled for this session". That is a different claim
//! from `DailySnapshot`'s "this symbol traded that day": the latest session in `DailySnapshot`
//! can hold 1,881 rows against the ~27,714 a completed pull writes, and a partial set silently
//! changes every derived return that reads it. `/bars/bulk` therefore serves a cached session
//! only when this table records a completed pull for it.
//!
//! The column is `Market`, not `Exchange`, because the bulk pull is keyed by the vendor's
//! market parameter and the endpoint's response uses the same word.

use async_trait::async_trait;
use chrono::NaiveDate;

use super::connection::SqlServerStore;
use super::error::StoreError;
use super::query::{date, execute, query_rows, required_integer, SqlParam};

/// One completed whole-market pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkPullMarker {
    pub session: NaiveDate,
    pub rows: i64,
}

/// Reads and writes the completeness marker.
#[async_trait]
pub trait BulkPullRepository: Send + Sync {
    /// The row count a completed pull wrote for `(market, session)`, or `None`.
    async fn bulk_pull_marker(
        &self,
        market: &str,
        session: NaiveDate,
    ) -> Result<Option<i64>, StoreError>;

    /// The newest completed pull for `market` strictly before `session`.
    ///
    /// This is the baseline the completeness check compares a fresh pull against: a session
    /// carrying a fraction of the codes the previous complete session had is a partial write,
    /// not a quiet market.
    async fn latest_bulk_pull_before(
        &self,
        market: &str,
        session: NaiveDate,
    ) -> Result<Option<BulkPullMarker>, StoreError>;

    async fn record_bulk_pull(
        &self,
        market: &str,
        session: NaiveDate,
        rows: i64,
        provider: &str,
    ) -> Result<(), StoreError>;
}

/// The marker for one `(market, session)`.
const MARKER_SQL: &str =
    "SELECT RowCount FROM dbo.BulkSnapshotPull WHERE Market = @P1 AND SessionDate = @P2";

/// The newest marker before a session, which is the completeness baseline.
const LATEST_BEFORE_SQL: &str = "\
SELECT TOP (1) SessionDate, RowCount FROM dbo.BulkSnapshotPull \
 WHERE Market = @P1 AND SessionDate < @P2 \
 ORDER BY SessionDate DESC";

/// Insert-or-update one marker. A re-pull of a completed session updates the count it recorded.
const RECORD_SQL: &str = "\
MERGE dbo.BulkSnapshotPull AS target \
USING (SELECT @P1 AS Market, @P2 AS SessionDate) AS source \
    ON target.Market = source.Market AND target.SessionDate = source.SessionDate \
WHEN MATCHED THEN UPDATE SET \
    RowCount = @P3, Provider = @P4, CompletedUtc = SYSUTCDATETIME() \
WHEN NOT MATCHED THEN INSERT (Market, SessionDate, RowCount, Provider) \
    VALUES (@P1, @P2, @P3, @P4);";

#[async_trait]
impl BulkPullRepository for SqlServerStore {
    async fn bulk_pull_marker(
        &self,
        market: &str,
        session: NaiveDate,
    ) -> Result<Option<i64>, StoreError> {
        let mut client = self.connection().await?;
        let rows = query_rows(
            &mut client,
            MARKER_SQL,
            &[SqlParam::text(market), SqlParam::date(session)],
        )
        .await?;
        match rows.first() {
            Some(row) => Ok(Some(required_integer(row, 0, "RowCount")?)),
            None => Ok(None),
        }
    }

    async fn latest_bulk_pull_before(
        &self,
        market: &str,
        session: NaiveDate,
    ) -> Result<Option<BulkPullMarker>, StoreError> {
        let mut client = self.connection().await?;
        let rows = query_rows(
            &mut client,
            LATEST_BEFORE_SQL,
            &[SqlParam::text(market), SqlParam::date(session)],
        )
        .await?;
        match rows.first() {
            Some(row) => Ok(Some(BulkPullMarker {
                session: date(row, 0)?
                    .ok_or_else(|| StoreError::database("SessionDate was NULL"))?,
                rows: required_integer(row, 1, "RowCount")?,
            })),
            None => Ok(None),
        }
    }

    async fn record_bulk_pull(
        &self,
        market: &str,
        session: NaiveDate,
        rows: i64,
        provider: &str,
    ) -> Result<(), StoreError> {
        let mut client = self.connection().await?;
        execute(
            &mut client,
            RECORD_SQL,
            &[
                SqlParam::text(market),
                SqlParam::date(session),
                SqlParam::integer(rows),
                SqlParam::text(provider),
            ],
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marker_is_a_session_and_a_row_count() {
        let marker = BulkPullMarker {
            session: NaiveDate::from_ymd_opt(2026, 11, 11).unwrap(),
            rows: 27_714,
        };
        assert_eq!(marker.session.to_string(), "2026-11-11");
        assert_eq!(marker.rows, 27_714);
    }

    #[test]
    fn every_marker_statement_keys_on_market_and_session_never_exchange() {
        for sql in [MARKER_SQL, LATEST_BEFORE_SQL, RECORD_SQL] {
            assert!(sql.contains("Market"), "{sql}");
            assert!(sql.contains("SessionDate"), "{sql}");
            assert!(!sql.contains("Exchange"), "{sql}");
        }
    }

    #[test]
    fn the_baseline_is_the_newest_pull_strictly_before_the_session() {
        assert!(LATEST_BEFORE_SQL.contains("SessionDate < @P2"));
        assert!(LATEST_BEFORE_SQL.contains("ORDER BY SessionDate DESC"));
        assert!(LATEST_BEFORE_SQL.contains("TOP (1)"));
    }

    #[test]
    fn a_re_pull_updates_the_recorded_count_rather_than_duplicating_the_row() {
        assert_eq!(RECORD_SQL.matches("MERGE dbo.BulkSnapshotPull").count(), 1);
        assert!(RECORD_SQL.contains("WHEN MATCHED THEN UPDATE SET"));
        assert!(RECORD_SQL.contains("RowCount = @P3"));
        assert!(RECORD_SQL.contains("WHEN NOT MATCHED THEN INSERT (Market, SessionDate, RowCount"));
    }
}
