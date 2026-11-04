//! The daily job as a command: one bulk pull, the free backfills, then ATR.
//!
//! # What each run costs
//!
//! `/eod-bulk-last-day` is **100 units flat** and returns the whole market. The two backfills
//! cost nothing: `--seed-from-barhourly` aggregates history the estate already paid for, and
//! `--fill-raw-close` reads the unadjusted 5-minute tape. `--rebuild-indicators` is local
//! arithmetic over `DailySnapshot`.
//!
//! # Why the order is fixed
//!
//! Seed, then fill, then pull, then rebuild. A seed or a fill is a local correction and never
//! a reason to spend 100 units on the day's bulk pull, so either one suppresses the pull — the
//! same rule the source job applied. Indicators run last because they read whatever the
//! earlier steps wrote.
//!
//! # What is not logged
//!
//! The vendor key never is. Only its source is, exactly as the service does, and the key is
//! omitted from every error the client renders.
//!
//! # Why this binary logs to stderr only
//!
//! `mmds-service` owns `logs/data_service.log`. A second process sharing that rotating file
//! would race its renames on Windows. Task Scheduler captures this run's output the same way
//! it captured the source job's.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use chrono::{Local, NaiveDate, Utc};
use clap::Parser;

use mmds::config::{KeySource, Settings};
use mmds::jobs::backfill::{
    distinct_dates, fill_raw_close, seed_from_barhourly, weekday_dates, RawCloseSummary,
    SeedSummary, SEED_CHUNK,
};
use mmds::jobs::daily::{pull_bulk, rebuild_indicators, PullSummary, RebuildSummary};
use mmds::logging;
use mmds::providers::eodhd_client::EodhdClient;
use mmds::refresh::RefreshFailure;
use mmds::secrets::EodhdKeyStore;
use mmds::store::usage::UsageRepository;
use mmds::store::{DailyBarRepository, DatabaseConfig, OpsRepository, SqlServerStore};
use mmds::usage::{StoreUsageLedger, Tariff};

/// Spend from this job is attributed to the job, not to an HTTP caller.
pub const CALLER: &str = "jobs.daily";

/// Any failure here is fatal: the scheduled task must see a non-zero exit.
type Fatal = Box<dyn std::error::Error + Send + Sync>;

/// The command-line contract, matching `python -m jobs.daily` flag for flag.
#[derive(Debug, Parser)]
#[command(name = "mmds-daily", about = "McTrader_Market_Data_Service daily job")]
struct Args {
    /// A specific session date (repeatable). Default: the latest.
    #[arg(long = "date", value_name = "YYYY-MM-DD", value_parser = parse_date)]
    dates: Vec<NaiveDate>,

    /// Pull every weekday from this date to today, to make the daily series contiguous.
    #[arg(long, value_name = "YYYY-MM-DD", value_parser = parse_date)]
    since: Option<NaiveDate>,

    /// The venue to pull and to stamp on written rows.
    #[arg(long, default_value = "US")]
    market: String,

    /// Free history seed from Back_Test.BarHourly (no vendor units).
    #[arg(long)]
    seed_from_barhourly: bool,

    /// How far back to seed (default 90 calendar days).
    #[arg(long, default_value_t = 90)]
    days: i64,

    /// Fill DailySnapshot.RawClose from the Bar5Min tape (no vendor units).
    #[arg(long)]
    fill_raw_close: bool,

    /// How far back the raw-close fill reaches (default 800).
    #[arg(long, default_value_t = 800)]
    raw_close_days: i64,

    /// Recompute IndicatorCache and exit (no vendor call).
    #[arg(long)]
    rebuild_indicators: bool,

    /// Do not rebuild IndicatorCache after loading.
    #[arg(long)]
    skip_indicators: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    logging::init_stderr();
    let args = Args::parse();
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("daily job failed: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), Fatal> {
    let keys = EodhdKeyStore::windows();
    let settings = Settings::from_env(&keys);
    log_startup(&settings);

    let store = Arc::new(SqlServerStore::connect(&DatabaseConfig::from_settings(&settings)).await?);
    store.ensure_schema().await?;
    tracing::info!("database={}", settings.db_database);

    let today = Local::now().date_naive();
    let dates = pull_dates(&args, today);

    let started = Instant::now();
    seed(&store, &args, today).await?;
    fill(&store, &args).await?;
    pull(&store, &settings, &args, &dates).await?;
    indicators(&store, &settings, &args).await?;
    report(&store, started).await?;
    Ok(())
}

/// The key's source, never the key. A missing key is not fatal: the free work still runs.
fn log_startup(settings: &Settings) {
    match settings.eodhd_key_source {
        KeySource::None => {
            tracing::warn!("EODHD key source: none -- the vendor pull will be skipped")
        }
        source => tracing::info!("EODHD key source: {source}"),
    }
}

/// The explicit dates first, then the weekday expansion, with duplicates removed so the same
/// session is never paid for twice in one run.
fn pull_dates(args: &Args, today: NaiveDate) -> Vec<NaiveDate> {
    let mut dates = args.dates.clone();
    if let Some(since) = args.since {
        let expanded = weekday_dates(since, today);
        tracing::info!(
            "--since {since} expands to {} weekday(s); {} date(s) to pull",
            expanded.len(),
            dates.len() + expanded.len()
        );
        dates.extend(expanded);
    }
    distinct_dates(dates)
}

async fn seed(store: &SqlServerStore, args: &Args, today: NaiveDate) -> Result<(), Fatal> {
    if !args.seed_from_barhourly {
        return Ok(());
    }
    let summary: SeedSummary =
        seed_from_barhourly(store, &args.market, args.days, today, SEED_CHUNK).await?;
    tracing::info!(
        "seed: {} row(s) since {}",
        summary.rows_written,
        summary.since
    );
    Ok(())
}

async fn fill(store: &SqlServerStore, args: &Args) -> Result<(), Fatal> {
    if !args.fill_raw_close {
        return Ok(());
    }
    let summary: RawCloseSummary =
        fill_raw_close(store, &args.market, args.raw_close_days, Utc::now()).await?;
    if let Some(skipped) = &summary.skipped {
        tracing::warn!("raw close: skipped ({skipped})");
    } else {
        tracing::info!(
            "raw close: {} row(s) across {} symbol(s)",
            summary.updated,
            summary.symbols
        );
    }
    Ok(())
}

/// The vendor pull. Suppressed by a fill or a rebuild, and skipped with a warning when no key
/// is configured — exactly the source job's rules.
async fn pull(
    store: &Arc<SqlServerStore>,
    settings: &Settings,
    args: &Args,
    dates: &[NaiveDate],
) -> Result<(), Fatal> {
    if args.rebuild_indicators || args.fill_raw_close {
        return Ok(());
    }
    if settings.eodhd_api_key.is_empty() {
        tracing::warn!("no EODHD key: skipping the vendor pull");
        return Ok(());
    }

    let client = EodhdClient::new(
        settings.eodhd_api_key.clone(),
        settings.eodhd_base_url.clone(),
        settings.eodhd_timeout_seconds,
        settings.eodhd_bulk_timeout_seconds,
    )?;
    attach_ledger(&client, store).await;

    let dates = (!dates.is_empty()).then(|| dates.to_vec());
    match pull_bulk(store.as_ref(), &client, &args.market, dates).await {
        Ok(summary) => log_pull(&summary),
        // A provider failure is reported and the run continues to the indicator rebuild,
        // which is what the source job did. A store failure is still fatal.
        Err(RefreshFailure::Provider(error)) => tracing::error!("bulk pull failed: {error}"),
        Err(other) => return Err(other.into()),
    }

    let stats = client.stats();
    let calls: u64 = stats.calls_by_purpose.values().sum();
    tracing::info!(
        "vendor spend this run: {} units over {calls} call(s)",
        stats.units
    );
    Ok(())
}

/// Price the run from the same tariff the API uses, so the spend is visible in
/// `dbo.EODHDUsage`. An unreadable tariff leaves the client's literal unit counts as the
/// meter rather than failing the pull.
async fn attach_ledger(client: &EodhdClient, store: &Arc<SqlServerStore>) {
    match store.request_types().await {
        Ok(rows) => {
            let concrete = Arc::clone(store);
            let repository: Arc<dyn UsageRepository> = concrete;
            let ledger = Arc::new(StoreUsageLedger::new(repository, Tariff::from_rows(rows)));
            client.set_recorder(ledger.recorder_for(Some(CALLER)));
        }
        Err(error) => tracing::warn!("no spend ledger for this run ({error})"),
    }
}

fn log_pull(summary: &PullSummary) {
    for session in &summary.sessions {
        tracing::info!(
            "session {}: {} received, {} written, {} without adjusted_close",
            session.session,
            session.received,
            session.written,
            session.rows_without_adjusted_close
        );
    }
    tracing::info!("pull: {} row(s) written", summary.rows_written);
}

async fn indicators(store: &SqlServerStore, settings: &Settings, args: &Args) -> Result<(), Fatal> {
    if args.skip_indicators {
        return Ok(());
    }
    let summary: RebuildSummary = rebuild_indicators(store, settings).await?;
    tracing::info!(
        "indicators: {} symbol(s) considered, {} row(s) written",
        summary.symbols_considered,
        summary.indicators_written
    );
    Ok(())
}

async fn report(store: &SqlServerStore, started: Instant) -> Result<(), Fatal> {
    let stats = store.stats().await?;
    tracing::info!(
        "done in {:.1}s: {} price row(s), {} daily row(s) across {} series, \
         {} indicator row(s), latest session {}",
        started.elapsed().as_secs_f64(),
        stats.price_rows,
        stats.daily_rows,
        stats.daily_symbols,
        stats.indicator_rows,
        stats
            .latest_session
            .map_or_else(|| "none".to_string(), |date| date.to_string())
    );
    for entry in store.session_symbol_counts(5).await? {
        tracing::info!(
            "  session {} {}: {} symbols",
            entry.session,
            entry.exchange,
            entry.symbols
        );
    }
    Ok(())
}

/// A `YYYY-MM-DD` date, refused with the format rather than silently ignored.
fn parse_date(text: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .map_err(|_| format!("{text:?} is not a YYYY-MM-DD date"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(dates: &[NaiveDate], since: Option<NaiveDate>) -> Args {
        Args {
            dates: dates.to_vec(),
            since,
            market: "US".into(),
            seed_from_barhourly: false,
            days: 90,
            fill_raw_close: false,
            raw_close_days: 800,
            rebuild_indicators: false,
            skip_indicators: false,
        }
    }

    fn day(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    #[test]
    fn a_bad_date_is_refused_with_the_format() {
        assert!(parse_date("2026-11-04").is_ok());
        assert_eq!(
            parse_date("04/11/2026").unwrap_err(),
            "\"04/11/2026\" is not a YYYY-MM-DD date"
        );
    }

    #[test]
    fn the_explicit_dates_come_first_and_the_expansion_follows() {
        let dates = pull_dates(
            &args(&[day(2026, 11, 2)], Some(day(2026, 11, 2))),
            day(2026, 11, 4),
        );
        assert_eq!(
            dates,
            vec![day(2026, 11, 2), day(2026, 11, 3), day(2026, 11, 4)]
        );
    }

    #[test]
    fn no_dates_means_the_latest_session() {
        assert!(pull_dates(&args(&[], None), day(2026, 11, 4)).is_empty());
    }
}
