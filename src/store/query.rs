//! Parameter binding and row reading, in one place.
//!
//! TDS has no `executemany`, so every write here is a set-based statement sized to stay
//! under SQL Server's 2,100-parameter ceiling. [`SqlParam`] owns the values so a caller can
//! build that statement's parameter list without fighting lifetimes, and [`query_rows`] /
//! [`execute`] are the only two ways the repositories talk to a connection.
//!
//! `decimal(18,6)` arrives as [`rust_decimal::Decimal`] and is converted to `f64` here, at the
//! boundary, matching the source service: every consumer downstream (ATR, JSON) works in
//! floats, and mixing the two is how a rounding rule gets invented by accident.

use chrono::{NaiveDate, NaiveDateTime};
use rust_decimal::prelude::ToPrimitive;
use tiberius::{Row, ToSql};

use super::error::{SqlConnection, StoreError};

/// SQL Server allows 2,100 parameters per statement. Staying below it leaves room for a
/// future column and makes the arithmetic obviously safe.
pub const MAX_PARAMS_PER_STATEMENT: usize = 2_000;

/// One bound value, typed so the server sees the column's real type.
///
/// Every variant is nullable: `None` is sent as SQL `NULL` with the correct type rather than
/// as an untyped null, which SQL Server may refuse to convert in a `MERGE`.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlParam {
    Text(Option<String>),
    Float(Option<f64>),
    Integer(Option<i64>),
    Integer32(Option<i32>),
    Bit(Option<bool>),
    Date(Option<NaiveDate>),
    Timestamp(Option<NaiveDateTime>),
}

impl SqlParam {
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(Some(value.into()))
    }

    /// Text for a binding, falling back when the value is absent or empty.
    ///
    /// This mirrors Python's `value or default`, where an empty string is falsy but a
    /// whitespace-only one is not: a provider that sent `" "` meant `" "`, however odd that is.
    pub fn text_or(value: Option<&str>, fallback: &str) -> Self {
        match value {
            Some(text) if !text.is_empty() => Self::Text(Some(text.to_string())),
            _ => Self::Text(Some(fallback.to_string())),
        }
    }

    pub fn optional_text(value: Option<String>) -> Self {
        Self::Text(value)
    }

    pub fn float(value: f64) -> Self {
        Self::Float(Some(value))
    }

    pub fn optional_float(value: Option<f64>) -> Self {
        Self::Float(value)
    }

    pub fn integer(value: i64) -> Self {
        Self::Integer(Some(value))
    }

    pub fn optional_integer(value: Option<i64>) -> Self {
        Self::Integer(value)
    }

    pub fn integer32(value: i32) -> Self {
        Self::Integer32(Some(value))
    }

    pub fn optional_integer32(value: Option<i32>) -> Self {
        Self::Integer32(value)
    }

    pub fn bit(value: bool) -> Self {
        Self::Bit(Some(value))
    }

    pub fn date(value: NaiveDate) -> Self {
        Self::Date(Some(value))
    }

    pub fn optional_date(value: Option<NaiveDate>) -> Self {
        Self::Date(value)
    }

    pub fn timestamp(value: NaiveDateTime) -> Self {
        Self::Timestamp(Some(value))
    }

    pub fn optional_timestamp(value: Option<NaiveDateTime>) -> Self {
        Self::Timestamp(value)
    }

    fn as_dyn(&self) -> &dyn ToSql {
        match self {
            Self::Text(value) => value,
            Self::Float(value) => value,
            Self::Integer(value) => value,
            Self::Integer32(value) => value,
            Self::Bit(value) => value,
            Self::Date(value) => value,
            Self::Timestamp(value) => value,
        }
    }
}

/// Borrow every parameter in the order the statement's `@P1..@Pn` expect.
pub(crate) fn bind(params: &[SqlParam]) -> Vec<&dyn ToSql> {
    params.iter().map(SqlParam::as_dyn).collect()
}

/// Run a `SELECT` and materialise every row of its first result set.
pub async fn query_rows(
    client: &mut SqlConnection,
    sql: &str,
    params: &[SqlParam],
) -> Result<Vec<Row>, StoreError> {
    let bound = bind(params);
    let stream = client.query(sql, &bound).await?;
    Ok(stream.into_first_result().await?)
}

/// Run a statement that returns no rows and report how many rows it touched.
pub async fn execute(
    client: &mut SqlConnection,
    sql: &str,
    params: &[SqlParam],
) -> Result<u64, StoreError> {
    let bound = bind(params);
    Ok(client.execute(sql, &bound).await?.total())
}

/// A decimal column as a float, or `None` when the column is `NULL`.
///
/// `decimal(18,6)` cannot be read as `f64` directly by tiberius, so the conversion is
/// explicit and happens exactly once.
pub fn decimal(row: &Row, index: usize) -> Result<Option<f64>, StoreError> {
    let value: Option<rust_decimal::Decimal> = row
        .try_get(index)
        .map_err(|error| StoreError::database(format!("column {index}: {error}")))?;
    Ok(value.and_then(|value| value.to_f64()))
}

/// A text column, owned so the row can be dropped.
pub fn text(row: &Row, index: usize) -> Result<Option<String>, StoreError> {
    let value: Option<&str> = row
        .try_get(index)
        .map_err(|error| StoreError::database(format!("column {index}: {error}")))?;
    Ok(value.map(str::to_string))
}

/// A `float` column, as the warehouse aggregate casts one.
///
/// [`decimal`] converts a `decimal(18,6)`; this reads a value the server has already cast to
/// `float` in the `SELECT`. Keeping the two readers apart means neither has to guess which
/// type the driver will hand back.
pub fn float(row: &Row, index: usize) -> Result<Option<f64>, StoreError> {
    row.try_get(index)
        .map_err(|error| StoreError::database(format!("column {index}: {error}")))
}

/// A `date` column.
pub fn date(row: &Row, index: usize) -> Result<Option<NaiveDate>, StoreError> {
    row.try_get(index)
        .map_err(|error| StoreError::database(format!("column {index}: {error}")))
}

/// A `datetime2` column. The value is naive UTC, as the source service's columns are.
pub fn timestamp(row: &Row, index: usize) -> Result<Option<NaiveDateTime>, StoreError> {
    row.try_get(index)
        .map_err(|error| StoreError::database(format!("column {index}: {error}")))
}

/// A `bigint`, `int` or `tinyint` column, widened to `i64`.
///
/// TDS distinguishes the widths, and tiberius will not hand a narrower integer to a wider
/// request, so each is tried in turn. `COUNT(*)`, `SUM(int)` and every `int` column arrive as
/// `I32`; `Volume` arrives as `I64`; and `dbo.Exchanges.Id` is a `tinyint`, which arrives as
/// `U8`.
pub fn integer(row: &Row, index: usize) -> Result<Option<i64>, StoreError> {
    if let Ok(value) = row.try_get::<i64, _>(index) {
        return Ok(value);
    }
    if let Ok(value) = row.try_get::<i32, _>(index) {
        return Ok(value.map(i64::from));
    }
    row.try_get::<u8, _>(index)
        .map(|value| value.map(i64::from))
        .map_err(|error| StoreError::database(format!("column {index}: {error}")))
}

/// An `int` column, narrowed from the driver's `i64` when needed.
pub fn integer32(row: &Row, index: usize) -> Result<Option<i32>, StoreError> {
    match integer(row, index)? {
        Some(value) => i32::try_from(value)
            .map(Some)
            .map_err(|_| StoreError::database(format!("column {index} does not fit an int"))),
        None => Ok(None),
    }
}

/// A `bit` column.
pub fn boolean(row: &Row, index: usize) -> Result<Option<bool>, StoreError> {
    row.try_get(index)
        .map_err(|error| StoreError::database(format!("column {index}: {error}")))
}

/// A non-null integer column, or a clear error naming the contradiction.
pub fn required_integer(row: &Row, index: usize, column: &str) -> Result<i64, StoreError> {
    integer(row, index)?.ok_or_else(|| StoreError::database(format!("{column} was NULL")))
}

/// A non-null text column, or a clear error naming the contradiction.
pub fn required_text(row: &Row, index: usize, column: &str) -> Result<String, StoreError> {
    text(row, index)?.ok_or_else(|| StoreError::database(format!("{column} was NULL")))
}

/// A non-null float column, or a clear error naming the contradiction.
pub fn required_float(row: &Row, index: usize, column: &str) -> Result<f64, StoreError> {
    decimal(row, index)?.ok_or_else(|| StoreError::database(format!("{column} was NULL")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiberius::ColumnData;

    #[test]
    fn every_variant_exposes_a_typed_to_sql() {
        assert!(matches!(
            SqlParam::text("AAPL").as_dyn().to_sql(),
            ColumnData::String(Some(_))
        ));
        assert!(matches!(
            SqlParam::float(1.5).as_dyn().to_sql(),
            ColumnData::F64(Some(_))
        ));
        assert!(matches!(
            SqlParam::integer(7).as_dyn().to_sql(),
            ColumnData::I64(Some(7))
        ));
        assert!(matches!(
            SqlParam::integer32(7).as_dyn().to_sql(),
            ColumnData::I32(Some(7))
        ));
        assert!(matches!(
            SqlParam::bit(true).as_dyn().to_sql(),
            ColumnData::Bit(Some(true))
        ));
        assert!(matches!(
            SqlParam::date(NaiveDate::from_ymd_opt(2026, 11, 4).unwrap())
                .as_dyn()
                .to_sql(),
            ColumnData::Date(Some(_))
        ));
        assert!(matches!(
            SqlParam::timestamp(
                NaiveDateTime::parse_from_str("2026-11-04 03:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
            )
            .as_dyn()
            .to_sql(),
            ColumnData::DateTime2(Some(_))
        ));
    }

    #[test]
    fn a_null_carries_its_column_type_rather_than_being_untyped() {
        assert!(matches!(
            SqlParam::optional_text(None).as_dyn().to_sql(),
            ColumnData::String(None)
        ));
        assert!(matches!(
            SqlParam::optional_float(None).as_dyn().to_sql(),
            ColumnData::F64(None)
        ));
        assert!(matches!(
            SqlParam::optional_timestamp(None).as_dyn().to_sql(),
            ColumnData::DateTime2(None)
        ));
    }
}
