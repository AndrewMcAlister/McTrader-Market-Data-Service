//! The four commands that talk to the database rather than to the service.
//!
//! Their SQL lives in [`mmds::store::operator`]; this module is only the operator-facing
//! presentation: what to read, what to print, and what counts as a failure. Each command opens
//! its own pool, so none of them depends on a service being up.

use std::collections::BTreeMap;
use std::time::Instant;

use chrono::NaiveDate;

use mmds::config::Settings;
use mmds::store::models::StoreStats;
use mmds::store::{DailyBarRepository, OpsRepository, SqlServerStore};

use crate::{open_store, settings, ToolResult};

/// The cutoff the source `cleanup_sessions` tool used. Kept as the default so an operator who
/// runs the command bare reproduces the previous behaviour.
const DEFAULT_CUTOFF: (i32, u32, u32) = (2026, 10, 14);

/// The source `copy_exchanges` tool's database.
const DEFAULT_SOURCE: &str = "McTrader_Live";

/// Symbols `bench-upsert` reads back after writing.
const SAMPLE_SYMBOLS: [&str; 3] = ["AAPL", "MSFT", "NVDA"];

pub(crate) async fn apply_schema() -> ToolResult<()> {
    let settings = settings();
    println!("server   : {}", settings.db_server);
    println!("database : {}", settings.db_database);
    println!("key      : {}", key_state(&settings));
    println!();

    let store = open_store(&settings).await?;
    let applied = store.ensure_schema().await?;
    println!("applied {applied} schema batch(es)");
    println!();

    let stats = store.stats().await?;
    print_stats(&stats);
    println!();
    print_sessions(&store, 8).await?;
    println!();

    let docs = store.schema_documentation().await?;
    println!("documented tables : {}", docs.tables.len());
    for table in &docs.tables {
        println!("  {table}");
    }
    println!("documented columns: {}", docs.columns.len());
    Ok(())
}

/// Time the daily upsert path against one real session. Reads the session back first, so the
/// benchmark exercises the same merge the job uses without inventing rows.
pub(crate) async fn bench_upsert() -> ToolResult<()> {
    let settings = settings();
    let store = open_store(&settings).await?;
    let Some(session) = store.latest_session().await? else {
        return Err("DailySnapshot is empty; run mmds-daily before benchmarking".into());
    };
    let rows = store.session_bars(session).await?;
    println!("session {session}: {} rows read back", rows.len());
    if rows.is_empty() {
        return Err(format!("session {session} has no rows to write").into());
    }

    let ten_thousand = 10_000usize.min(rows.len());
    for (label, count) in [("10k rows", ten_thousand), ("full session", rows.len())] {
        let sample: Vec<_> = rows.iter().take(count).cloned().collect();
        let started = Instant::now();
        let written = store.upsert_daily(sample).await?;
        let elapsed = started.elapsed().as_secs_f64();
        let rate = written as f64 / elapsed.max(f64::EPSILON);
        println!("  {label:<14} {written:>7} rows  {elapsed:6.2}s  ({rate:.0} rows/s)");
    }

    let symbols: Vec<String> = SAMPLE_SYMBOLS
        .iter()
        .map(|symbol| symbol.to_string())
        .collect();
    let started = Instant::now();
    let bars = store.get_daily_bars_many(&symbols, 51, "US").await?;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    let counts: BTreeMap<&str, usize> = bars
        .iter()
        .map(|(symbol, bars)| (symbol.as_str(), bars.len()))
        .collect();
    println!("  read after write: {elapsed_ms:.0} ms, {counts:?}");

    let stats = store.stats().await?;
    println!("  total daily rows: {}", stats.daily_rows);
    Ok(())
}

/// Delete every session after the cutoff and clear the indicator cache, in one transaction.
pub(crate) async fn cleanup_sessions(cutoff: Option<NaiveDate>) -> ToolResult<()> {
    let cutoff = cutoff.unwrap_or_else(default_cutoff);
    let settings = settings();
    let store = open_store(&settings).await?;

    let counts = store.delete_sessions_after(cutoff).await?;
    println!("deleted daily rows after {cutoff}: {}", counts.daily_rows);
    println!(
        "cleared IndicatorCache rows      : {}",
        counts.indicator_rows
    );
    print_stats(&store.stats().await?);
    print_sessions(&store, 8).await?;
    Ok(())
}

/// Copy the exchange reference and print what the clock would now read.
pub(crate) async fn copy_exchanges(source: Option<String>) -> ToolResult<()> {
    let source = resolve_source(source);
    let settings = settings();
    let store = open_store(&settings).await?;

    let rows = store.copy_exchanges_from(&source).await?;
    println!(
        "{}.dbo.Exchanges <- {source}.dbo.Exchanges: {} row(s)",
        settings.db_database,
        rows.len()
    );
    for row in &rows {
        println!(
            "  {:>2} {:<8} {:<5} {}-{} {}",
            row.id,
            row.code.as_deref().unwrap_or(""),
            row.timezone.as_deref().unwrap_or(""),
            row.opens.as_deref().unwrap_or(""),
            row.closes.as_deref().unwrap_or(""),
            row.currency.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

/// `--source`, then `$MDS_EXCHANGE_SOURCE`, then the source tool's database.
fn resolve_source(argument: Option<String>) -> String {
    argument
        .or_else(|| std::env::var("MDS_EXCHANGE_SOURCE").ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| DEFAULT_SOURCE.to_string())
}

fn default_cutoff() -> NaiveDate {
    let (year, month, day) = DEFAULT_CUTOFF;
    NaiveDate::from_ymd_opt(year, month, day).expect("the compiled-in cutoff is a real date")
}

/// A missing key is reported as `ABSENT`, never as a value.
fn key_state(settings: &Settings) -> &'static str {
    if settings.eodhd_api_key.is_empty() {
        "ABSENT"
    } else {
        "present"
    }
}

fn print_stats(stats: &StoreStats) {
    println!(
        "stats: {} price row(s), {} daily row(s) across {} series, {} indicator row(s), \
         latest session {}",
        stats.price_rows,
        stats.daily_rows,
        stats.daily_symbols,
        stats.indicator_rows,
        stats
            .latest_session
            .map_or_else(|| "none".to_string(), |date| date.to_string())
    );
    for entry in &stats.daily_by_exchange {
        println!("  {}: {} bars", entry.exchange, entry.bars);
    }
}

async fn print_sessions(store: &SqlServerStore, limit: i64) -> ToolResult<()> {
    println!("sessions in DailySnapshot:");
    for entry in store.session_symbol_counts(limit).await? {
        println!(
            "  {} {}: {} symbols",
            entry.session, entry.exchange, entry.symbols
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_cutoff_is_the_source_tools() {
        assert_eq!(
            default_cutoff(),
            NaiveDate::from_ymd_opt(2026, 10, 14).unwrap()
        );
    }

    #[test]
    fn an_explicit_source_wins_over_the_environment() {
        assert_eq!(
            resolve_source(Some(" McTrader_Paper ".into())),
            "McTrader_Paper"
        );
    }

    #[test]
    fn a_blank_source_falls_back_to_the_source_database() {
        let resolved = resolve_source(Some("   ".into()));
        assert_eq!(
            resolved,
            std::env::var("MDS_EXCHANGE_SOURCE")
                .ok()
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| DEFAULT_SOURCE.to_string())
        );
    }
}
