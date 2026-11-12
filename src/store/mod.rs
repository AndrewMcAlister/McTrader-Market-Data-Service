//! The store: the owned tables, plus the estate tables the service reads.
//!
//! The owned tables are `SymbolPriceCache`, `DailySnapshot`, `IndicatorCache` (the three the
//! service was built on) and the two the EODHD egress routes added: `IntradayBar5Min` and
//! `BulkSnapshotPull`.
//!
//! Nothing here names a caller, an HTTP route, or a provider. A repository answers a question
//! about persisted rows and nothing else; the products layer decides when to ask, and the API
//! decides how to render the answer.
//!
//! # Shape
//!
//! The source service was one 900-line `Store` class. Here it is split into focused
//! repository traits — prices, daily bars, indicators, the usage ledger, exchanges, ops — and
//! the [`MarketDataStore`] supertrait that combines them. Products depend on
//! `Arc<dyn MarketDataStore>`, so a test can supply one in-memory fake instead of a database.
//!
//! # Conventions
//!
//! * A read that says nothing about its exchange means `US`. A write that says nothing is
//!   refused: see [`keys`].
//! * `decimal(18,6)` becomes `f64` at this boundary, once. Downstream code never sees
//!   [`rust_decimal::Decimal`], and never invents a second rounding rule.
//! * A lock timeout (SQL Server 1222) is [`error::StoreError::Busy`], which the API renders
//!   as a retryable 503 rather than a two-minute stall.
//! * TDS has no `executemany`; writes are set-based statements sized under SQL Server's
//!   2,100-parameter ceiling ([`query::MAX_PARAMS_PER_STATEMENT`]).

pub mod bars;
pub mod bulk;
pub mod config;
pub mod connection;
pub mod error;
pub mod indicators;
pub mod intraday;
pub mod keys;
pub mod models;
pub mod operator;
pub mod ops;
pub mod prices;
pub mod query;
pub mod schema;
pub mod sql;
pub mod usage;
pub mod warehouse;

pub use bars::DailyBarRepository;
pub use bulk::BulkPullRepository;
pub use config::DatabaseConfig;
pub use connection::SqlServerStore;
pub use error::{StoreError, STORE_BUSY_DETAIL};
pub use indicators::IndicatorRepository;
pub use intraday::IntradayRepository;
pub use ops::{ExchangeRepository, OpsRepository};
pub use prices::PriceRepository;
pub use usage::UsageRepository;
pub use warehouse::{FiveMinuteTape, WarehouseRepository};

/// Every repository the service needs, as one object.
///
/// This exists so orchestration code can depend on a single `Arc<dyn MarketDataStore>` while
/// each repository stays small enough to implement and test on its own. The blanket impl
/// means [`SqlServerStore`] satisfies it without a line of glue.
pub trait MarketDataStore:
    PriceRepository
    + DailyBarRepository
    + IndicatorRepository
    + UsageRepository
    + ExchangeRepository
    + OpsRepository
    + IntradayRepository
    + BulkPullRepository
{
}

impl<T> MarketDataStore for T where
    T: PriceRepository
        + DailyBarRepository
        + IndicatorRepository
        + UsageRepository
        + ExchangeRepository
        + OpsRepository
        + IntradayRepository
        + BulkPullRepository
{
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_store_object<T: MarketDataStore + ?Sized>() {}

    #[test]
    fn sql_server_store_satisfies_every_repository() {
        assert_store_object::<SqlServerStore>();
    }
}
