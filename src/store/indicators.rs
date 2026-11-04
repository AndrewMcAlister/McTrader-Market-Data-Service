//! `IndicatorCache` reads and writes.
//!
//! The cache is keyed by `(SymbolCode, Exchange)`, so the same ticker can hold a US ATR and an
//! ASX one. Reads and writes therefore take the exchange from the same place every time; a read
//! that says nothing means US, a write that says nothing is refused.
//!
//! `Atr14`..`Atr50` are five columns rather than a period-keyed table because they are the
//! periods the estate asks for; a period outside the schema's five cannot be cached and is
//! simply not written.

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;

use super::connection::SqlServerStore;
use super::error::StoreError;
use super::keys::{normalise_symbols, read_exchange_key, required_exchange_key};
use super::models::{IndicatorWrite, StoredIndicator};
use super::query::{
    date, decimal, execute, integer32, query_rows, required_text, text, timestamp, SqlParam,
};
use super::sql::{placeholders, rows_per_statement, values_rows};

/// Read chunk size from the source service.
pub const INDICATOR_READ_CHUNK: usize = 1_000;

/// Parameters per indicator row: the twelve columns a writer states.
const INDICATOR_PARAMS_PER_ROW: usize = 12;

/// The ATR periods the schema holds, in column order.
pub const ATR_PERIODS: [i64; 5] = [14, 20, 22, 26, 50];

/// The method recorded when a writer does not state one.
pub const DEFAULT_METHOD: &str = "tr";

/// The smoothing recorded when a writer does not state one.
pub const DEFAULT_SMOOTHING: &str = "simple";

/// Reads and writes the indicator cache.
#[async_trait]
pub trait IndicatorRepository: Send + Sync {
    async fn get_indicators(
        &self,
        symbols: &[String],
        exchange: &str,
    ) -> Result<HashMap<String, StoredIndicator>, StoreError>;

    async fn upsert_indicators(&self, rows: Vec<IndicatorWrite>) -> Result<usize, StoreError>;
}

#[async_trait]
impl IndicatorRepository for SqlServerStore {
    async fn get_indicators(
        &self,
        symbols: &[String],
        exchange: &str,
    ) -> Result<HashMap<String, StoredIndicator>, StoreError> {
        let wanted = normalise_symbols(symbols);
        if wanted.is_empty() {
            return Ok(HashMap::new());
        }
        let exchange = read_exchange_key(Some(exchange));
        let mut client = self.connection().await?;
        let mut out = HashMap::with_capacity(wanted.len());
        for chunk in wanted.chunks(INDICATOR_READ_CHUNK) {
            let sql = format!(
                "SELECT SymbolCode, Atr14, Atr20, Atr22, Atr26, Atr50, ReferencePrice, \
                        SessionsUsed, AsOfSession, Method, Smoothing, UpdatedUtc \
                   FROM dbo.IndicatorCache \
                  WHERE Exchange = @P1 AND SymbolCode IN ({})",
                placeholders(2, chunk.len())
            );
            let mut params = Vec::with_capacity(chunk.len() + 1);
            params.push(SqlParam::text(&exchange));
            params.extend(chunk.iter().map(SqlParam::text));
            let rows = query_rows(&mut client, &sql, &params).await?;
            for row in &rows {
                let symbol = required_text(row, 0, "SymbolCode")?.to_uppercase();
                let mut values = BTreeMap::new();
                for (offset, period) in ATR_PERIODS.iter().enumerate() {
                    values.insert(*period, decimal(row, 1 + offset)?);
                }
                out.insert(
                    symbol.clone(),
                    StoredIndicator {
                        symbol,
                        values,
                        reference_price: decimal(row, 6)?,
                        sessions_used: integer32(row, 7)?,
                        as_of_session: date(row, 8)?,
                        method: text(row, 9)?,
                        smoothing: text(row, 10)?,
                        updated_at: timestamp(row, 11)?,
                    },
                );
            }
        }
        Ok(out)
    }

    async fn upsert_indicators(&self, rows: Vec<IndicatorWrite>) -> Result<usize, StoreError> {
        if rows.is_empty() {
            return Ok(0);
        }
        let payload: Vec<Vec<SqlParam>> = rows
            .iter()
            .map(indicator_params)
            .collect::<Result<_, _>>()?;
        let attempted = payload.len();

        let mut client = self.connection().await?;
        client.begin_transaction().await?;
        for chunk in payload.chunks(rows_per_statement(INDICATOR_PARAMS_PER_ROW)) {
            let sql = merge_indicators_sql(chunk.len());
            let params: Vec<SqlParam> = chunk.iter().flatten().cloned().collect();
            if let Err(error) = execute(&mut client, &sql, &params).await {
                let _ = client.rollback_transaction().await;
                return Err(error);
            }
        }
        client.commit_transaction().await?;
        Ok(attempted)
    }
}

/// One row's twelve parameters, in the column order [`merge_indicators_sql`] states.
fn indicator_params(row: &IndicatorWrite) -> Result<Vec<SqlParam>, StoreError> {
    let exchange = required_exchange_key(row.exchange.as_deref())?;
    Ok(vec![
        SqlParam::text(&row.symbol),
        SqlParam::text(exchange),
        SqlParam::optional_float(row.value(14)),
        SqlParam::optional_float(row.value(20)),
        SqlParam::optional_float(row.value(22)),
        SqlParam::optional_float(row.value(26)),
        SqlParam::optional_float(row.value(50)),
        SqlParam::optional_float(row.reference_price),
        SqlParam::optional_integer32(row.sessions_used),
        SqlParam::optional_date(row.as_of_session),
        SqlParam::text_or(row.method.as_deref(), DEFAULT_METHOD),
        SqlParam::text_or(row.smoothing.as_deref(), DEFAULT_SMOOTHING),
    ])
}

/// A `MERGE` for one parameter-bounded block of indicator rows.
fn merge_indicators_sql(rows: usize) -> String {
    format!(
        "MERGE dbo.IndicatorCache AS target \
         USING (VALUES {}) AS source \
             (SymbolCode, Exchange, Atr14, Atr20, Atr22, Atr26, Atr50, ReferencePrice, \
              SessionsUsed, AsOfSession, Method, Smoothing) \
             ON target.SymbolCode = source.SymbolCode AND target.Exchange = source.Exchange \
         WHEN MATCHED THEN UPDATE SET \
             Atr14 = source.Atr14, Atr20 = source.Atr20, Atr22 = source.Atr22, \
             Atr26 = source.Atr26, Atr50 = source.Atr50, \
             ReferencePrice = source.ReferencePrice, \
             SessionsUsed = source.SessionsUsed, AsOfSession = source.AsOfSession, \
             Method = source.Method, Smoothing = source.Smoothing, \
             UpdatedUtc = SYSUTCDATETIME() \
         WHEN NOT MATCHED THEN INSERT \
             (SymbolCode, Exchange, Atr14, Atr20, Atr22, Atr26, Atr50, ReferencePrice, \
              SessionsUsed, AsOfSession, Method, Smoothing) \
         VALUES \
             (source.SymbolCode, source.Exchange, source.Atr14, source.Atr20, source.Atr22, \
              source.Atr26, source.Atr50, source.ReferencePrice, source.SessionsUsed, \
              source.AsOfSession, source.Method, source.Smoothing);",
        values_rows(rows, INDICATOR_PARAMS_PER_ROW)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(exchange: Option<&str>) -> IndicatorWrite {
        IndicatorWrite {
            symbol: "AAPL".into(),
            exchange: exchange.map(str::to_string),
            values: [(14, Some(1.5)), (20, None), (22, Some(2.5))]
                .into_iter()
                .collect(),
            reference_price: Some(100.0),
            sessions_used: Some(21),
            as_of_session: None,
            method: None,
            smoothing: None,
        }
    }

    #[test]
    fn the_merge_names_every_column_and_keeps_the_exchange_key() {
        let sql = merge_indicators_sql(2);
        assert!(sql.contains("MERGE dbo.IndicatorCache AS target"));
        assert!(sql.contains("ON target.SymbolCode = source.SymbolCode"));
        assert!(sql.contains("AND target.Exchange = source.Exchange"));
        assert!(sql.contains("UpdatedUtc = SYSUTCDATETIME()"));
        assert!(sql.contains("(@P1,@P2,@P3,@P4,@P5,@P6,@P7,@P8,@P9,@P10,@P11,@P12)"));
        assert!(sql.contains("(@P13,"));
    }

    #[test]
    fn a_merge_block_stays_under_the_parameter_ceiling() {
        let rows = rows_per_statement(INDICATOR_PARAMS_PER_ROW);
        assert_eq!(rows, 166);
        assert_eq!(merge_indicators_sql(rows).matches("@P").count(), rows * 12);
    }

    #[test]
    fn indicator_parameters_follow_the_column_order_and_defaults() {
        let params = indicator_params(&write(Some("au"))).unwrap();
        assert_eq!(params.len(), INDICATOR_PARAMS_PER_ROW);
        assert_eq!(params[0], SqlParam::text("AAPL"));
        assert_eq!(params[1], SqlParam::text("AU"));
        assert_eq!(params[2], SqlParam::optional_float(Some(1.5)));
        assert_eq!(params[3], SqlParam::optional_float(None));
        assert_eq!(params[4], SqlParam::optional_float(Some(2.5)));
        assert_eq!(params[5], SqlParam::optional_float(None));
        assert_eq!(params[6], SqlParam::optional_float(None));
        assert_eq!(params[7], SqlParam::optional_float(Some(100.0)));
        assert_eq!(params[8], SqlParam::optional_integer32(Some(21)));
        assert_eq!(params[9], SqlParam::optional_date(None));
        assert_eq!(params[10], SqlParam::text(DEFAULT_METHOD));
        assert_eq!(params[11], SqlParam::text(DEFAULT_SMOOTHING));
    }

    #[test]
    fn an_indicator_row_without_an_exchange_is_refused_before_connecting() {
        assert!(matches!(
            indicator_params(&write(None)).unwrap_err(),
            StoreError::Invalid(_)
        ));
    }
}
