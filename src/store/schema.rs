//! Applying `sql/001_schema.sql`.
//!
//! The script guards every object with `IF ... IS NULL` / `NOT EXISTS`, so running it on
//! every start is safe and a fresh database needs no manual step. It is embedded with
//! `include_str!` so the service can start from any working directory, and the same file is
//! kept in `sql/` for DBAs to review and run by hand.
//!
//! `GO` is a client batch separator, not T-SQL: pyodbc could not execute it and had to split
//! the script, and TDS has the same rule. [`script_batches`] reproduces the source service's
//! splitter exactly — a line whose trimmed, upper-cased text is `GO`.

use tiberius::QueryStream;

use crate::providers::base::truncate_chars;

use super::connection::SqlServerStore;
use super::error::{SqlConnection, StoreError};

/// The schema, compiled into the binary.
pub const SCHEMA_SQL: &str = include_str!("../../sql/001_schema.sql");

/// Split a T-SQL script on `GO` and discard empty batches.
pub fn script_batches(sql_text: &str) -> Vec<String> {
    let mut batches = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in sql_text.lines() {
        if line.trim().eq_ignore_ascii_case("GO") {
            push_batch(&mut batches, &current);
            current.clear();
        } else {
            current.push(line);
        }
    }
    push_batch(&mut batches, &current);
    batches
}

fn push_batch(batches: &mut Vec<String>, lines: &[&str]) {
    let body = lines.join("\n").trim().to_string();
    if !body.is_empty() {
        batches.push(body);
    }
}

/// Apply the schema, returning the number of batches that ran.
///
/// The whole script runs in one transaction: a database that fails on batch 7 must not be
/// left with batches 1–6 applied and half a schema.
pub async fn ensure_schema(client: &mut SqlConnection) -> Result<usize, StoreError> {
    let batches = script_batches(SCHEMA_SQL);
    client.begin_transaction().await?;
    for (index, batch) in batches.iter().enumerate() {
        if let Err(error) = run_batch(client, batch).await {
            let _ = client.rollback_transaction().await;
            return Err(StoreError::database(format!(
                "schema batch {} failed: {error}\n--- batch ---\n{}",
                index + 1,
                truncate_chars(batch, 800)
            )));
        }
    }
    client.commit_transaction().await?;
    Ok(batches.len())
}

/// Run one batch and drain every result set it leaves open.
async fn run_batch(client: &mut SqlConnection, batch: &str) -> Result<(), tiberius::error::Error> {
    let stream: QueryStream<'_> = client.simple_query(batch).await?;
    stream.into_results().await?;
    Ok(())
}

impl SqlServerStore {
    /// Apply the schema on a pooled connection, returning the batch count.
    ///
    /// The service calls this on startup so a fresh database needs no manual step; a failure
    /// is fatal to startup, because serving from a half-created schema would fail later and
    /// less clearly.
    pub async fn ensure_schema(&self) -> Result<usize, StoreError> {
        let mut client = self.connection().await?;
        ensure_schema(&mut client).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_file_is_embedded_and_creates_every_owned_table() {
        assert!(SCHEMA_SQL.contains("dbo.SymbolPriceCache"));
        assert!(SCHEMA_SQL.contains("dbo.DailySnapshot"));
        assert!(SCHEMA_SQL.contains("dbo.IndicatorCache"));
        assert!(SCHEMA_SQL.contains("dbo.IntradayBar5Min"));
        assert!(SCHEMA_SQL.contains("dbo.BulkSnapshotPull"));
    }

    #[test]
    fn the_intraday_index_is_created_with_its_table() {
        // 002_indexes.sql is a manual load-quiet swap the service never applies, so an index
        // placed there would never exist for this table.
        assert!(SCHEMA_SQL.contains("IX_IntradayBar5Min_Symbol"));
        assert!(SCHEMA_SQL.contains("ON dbo.IntradayBar5Min (SymbolCode, Exchange, TsUtc DESC)"));
    }

    #[test]
    fn the_screener_tariff_row_is_guarded_and_flat_five() {
        assert!(SCHEMA_SQL.contains("RequestType = N''/screener''"));
        assert!(SCHEMA_SQL.contains("N''flat'', 5"));
    }

    #[test]
    fn go_is_split_on_a_line_of_its_own_in_any_case() {
        let batches = script_batches("SELECT 1\nGO\nselect 2\ngo\n  GO  \nSELECT 3");
        assert_eq!(batches, vec!["SELECT 1", "select 2", "SELECT 3"]);
    }

    #[test]
    fn a_go_inside_a_statement_is_not_a_separator() {
        let batches = script_batches("SELECT 'GO' AS marker\nGO");
        assert_eq!(batches, vec!["SELECT 'GO' AS marker"]);
    }

    #[test]
    fn empty_batches_are_discarded() {
        assert_eq!(script_batches("\nGO\n\nGO\n"), Vec::<String>::new());
    }

    #[test]
    fn the_real_script_has_no_empty_batches() {
        let batches = script_batches(SCHEMA_SQL);
        assert!(batches.len() > 1);
        assert!(batches.iter().all(|batch| !batch.trim().is_empty()));
    }
}
