//! Operator commands: the source `tools/*.py` scripts, as one subcommand tree.
//!
//! # Why one binary
//!
//! The Python project had ten scripts, each with its own `sys.path` bootstrap, its own
//! connection setup and its own idea of how to print a result. They are one command with
//! subcommands here: shared configuration, shared store construction, shared HTTP client, and
//! one place that decides what a failure looks like.
//!
//! # What is safe to run
//!
//! `apply-schema`, `bench-upsert`, `cleanup-sessions` and `copy-exchanges` talk to the
//! database directly. The remaining commands talk to a **running** service over HTTP and some
//! of them deliberately spend vendor units — `probe-bulk-route` makes one 100-unit call, and
//! `smoke-api` fetches quotes. `check-mcp` starts the MCP server over stdio and calls all five
//! of its tools, which reach the same running service. They are operator diagnostics, never
//! tests: the test suite neither reads a vendor key nor spends a unit.

mod http;
mod mcp;
mod operator;
mod probes;
mod smoke;

use std::process::ExitCode;

use chrono::NaiveDate;
use clap::{Parser, Subcommand};

use mmds::config::Settings;
use mmds::secrets::EodhdKeyStore;
use mmds::store::{DatabaseConfig, SqlServerStore};

/// Every failure the commands report. The process exits 1 with the message on stderr.
pub(crate) type ToolResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Where a running service is reached when the environment does not say otherwise.
pub(crate) const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8003";

#[derive(Debug, Parser)]
#[command(
    name = "mmds-tool",
    about = "Operator commands for McTrader_Market_Data_Service"
)]
struct Cli {
    /// The running service. Defaults to $MDS_BASE_URL or http://127.0.0.1:8003.
    #[arg(long, global = true, value_name = "URL")]
    base_url: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Apply sql/001_schema.sql and report what landed.
    ApplySchema,

    /// Time the daily upsert path without spending vendor units.
    BenchUpsert,

    /// Remove daily rows after a session and clear the indicator cache.
    CleanupSessions {
        /// The last session to keep. Default: 2026-10-14, the source tool's cutoff.
        #[arg(value_name = "YYYY-MM-DD", value_parser = parse_date)]
        cutoff: Option<NaiveDate>,
    },

    /// Copy the exchange reference into the service's database.
    CopyExchanges {
        /// The source database. Default: $MDS_EXCHANGE_SOURCE or McTrader_Live.
        #[arg(long, value_name = "DATABASE")]
        source: Option<String>,
    },

    /// Start the MCP server and list and call all five of its tools.
    CheckMcp,

    /// Prove the bulk routing: 150 symbols should cost 100 units, not 150.
    ProbeBulkRoute,

    /// Prove the service stays responsive while a heavier call is running.
    ProbeResponsiveness,

    /// Smoke-test the data endpoints against a running service.
    SmokeApi,

    /// Prove the provider routing against the live services.
    SmokeProviders,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let base_url = resolve_base_url(cli.base_url);
    match dispatch(cli.command, &base_url).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(command: Command, base_url: &str) -> ToolResult<()> {
    match command {
        Command::ApplySchema => operator::apply_schema().await,
        Command::BenchUpsert => operator::bench_upsert().await,
        Command::CleanupSessions { cutoff } => operator::cleanup_sessions(cutoff).await,
        Command::CopyExchanges { source } => operator::copy_exchanges(source).await,
        Command::CheckMcp => mcp::check_mcp(base_url).await,
        Command::ProbeBulkRoute => probes::probe_bulk_route(base_url).await,
        Command::ProbeResponsiveness => probes::probe_responsiveness(base_url).await,
        Command::SmokeApi => smoke::smoke_api(base_url).await,
        Command::SmokeProviders => smoke::smoke_providers(base_url).await,
    }
}

/// The deployment's settings, with the vendor key resolved the same way the service resolves
/// it. Only the source is ever printed; the key itself never is.
pub(crate) fn settings() -> Settings {
    Settings::from_env(&EodhdKeyStore::windows())
}

/// A store pool. No connection is opened here; the first statement does that.
pub(crate) async fn open_store(settings: &Settings) -> ToolResult<SqlServerStore> {
    Ok(SqlServerStore::connect(&DatabaseConfig::from_settings(settings)).await?)
}

fn resolve_base_url(argument: Option<String>) -> String {
    argument
        .or_else(|| std::env::var("MDS_BASE_URL").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn parse_date(text: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .map_err(|_| format!("{text:?} is not a YYYY-MM-DD date"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_base_url_wins_and_loses_its_trailing_slash() {
        assert_eq!(
            resolve_base_url(Some("http://host:9000/".into())),
            "http://host:9000"
        );
    }

    #[test]
    fn the_default_is_the_source_service_port() {
        assert_eq!(
            resolve_base_url(None),
            std::env::var("MDS_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string())
        );
    }

    #[test]
    fn a_bad_date_is_refused_with_the_format() {
        assert!(parse_date("2026-10-14").is_ok());
        assert!(parse_date("14/10/2026").is_err());
    }
}
