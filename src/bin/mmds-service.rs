//! The Windows service host: one process, one cache, one schedule.
//!
//! # Why this is a separate binary
//!
//! `cargo run` is for a developer at a console. A Windows service has no console, so anything
//! written only to stderr goes nowhere and a broken service looks identical to a quiet one.
//! This entry point therefore installs the rotating file log *before* it resolves configuration,
//! and exits non-zero on any fatal start so NSSM restarts it. That exit code is the difference
//! between a service that recovers and one that silently stays down.
//!
//! # Startup order
//!
//! 1. Log file, so a failure in every later step is recorded.
//! 2. Settings, resolving the vendor key last. Only the *source* is logged.
//! 3. Store pool, then `ensure_schema()`. A schema failure is logged, not fatal: the source
//!    service behaved the same way, and `/healthz` still tells an operator the database is
//!    unreachable.
//! 4. The market clock, from `dbo.Exchanges` when it answers and `data/clock.json` otherwise.
//! 5. The product layer and the collection schedule.
//! 6. Bind the port, log "ready", and serve until Ctrl+C or `/admin/shutdown`.
//!
//! The schedule starts only after the port is bound, so a tick cannot spend vendor units on a
//! process that will immediately die because its port was already taken.
//!
//! # Shutdown
//!
//! NSSM's console stop method sends Ctrl+C. Both that and `/admin/shutdown` resolve to the
//! same watch channel: axum finishes in-flight requests, then the scheduler is stopped and the
//! process exits 0. There is no second service instance and no shared cache — the collection
//! schedule, price snapshot and refresh gate are process-local by design.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::watch;

use mmds::api::{build_router, AppState, AppStore, Shared, ShutdownHook};
use mmds::clock::{MarketClock, SystemClock};
use mmds::config::{KeySource, Settings};
use mmds::logging::{self, LOG_FILE_NAME};
use mmds::secrets::EodhdKeyStore;
use mmds::store::{DatabaseConfig, ExchangeRepository, SqlServerStore};

/// Brokers, asset classes and the fallback exchange table, shipped beside the binary.
const CLOCK_REFERENCE: &str = "data/clock.json";

/// Relative to the service directory, which NSSM sets as the working directory.
const LOG_DIRECTORY: &str = "logs";

/// The source service's tick. A test pins it lower; production does not.
const SCHEDULE_TICK_SECONDS: u64 = 20;

/// Any failure here is fatal: the process manager must see a non-zero exit and restart us.
type Fatal = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> ExitCode {
    let log_path = match logging::init(Path::new(LOG_DIRECTORY)) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("fatal: could not open {LOG_DIRECTORY}/{LOG_FILE_NAME}: {error}");
            return ExitCode::FAILURE;
        }
    };
    match run(log_path).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(
                "fatal error; exiting non-zero so the service manager restarts us: {error}"
            );
            ExitCode::FAILURE
        }
    }
}

async fn run(log_path: PathBuf) -> Result<(), Fatal> {
    let keys = Arc::new(EodhdKeyStore::windows());
    let settings = Settings::from_env(keys.as_ref());
    log_startup(&settings, &log_path);

    let store = Arc::new(SqlServerStore::connect(&DatabaseConfig::from_settings(&settings)).await?);
    if let Err(error) = store.ensure_schema().await {
        tracing::error!("schema ensure failed: {error}");
    }
    log_missing_key(&settings);

    let market = Arc::new(load_clock(store.as_ref()).await?);
    let (stop_sender, stop_receiver) = watch::channel(false);
    let state = build_state(&settings, market, store, keys, stop_sender);
    state.rebuild_products().await;

    let listener = bind(&settings).await?;
    warn_if_unscheduled(state.install_schedule(SCHEDULE_TICK_SECONDS));

    tracing::info!(
        "McTrader_Market_Data_Service {} ready: db={} port={}",
        env!("CARGO_PKG_VERSION"),
        settings.db_database,
        settings.port
    );
    serve(state.clone(), listener, stop_receiver).await?;

    if let Some(scheduler) = state.scheduler() {
        scheduler.stop();
    }
    tracing::info!("stopped cleanly");
    Ok(())
}

/// The three facts an operator needs before any request is answered.
fn log_startup(settings: &Settings, log_path: &Path) {
    tracing::info!(
        "starting {} on {}:{} (database={}, pid={})",
        settings.service_name,
        settings.host,
        settings.port,
        settings.db_database,
        std::process::id()
    );
    tracing::info!("log file: {}", log_path.display());
    match settings.eodhd_key_source {
        KeySource::None => tracing::error!(
            "EODHD key source: none -- no key found: set EODHD_API_KEY in the service \
             environment (install-service.ps1 -EodhdApiKey). The keyring is per user and a \
             LocalSystem service cannot see it."
        ),
        source => tracing::info!("EODHD key source: {source}"),
    }
}

/// A missing key is not fatal: `/healthz` still answers, and the data endpoints refuse with a
/// named error rather than a vendor-client 500. The source service logged the same line.
fn log_missing_key(settings: &Settings) {
    if settings.eodhd_api_key.is_empty() {
        tracing::error!(
            "no EODHD API key resolved; price/bars/atr will refuse. \
             POST /secrets/eodhd to store one in this account's keyring."
        );
    }
}

/// Exchange rows when the database answers, otherwise the frozen reference file.
async fn load_clock(store: &SqlServerStore) -> Result<MarketClock, Fatal> {
    let rows = match store.exchange_rows().await {
        Ok(rows) => Some(rows),
        Err(error) => {
            tracing::warn!(
                "clock: could not read dbo.Exchanges, falling back to {CLOCK_REFERENCE} for \
                 exchanges too ({error})"
            );
            None
        }
    };
    let clock = MarketClock::load(Path::new(CLOCK_REFERENCE), rows)?;
    tracing::info!("clock source: {}", clock.source());
    Ok(clock)
}

/// One shared state, with the stop hook that `/admin/shutdown` and Ctrl+C both drive.
fn build_state(
    settings: &Settings,
    market: Arc<MarketClock>,
    store: Arc<SqlServerStore>,
    keys: Arc<EodhdKeyStore>,
    stop: watch::Sender<bool>,
) -> AppState {
    let hook: ShutdownHook = Arc::new(move || {
        let _ = stop.send(true);
    });
    let store: Arc<dyn AppStore> = store;
    Arc::new(
        Shared::new(settings.clone(), market, Arc::new(SystemClock))
            .with_store(store)
            .with_key_store(keys)
            .with_shutdown(hook),
    )
}

/// Bind the configured address. The port is a number in the environment, so it is validated
/// here rather than truncated into a u16.
async fn bind(settings: &Settings) -> Result<TcpListener, Fatal> {
    let port = u16::try_from(settings.port)
        .map_err(|_| format!("MDS_PORT is not a valid port: {}", settings.port))?;
    Ok(TcpListener::bind((settings.host.as_str(), port)).await?)
}

fn warn_if_unscheduled(installed: bool) {
    if !installed {
        tracing::warn!(
            "collection schedule not installed: the product layer or store is unavailable"
        );
    }
}

async fn serve(
    state: AppState,
    listener: TcpListener,
    stop: watch::Receiver<bool>,
) -> io::Result<()> {
    axum::serve(
        listener,
        build_router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(stopping(stop))
    .await
}

/// Resolve when the process is asked to stop, whether by the admin endpoint or the console.
async fn stopping(mut requested: watch::Receiver<bool>) {
    if *requested.borrow() {
        return;
    }
    tokio::select! {
        _ = requested.changed() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}
