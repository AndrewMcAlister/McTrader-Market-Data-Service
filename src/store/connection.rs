//! The connection pool and the one place a TDS connection is made.
//!
//! Each new connection runs `SET LOCK_TIMEOUT 15000`, so a read blocked behind the daily
//! loader fails in fifteen seconds with SQL Server error 1222 rather than hanging like a
//! dead service. [`super::error::StoreError`] turns 1222 into the API's retryable 503.
//!
//! Authentication is Windows integrated by default. `tiberius` speaks TDS directly, so the
//! ODBC driver name the Python service required (`MDS_DB_DRIVER`) is accepted but unused —
//! see [`DatabaseConfig::tiberius_config`].

use std::time::Duration;

use bb8::{ManageConnection, Pool, PooledConnection};
use tiberius::{Client, Config, SqlBrowser};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;

use super::config::{DatabaseConfig, POOL_MAX_SIZE};
use super::error::{SqlConnection, StoreError};

/// How long a read waits for a writer before SQL Server raises 1222.
pub const LOCK_TIMEOUT_MS: u64 = 15_000;

/// The per-connection session setting, built from [`LOCK_TIMEOUT_MS`] so there is one number.
pub fn lock_timeout_sql() -> String {
    format!("SET LOCK_TIMEOUT {LOCK_TIMEOUT_MS}")
}

/// Creates tiberius clients for the pool.
pub struct TiberiusManager {
    config: Config,
}

impl TiberiusManager {
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

impl ManageConnection for TiberiusManager {
    type Connection = SqlConnection;
    type Error = tiberius::error::Error;

    async fn connect(&self) -> Result<Self::Connection, Self::Error> {
        let tcp = TcpStream::connect_named(&self.config).await?;
        tcp.set_nodelay(true)?;
        let mut client = Client::connect(self.config.clone(), tcp.compat_write()).await?;
        client
            .simple_query(lock_timeout_sql())
            .await?
            .into_results()
            .await?;
        Ok(client)
    }

    async fn is_valid(&self, connection: &mut Self::Connection) -> Result<(), Self::Error> {
        connection
            .simple_query("SELECT 1")
            .await?
            .into_results()
            .await?;
        Ok(())
    }

    fn has_broken(&self, _connection: &mut Self::Connection) -> bool {
        // There is no cheap broken flag on a tiberius client; `is_valid` runs SELECT 1 on
        // checkout, which is the real proof the connection still speaks TDS.
        false
    }
}

/// The SQL Server implementation of every repository.
pub struct SqlServerStore {
    pool: Pool<TiberiusManager>,
}

impl SqlServerStore {
    /// Build the pool. No connection is opened until the first operation, matching the
    /// source service, whose `Store` construction could not fail either.
    pub async fn connect(config: &DatabaseConfig) -> Result<Self, StoreError> {
        let tiberius = config.tiberius_config()?;
        let manager = TiberiusManager::new(tiberius);
        let pool = Pool::builder()
            .max_size(POOL_MAX_SIZE)
            .connection_timeout(config.pool_timeout())
            .build(manager)
            .await?;
        Ok(Self { pool })
    }

    /// Build from an already-parsed tiberius configuration. Integration tests use this to
    /// point at a disposable database without rebuilding `Settings`.
    pub async fn with_tiberius_config(
        tiberius: Config,
        checkout_timeout: Duration,
    ) -> Result<Self, StoreError> {
        let manager = TiberiusManager::new(tiberius);
        let pool = Pool::builder()
            .max_size(POOL_MAX_SIZE)
            .connection_timeout(checkout_timeout)
            .build(manager)
            .await?;
        Ok(Self { pool })
    }

    /// One pooled connection, borrowed for the length of one operation.
    pub(crate) async fn connection(
        &self,
    ) -> Result<PooledConnection<'_, TiberiusManager>, StoreError> {
        Ok(self.pool.get().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_timeout_is_the_documented_fifteen_seconds() {
        assert_eq!(LOCK_TIMEOUT_MS, 15_000);
        assert_eq!(lock_timeout_sql(), "SET LOCK_TIMEOUT 15000");
    }
}
