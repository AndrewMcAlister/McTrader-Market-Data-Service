//! The statements only the operator commands run.
//!
//! None of this is on the service's request path, and none of it is part of
//! [`super::MarketDataStore`]: putting a schema report or a benchmark read on the repository
//! supertrait would force every test double to implement a method the service never calls.
//! It lives beside the tables it touches so the SQL stays in one place.
//!
//! The destructive statements are stated here in full rather than assembled by a caller. The
//! only text ever formatted into SQL is the source database of a `copy-exchanges` merge, and
//! it is refused unless it is a plain identifier ([`valid_identifier`]).

use chrono::NaiveDate;

use super::connection::SqlServerStore;
use super::error::StoreError;
use super::models::{DailyBarWrite, ExchangeRow};
use super::query::{
    boolean, date, decimal, execute, integer, query_rows, required_float, required_text, text,
    SqlParam,
};
use super::ExchangeRepository;

/// The rows of one session, in the shape the daily write path accepts.
const SESSION_BARS_SQL: &str = "\
SELECT SymbolCode, Exchange, SnapshotDate, [Open], [High], [Low], [Close], [RawClose],
       Volume, Source, IsAdjusted
  FROM dbo.DailySnapshot
 WHERE SnapshotDate = @P1
 ORDER BY SymbolCode";

const DELETE_DAILY_AFTER_SQL: &str = "DELETE FROM dbo.DailySnapshot WHERE SnapshotDate > @P1";

const CLEAR_INDICATORS_SQL: &str = "DELETE FROM dbo.IndicatorCache";

/// The three tables this service owns, documented in the estate's doc tables.
const DOCUMENTED_TABLES_SQL: &str = "\
SELECT TableName
  FROM dbo.TableDoc
 WHERE TableName IN ('SymbolPriceCache', 'DailySnapshot', 'IndicatorCache')
 ORDER BY TableName";

const DOCUMENTED_COLUMNS_SQL: &str = "\
SELECT TableName, ColumnName
  FROM dbo.ColumnDoc
 WHERE TableName IN ('SymbolPriceCache', 'DailySnapshot', 'IndicatorCache')
 ORDER BY TableName, ColumnName";

/// The widest tier of the latest session: the symbols `probe-bulk-route` asks for.
const TOP_DOLLAR_VOLUME_SQL: &str = "\
SELECT TOP (@P1) d.SymbolCode
  FROM dbo.DailySnapshot d
  JOIN dbo.Symbol s ON s.SymbolCode = d.SymbolCode
 WHERE d.SnapshotDate = @P2
 ORDER BY s.DollarVolume DESC";

/// The tool's copy of the exchange reference, created when the service database has none.
const CREATE_EXCHANGES_SQL: &str = "\
IF OBJECT_ID('dbo.Exchanges', 'U') IS NULL
BEGIN
    CREATE TABLE dbo.Exchanges (
        Id       tinyint       NOT NULL CONSTRAINT PK_Exchanges PRIMARY KEY,
        Code     varchar(20)   NOT NULL CONSTRAINT UQ_Exchanges_Code UNIQUE,
        Name     nvarchar(200) NULL,
        Country  varchar(100)  NULL,
        Currency char(3)       NULL,
        Timezone varchar(50)   NULL,
        Opens    varchar(5)    NULL,
        Closes   varchar(5)    NULL
    );
END";

/// The merge from the live reference, with the source database formatted in.
const MERGE_EXCHANGES_TEMPLATE: &str = "\
MERGE dbo.Exchanges AS t
USING (
    SELECT Id, Code, Name, Country, Currency, Timezone, Opens, Closes
    FROM {source}.dbo.Exchanges
) AS s
ON t.Id = s.Id
WHEN MATCHED THEN UPDATE SET
    Code = s.Code, Name = s.Name, Country = s.Country, Currency = s.Currency,
    Timezone = s.Timezone, Opens = s.Opens, Closes = s.Closes
WHEN NOT MATCHED BY TARGET THEN
    INSERT (Id, Code, Name, Country, Currency, Timezone, Opens, Closes)
    VALUES (s.Id, s.Code, s.Name, s.Country, s.Currency, s.Timezone, s.Opens, s.Closes)
WHEN NOT MATCHED BY SOURCE THEN DELETE";

/// Record the copy in the estate's documentation, updating or inserting.
const DOCUMENT_EXCHANGES_SQL: &str = "\
IF EXISTS (SELECT 1 FROM dbo.TableDoc WHERE TableName = 'Exchanges')
    UPDATE dbo.TableDoc
       SET Grain = 'one row per exchange', Purpose = @P1, RowCountNote = @P2, DataSource = @P3
     WHERE TableName = 'Exchanges'
ELSE
    INSERT INTO dbo.TableDoc (TableName, Grain, Purpose, RowCountNote, DataSource)
    VALUES ('Exchanges', 'one row per exchange', @P1, @P2, @P3);";

/// What a cleanup removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupCounts {
    pub daily_rows: u64,
    pub indicator_rows: u64,
}

/// The documentation rows proving the owned tables were described.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaDocumentation {
    pub tables: Vec<String>,
    /// `TableName.ColumnName`, in table then column order.
    pub columns: Vec<String>,
}

impl SqlServerStore {
    /// Every row of one session, ready to be written back through the same `MERGE`.
    pub async fn session_bars(&self, session: NaiveDate) -> Result<Vec<DailyBarWrite>, StoreError> {
        let mut client = self.connection().await?;
        let rows = query_rows(&mut client, SESSION_BARS_SQL, &[SqlParam::date(session)]).await?;
        rows.iter()
            .map(|row| {
                Ok(DailyBarWrite {
                    symbol: required_text(row, 0, "SymbolCode")?,
                    exchange: text(row, 1)?,
                    session: date(row, 2)?
                        .ok_or_else(|| StoreError::database("SnapshotDate was NULL"))?,
                    open: required_float(row, 3, "[Open]")?,
                    high: required_float(row, 4, "[High]")?,
                    low: required_float(row, 5, "[Low]")?,
                    close: required_float(row, 6, "[Close]")?,
                    raw_close: decimal(row, 7)?,
                    volume: integer(row, 8)?,
                    source: text(row, 9)?,
                    adjusted: boolean(row, 10)?,
                })
            })
            .collect()
    }

    /// Delete every daily row after `cutoff` and clear the indicator cache, atomically.
    ///
    /// `DailySnapshot` is append-only by session, so undoing a bad load is a surgical delete
    /// followed by a rebuild rather than a table rewrite.
    pub async fn delete_sessions_after(
        &self,
        cutoff: NaiveDate,
    ) -> Result<CleanupCounts, StoreError> {
        let mut client = self.connection().await?;
        client.begin_transaction().await?;
        let daily_rows = match execute(
            &mut client,
            DELETE_DAILY_AFTER_SQL,
            &[SqlParam::date(cutoff)],
        )
        .await
        {
            Ok(rows) => rows,
            Err(error) => {
                let _ = client.rollback_transaction().await;
                return Err(error);
            }
        };
        let indicator_rows = match execute(&mut client, CLEAR_INDICATORS_SQL, &[]).await {
            Ok(rows) => rows,
            Err(error) => {
                let _ = client.rollback_transaction().await;
                return Err(error);
            }
        };
        client.commit_transaction().await?;
        Ok(CleanupCounts {
            daily_rows,
            indicator_rows,
        })
    }

    /// The doc rows for the three tables this service owns.
    pub async fn schema_documentation(&self) -> Result<SchemaDocumentation, StoreError> {
        let mut client = self.connection().await?;
        let tables = query_rows(&mut client, DOCUMENTED_TABLES_SQL, &[])
            .await?
            .iter()
            .map(|row| required_text(row, 0, "TableName"))
            .collect::<Result<Vec<_>, _>>()?;
        let columns = query_rows(&mut client, DOCUMENTED_COLUMNS_SQL, &[])
            .await?
            .iter()
            .map(|row| {
                Ok(format!(
                    "{}.{}",
                    required_text(row, 0, "TableName")?,
                    required_text(row, 1, "ColumnName")?
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok(SchemaDocumentation { tables, columns })
    }

    /// The widest tier of a session, by the estate's dollar-volume ranking.
    pub async fn top_dollar_volume_symbols(
        &self,
        session: NaiveDate,
        limit: i64,
    ) -> Result<Vec<String>, StoreError> {
        let mut client = self.connection().await?;
        let rows = query_rows(
            &mut client,
            TOP_DOLLAR_VOLUME_SQL,
            &[SqlParam::integer(limit), SqlParam::date(session)],
        )
        .await?;
        rows.iter()
            .map(|row| required_text(row, 0, "SymbolCode"))
            .collect()
    }

    /// Copy `source.dbo.Exchanges` into this database and record it in `TableDoc`.
    ///
    /// Idempotent: rows are merged by `Id`, so a re-run refreshes the copy rather than
    /// duplicating it, and a row deleted at the source is deleted here.
    pub async fn copy_exchanges_from(&self, source: &str) -> Result<Vec<ExchangeRow>, StoreError> {
        if !valid_identifier(source) {
            return Err(StoreError::invalid(format!(
                "refusing source database {source:?}: not a plain identifier"
            )));
        }
        let merge = MERGE_EXCHANGES_TEMPLATE.replace("{source}", source);
        let purpose = format!(
            "Exchange session definitions copied from {source}.dbo.Exchanges: Code, Timezone, \
             Opens and Closes in the exchange's own local time. Drives the MMDS market clock \
             (/market/clock, /market/session, /market/exchanges) instead of the frozen \
             data/clock.json exchange list."
        );
        let doc = [
            SqlParam::text(purpose),
            SqlParam::text("13 rows"),
            SqlParam::text(source),
        ];

        let mut client = self.connection().await?;
        client.begin_transaction().await?;
        if let Err(error) = run_simple(&mut client, CREATE_EXCHANGES_SQL).await {
            let _ = client.rollback_transaction().await;
            return Err(error);
        }
        if let Err(error) = execute(&mut client, &merge, &[]).await {
            let _ = client.rollback_transaction().await;
            return Err(error);
        }
        if let Err(error) = execute(&mut client, DOCUMENT_EXCHANGES_SQL, &doc).await {
            let _ = client.rollback_transaction().await;
            return Err(error);
        }
        client.commit_transaction().await?;
        // A separate pooled connection reads the committed copy, so the report can never show
        // rows from an uncommitted transaction.
        drop(client);
        self.exchange_rows().await
    }
}

/// A source database name must be a plain identifier before it reaches a `MERGE`.
pub fn valid_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    match characters.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Run a batch that takes no parameters and drain every result set it leaves open.
async fn run_simple(client: &mut super::error::SqlConnection, sql: &str) -> Result<(), StoreError> {
    client.simple_query(sql).await?.into_results().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_database_name_is_accepted() {
        assert!(valid_identifier("McTrader_Live"));
        assert!(valid_identifier("McTrader_Paper"));
        assert!(valid_identifier("_staging"));
        assert!(valid_identifier("Market2026"));
    }

    #[test]
    fn anything_that_could_end_the_identifier_is_refused() {
        for name in [
            "",
            "1Market",
            "Market_Data; DROP TABLE x",
            "Market-Data",
            "Market Data",
            "[Market]",
            "..\\x",
        ] {
            assert!(!valid_identifier(name), "{name:?} was accepted");
        }
    }

    #[test]
    fn the_cleanup_only_deletes_after_the_cutoff_and_clears_the_cache() {
        assert_eq!(
            DELETE_DAILY_AFTER_SQL,
            "DELETE FROM dbo.DailySnapshot WHERE SnapshotDate > @P1"
        );
        assert_eq!(CLEAR_INDICATORS_SQL, "DELETE FROM dbo.IndicatorCache");
    }

    #[test]
    fn the_exchange_merge_is_keyed_on_id_and_removes_deleted_rows() {
        let merge = MERGE_EXCHANGES_TEMPLATE.replace("{source}", "McTrader_Live");
        assert!(merge.contains("FROM McTrader_Live.dbo.Exchanges"));
        assert!(merge.contains("ON t.Id = s.Id"));
        assert!(merge.contains("WHEN NOT MATCHED BY SOURCE THEN DELETE"));
        assert!(!merge.contains("{source}"));
    }

    #[test]
    fn the_benchmark_reads_the_session_in_symbol_order() {
        assert!(SESSION_BARS_SQL.contains("WHERE SnapshotDate = @P1"));
        assert!(SESSION_BARS_SQL.contains("ORDER BY SymbolCode"));
        for column in ["[RawClose]", "Volume", "Source", "IsAdjusted"] {
            assert!(
                SESSION_BARS_SQL.contains(column),
                "{column} is not selected"
            );
        }
    }

    #[test]
    fn the_wide_tier_is_ordered_by_the_estate_dollar_volume() {
        assert!(TOP_DOLLAR_VOLUME_SQL.contains("SELECT TOP (@P1)"));
        assert!(TOP_DOLLAR_VOLUME_SQL.contains("JOIN dbo.Symbol s"));
        assert!(TOP_DOLLAR_VOLUME_SQL.contains("ORDER BY s.DollarVolume DESC"));
    }
}
