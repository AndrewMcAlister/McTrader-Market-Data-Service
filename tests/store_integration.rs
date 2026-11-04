//! Integration coverage against a real SQL Server.
//!
//! Every test is skipped unless `MDS_TEST_DATABASE` names a database. `MDS_TEST_SERVER`
//! defaults to `localhost`, and Windows integrated authentication is used with the server
//! certificate trusted, which is how this estate runs.
//!
//! What these tests prove that unit tests cannot:
//!
//! * tiberius' integrated authentication works at all;
//! * `decimal(18,6)` survives the round trip through [`rust_decimal::Decimal`];
//! * a `#stage` table created by a plain batch is visible to later parameterised batches on
//!   the same pooled connection — the assumption the daily upsert is built on;
//! * SQL Server error 1222 is classified as the retryable [`StoreError::Busy`];
//! * every read-only repository query parses the real columns it selects.
//!
//! The tests write only to session-scoped or global temp tables. They never touch
//! `SymbolPriceCache`, `DailySnapshot` or `IndicatorCache`.
//!
//! `MDS_TEST_SCHEMA=1` additionally runs the embedded schema script. Point that at a
//! **disposable** database only: the script documents the estate tables and, on an old
//! database, clears `IndicatorCache` as part of its venue-key migration.
//!
//! `MDS_TEST_CONCURRENCY=1` additionally runs the lock-wait test, which needs **two
//! simultaneous TDS sessions**. TDS only reaches SQL Server over TCP, so the server's TCP/IP
//! protocol must be enabled. A server with TCP disabled still accepts one connection on the
//! Dedicated Administrator Connection (DAC) port 1434, which is enough for every other test
//! here but cannot hold two sessions — that is the state of the development host this port
//! was written on, where ODBC reached the engine through shared memory and only the DAC
//! listened on TCP.

use mmds::store::config::DatabaseConfig;
use mmds::store::error::SqlConnection;
use mmds::store::query::{decimal, execute, query_rows, SqlParam};
use mmds::store::{
    DailyBarRepository, ExchangeRepository, IndicatorRepository, OpsRepository, PriceRepository,
    SqlServerStore, UsageRepository,
};
use tiberius::{Client, SqlBrowser};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;

/// The configured test database, or `None` when the suite should skip.
fn database() -> Option<String> {
    std::env::var("MDS_TEST_DATABASE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn config_for(database: &str) -> DatabaseConfig {
    DatabaseConfig {
        server: std::env::var("MDS_TEST_SERVER").unwrap_or_else(|_| "localhost".into()),
        database: database.to_string(),
        trusted: true,
        trust_server_certificate: true,
        user: String::new(),
        password: String::new(),
        timeout_seconds: 30,
    }
}

async fn raw_client(database: &str) -> SqlConnection {
    let config = config_for(database)
        .tiberius_config()
        .expect("the test configuration is valid");
    let tcp = TcpStream::connect_named(&config)
        .await
        .expect("the test SQL Server is reachable");
    tcp.set_nodelay(true).expect("TCP_NODELAY");
    Client::connect(config, tcp.compat_write())
        .await
        .expect("integrated authentication succeeds")
}

async fn store(database: &str) -> SqlServerStore {
    SqlServerStore::connect(&config_for(database))
        .await
        .expect("the pool builds")
}

#[tokio::test]
async fn decimals_and_a_staged_merge_round_trip_through_tds() {
    let Some(database) = database() else {
        return;
    };
    let mut client = raw_client(&database).await;

    let rows = query_rows(
        &mut client,
        "SELECT CAST(@P1 AS decimal(18,6))",
        &[SqlParam::float(123.456_789)],
    )
    .await
    .expect("a decimal cast is readable");
    let value = decimal(&rows[0], 0)
        .expect("the decimal column parses")
        .expect("the decimal is not NULL");
    assert!((value - 123.456_789).abs() < 1e-9, "read {value}");

    client
        .simple_query(
            "IF OBJECT_ID('tempdb..#mmds_stage') IS NOT NULL DROP TABLE #mmds_stage; \
             CREATE TABLE #mmds_stage (SymbolCode varchar(24) NOT NULL, Price decimal(18,6) NOT NULL); \
             IF OBJECT_ID('tempdb..#mmds_target') IS NOT NULL DROP TABLE #mmds_target; \
             CREATE TABLE #mmds_target (SymbolCode varchar(24) NOT NULL, Price decimal(18,6) NOT NULL);",
        )
        .await
        .expect("the temp tables are created")
        .into_results()
        .await
        .expect("the creation batch completes");

    execute(
        &mut client,
        "INSERT INTO #mmds_stage (SymbolCode, Price) VALUES (@P1,@P2),(@P3,@P4);",
        &[
            SqlParam::text("AAPL"),
            SqlParam::float(100.25),
            SqlParam::text("MSFT"),
            SqlParam::float(200.5),
        ],
    )
    .await
    .expect("the parameterised stage insert sees the plain-batch temp table");

    client
        .simple_query(
            "MERGE #mmds_target AS target \
             USING #mmds_stage AS source \
                 ON target.SymbolCode = source.SymbolCode \
             WHEN MATCHED THEN UPDATE SET Price = source.Price \
             WHEN NOT MATCHED THEN INSERT (SymbolCode, Price) \
                 VALUES (source.SymbolCode, source.Price);",
        )
        .await
        .expect("the merge runs")
        .into_results()
        .await
        .expect("the merge completes");

    let rows = query_rows(&mut client, "SELECT COUNT(*) FROM #mmds_target", &[])
        .await
        .expect("the staged rows are readable");
    let count: Option<i32> = rows[0].try_get(0).unwrap();
    assert_eq!(count, Some(2));
}

#[tokio::test]
async fn every_read_only_repository_answers_against_a_real_server() {
    let Some(database) = database() else {
        return;
    };
    let store = store(&database).await;

    store.ping().await.expect("ping");
    let exchanges = store.exchange_rows().await.expect("Exchanges reads");
    assert!(
        !exchanges.is_empty(),
        "MDS_TEST_DATABASE must contain dbo.Exchanges; got none"
    );
    let stats = store.stats().await.expect("stats read");
    // The counts are data, not an invariant; reaching here proves the row readers parsed.
    let _ = (
        stats.price_rows,
        stats.daily_rows,
        stats.daily_symbols,
        stats.indicator_rows,
        stats.latest_session,
        stats.latest_price_at,
        stats.daily_by_exchange,
    );
    store.request_types().await.expect("RequestTypes reads");

    store.latest_session().await.expect("latest session reads");
    store
        .session_symbol_counts(5)
        .await
        .expect("session counts read");
    store
        .get_prices(&["AAPL".into()], "US")
        .await
        .expect("prices read");
    store
        .get_daily_bars_many(&["AAPL".into()], 20, "US")
        .await
        .expect("bars read");
    store
        .daily_bar_depth(&["AAPL".into()], "US", None)
        .await
        .expect("bar depth reads");
    store
        .get_indicators(&["AAPL".into()], "US")
        .await
        .expect("indicators read");
    store
        .bulk_pull_count("/eod-bulk-last-day/US", None)
        .await
        .expect("the ledger freshness query reads");
    store
        .usage_summary(None)
        .await
        .expect("the ledger summary reads");
}

#[tokio::test]
async fn a_real_lock_wait_is_classified_as_the_retryable_busy_error() {
    let Some(database) = database() else {
        return;
    };
    if std::env::var("MDS_TEST_CONCURRENCY").ok().as_deref() != Some("1") {
        return;
    }
    let mut holder = raw_client(&database).await;
    let mut waiter = raw_client(&database).await;

    holder
        .simple_query(
            "IF OBJECT_ID('tempdb..##mmds_lock_probe') IS NOT NULL DROP TABLE ##mmds_lock_probe; \
             CREATE TABLE ##mmds_lock_probe (id int NOT NULL, v int NOT NULL); \
             INSERT INTO ##mmds_lock_probe VALUES (1, 1);",
        )
        .await
        .expect("the probe table is created")
        .into_results()
        .await
        .expect("the creation batch completes");

    holder
        .begin_transaction()
        .await
        .expect("the holder transaction opens");
    execute(
        &mut holder,
        "UPDATE ##mmds_lock_probe SET v = v + 1 WHERE id = 1",
        &[],
    )
    .await
    .expect("the holder owns the row lock");

    waiter
        .simple_query("SET LOCK_TIMEOUT 250")
        .await
        .expect("the wait is shortened")
        .into_results()
        .await
        .expect("the setting applies");

    let bound: Vec<&dyn tiberius::ToSql> = Vec::new();
    let outcome: Result<(), tiberius::error::Error> = match waiter
        .query("SELECT v FROM ##mmds_lock_probe WHERE id = 1", &bound)
        .await
    {
        Ok(stream) => stream.into_first_result().await.map(|_| ()),
        Err(error) => Err(error),
    };
    let error = outcome.expect_err("the read must not wait behind the holder");
    let error = mmds::store::StoreError::from(error);
    assert!(
        error.is_busy(),
        "expected SQL Server 1222 to become Busy, got {error:?}"
    );

    holder.rollback_transaction().await.expect("rollback");
    holder
        .simple_query("DROP TABLE ##mmds_lock_probe")
        .await
        .expect("the probe is dropped")
        .into_results()
        .await
        .expect("the drop completes");
}

#[tokio::test]
async fn the_embedded_schema_applies_to_a_disposable_database() {
    let Some(database) = database() else {
        return;
    };
    if std::env::var("MDS_TEST_SCHEMA").ok().as_deref() != Some("1") {
        return;
    }
    let store = store(&database).await;
    let batches = store.ensure_schema().await.expect("the schema applies");
    assert!(batches > 1);
}
