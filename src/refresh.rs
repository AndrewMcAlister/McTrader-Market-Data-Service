//! One bulk refresh of the daily bars, callable from the endpoint and from the daily job.
//!
//! # Why the policy is separate from the pull
//!
//! The pull already existed inside the daily job, reachable only by running that script. An
//! endpoint needs the same work with different decisions around it — whether it is already
//! fresh, whether another refresh is in flight, and what to report back — so the work and the
//! policy are separated here rather than copied. [`RefreshJobs`] is the work; this module is
//! the policy.
//!
//! # What a refresh costs
//!
//! Measured: 100 vendor units and about 10 seconds for a full-market session. That is one
//! flat-priced `/eod-bulk-last-day` call, which is why the freshness check runs before the
//! lock is taken and again before any vendor work: a second pull in the same local day spends
//! another 100 units for rows already stored.
//!
//! # Why a non-blocking lock
//!
//! Two callers asking at once would each spend 100 units and write the same rows. The second
//! is told the refresh is already running rather than queued behind it, because waiting
//! twenty seconds for someone else's pull is worse than being told to ask again.

use std::fmt;
use std::time::Instant;

use async_trait::async_trait;
use chrono::NaiveDate;
use serde::Serialize;
use serde_json::Value;

use crate::clock::Clock;
use crate::config::Settings;
use crate::domain::sessions::latest_completed_session;
use crate::error::ProviderFailure;
use crate::jobs::daily::{PullSummary, RebuildSummary};
use crate::presence::Presence;
use crate::providers::eodhd_client::EodhdClient;
use crate::store::{DailyBarRepository, IndicatorRepository, StoreError, UsageRepository};

/// The repositories a refresh reads and writes.
pub trait RefreshStore: DailyBarRepository + IndicatorRepository + UsageRepository {}

impl<T: ?Sized> RefreshStore for T where
    T: DailyBarRepository + IndicatorRepository + UsageRepository
{
}

/// The vendor capability a bulk refresh needs, and its meter.
///
/// A trait rather than the concrete client so the policy can be tested without a network or a
/// key, and so the unit count is read from the client that actually spent it rather than
/// assumed from the tariff.
#[async_trait]
pub trait RefreshVendor: Send + Sync {
    async fn bulk_last_day(
        &self,
        market: &str,
        session_date: Option<NaiveDate>,
    ) -> Result<Vec<Value>, ProviderFailure>;

    fn units_spent(&self) -> i64;
}

#[async_trait]
impl RefreshVendor for EodhdClient {
    async fn bulk_last_day(
        &self,
        market: &str,
        session_date: Option<NaiveDate>,
    ) -> Result<Vec<Value>, ProviderFailure> {
        EodhdClient::bulk_last_day(self, market, session_date).await
    }

    fn units_spent(&self) -> i64 {
        self.stats().units
    }
}

/// The pull and indicator rebuild a refresh performs.
///
/// The production implementation delegates to [`crate::jobs::daily`]; a test supplies an
/// implementation that records the call and returns a fixed summary, so the policy can be
/// pinned without a database.
#[async_trait]
pub trait RefreshJobs: Send + Sync {
    async fn pull(
        &self,
        store: &dyn RefreshStore,
        vendor: &dyn RefreshVendor,
        market: &str,
        dates: Option<Vec<NaiveDate>>,
    ) -> Result<PullSummary, RefreshFailure>;

    async fn rebuild_indicators(
        &self,
        store: &dyn RefreshStore,
        settings: &Settings,
    ) -> Result<RebuildSummary, RefreshFailure>;
}

/// What went wrong during a refresh.
#[derive(Debug, Clone, PartialEq)]
pub enum RefreshFailure {
    Store(StoreError),
    Provider(ProviderFailure),
    Invalid(String),
}

impl fmt::Display for RefreshFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "database error: {error}"),
            Self::Provider(error) => write!(formatter, "{error}"),
            Self::Invalid(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for RefreshFailure {}

impl From<StoreError> for RefreshFailure {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<ProviderFailure> for RefreshFailure {
    fn from(error: ProviderFailure) -> Self {
        Self::Provider(error)
    }
}

/// Which bulk request the ledger records for a market.
///
/// Per market, and that is a fix rather than a nicety: the type was hard-coded to `/US`, so
/// once the schedule started pulling the ASX at its own open + 45, an AU refresh consulted
/// the US row, read "fresh" because New York had pulled, and skipped the ASX session.
pub fn bulk_request_type(market: &str) -> String {
    format!("/eod-bulk-last-day/{}", market.to_uppercase())
}

/// True when a bulk pull for `market` is already recorded for the current local day.
///
/// The ledger is the right source and `MAX(SnapshotDate)` is not: single-symbol rows written
/// by `/bars` land in `DailySnapshot` with today's date, so the newest session can be today
/// while the market-wide pull has not run for weeks.
pub async fn is_fresh<S>(store: &S, market: &str) -> bool
where
    S: UsageRepository + ?Sized,
{
    match store
        .bulk_pull_count(&bulk_request_type(market), None)
        .await
    {
        Ok(count) => count > 0,
        // An unreadable ledger means "do not assume fresh": guessing fresh silently stops
        // refreshing, while guessing stale costs one pull.
        Err(error) => {
            tracing::warn!("could not read the ledger for freshness ({error}); treating as stale");
            false
        }
    }
}

/// One non-blocking refresh slot, process-wide.
#[derive(Debug, Default)]
pub struct RefreshGate {
    held: tokio::sync::Mutex<()>,
}

impl RefreshGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the slot without waiting, or answer `None` when someone else holds it.
    pub fn try_acquire(&self) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        self.held.try_lock().ok()
    }
}

/// What a caller asks a refresh to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshRequest {
    pub market: String,
    /// An explicit session bypasses the freshness check: today's ledger entry cannot vouch
    /// for a dated backfill.
    pub session_date: Option<NaiveDate>,
    pub force: bool,
    pub include_indicators: bool,
}

impl Default for RefreshRequest {
    fn default() -> Self {
        Self {
            market: "US".to_string(),
            session_date: None,
            force: false,
            include_indicators: true,
        }
    }
}

/// The four answers a refresh can give.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RefreshStatus {
    Refreshed,
    AlreadyFresh,
    Busy,
    NoVendorKey,
}

impl RefreshStatus {
    /// The token the JSON contract uses. Nothing else may spell these out.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Refreshed => "refreshed",
            Self::AlreadyFresh => "already-fresh",
            Self::Busy => "busy",
            Self::NoVendorKey => "no-vendor-key",
        }
    }
}

/// The summary a refresh returns.
///
/// The key set varies by status exactly as the source service's does: a refusal carries only
/// `status`, `detail` and `units`; a refreshed pull carries the session accounting and either
/// a null or a real expected session. [`Presence`] and the skipped options reproduce that.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RefreshOutcome {
    pub status: RefreshStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<crate::jobs::daily::SessionSummary>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows_written: Option<usize>,
    #[serde(skip_serializing_if = "Presence::is_absent")]
    pub expected_session: Presence<String>,
    #[serde(skip_serializing_if = "Presence::is_absent")]
    pub newest_session: Presence<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behind_expected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indicators: Option<RebuildSummary>,
    pub units: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl RefreshOutcome {
    fn refusal(status: RefreshStatus, detail: impl Into<String>) -> Self {
        Self {
            status,
            market: None,
            sessions: None,
            rows_written: None,
            expected_session: Presence::Absent,
            newest_session: Presence::Absent,
            behind_expected: None,
            indicators: None,
            units: 0,
            duration_ms: None,
            detail: Some(detail.into()),
        }
    }
}

/// Pull the whole market into `DailySnapshot`, optionally rebuilding ATR.
///
/// Returns a summary rather than raising for the ordinary outcomes: "already fresh" and
/// "someone else is doing it" are answers, not errors. A store or vendor failure during the
/// pull is still a failure, because nothing was refreshed.
pub async fn refresh_daily_bars(
    store: &dyn RefreshStore,
    client: Option<&dyn RefreshVendor>,
    jobs: &dyn RefreshJobs,
    settings: &Settings,
    gate: &RefreshGate,
    clock: &dyn Clock,
    request: RefreshRequest,
) -> Result<RefreshOutcome, RefreshFailure> {
    let Some(client) = client else {
        return Ok(RefreshOutcome::refusal(
            RefreshStatus::NoVendorKey,
            "no EODHD key; store one with POST /secrets/eodhd",
        ));
    };

    if !request.force && request.session_date.is_none() && is_fresh(store, &request.market).await {
        return Ok(RefreshOutcome::refusal(
            RefreshStatus::AlreadyFresh,
            format!(
                "a {} pull is already recorded today; pass force=true to pull again",
                bulk_request_type(&request.market)
            ),
        ));
    }

    let Some(_slot) = gate.try_acquire() else {
        return Ok(RefreshOutcome::refusal(
            RefreshStatus::Busy,
            "a refresh is already running",
        ));
    };

    let started = Instant::now();
    let units_before = client.units_spent();
    // What the venue's own clock says should be available, reported next to what the pull
    // actually returned: "the refresh ran" and "the refresh produced the expected session"
    // are different claims, and only the second is useful.
    let expected = latest_completed_session(Some(&request.market), clock.now_utc()).to_string();

    let pull = jobs
        .pull(
            store,
            client,
            &request.market,
            request.session_date.map(|date| vec![date]),
        )
        .await?;
    let newest = pull.sessions.last().map(|session| session.session.clone());
    let behind_expected = newest
        .as_deref()
        .is_some_and(|newest| newest < expected.as_str());
    let indicators = if request.include_indicators {
        Some(jobs.rebuild_indicators(store, settings).await?)
    } else {
        None
    };

    let units = client.units_spent() - units_before;
    let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let outcome = RefreshOutcome {
        status: RefreshStatus::Refreshed,
        market: Some(request.market.clone()),
        sessions: Some(pull.sessions),
        rows_written: Some(pull.rows_written),
        expected_session: Presence::Value(expected.clone()),
        newest_session: Presence::from_option(newest.clone()),
        behind_expected: Some(behind_expected),
        indicators,
        units,
        duration_ms: Some(duration_ms),
        detail: None,
    };

    tracing::info!(
        "bulk refresh: {} rows over {} session(s) in {duration_ms}ms, {} units",
        outcome.rows_written.unwrap_or(0),
        outcome.sessions.as_ref().map_or(0, Vec::len),
        outcome.units
    );
    if behind_expected {
        tracing::warn!(
            "bulk refresh returned session {} but {expected} was expected — vendor lag or a \
             venue holiday",
            newest.unwrap_or_default()
        );
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_market_has_its_own_freshness_row() {
        assert_eq!(bulk_request_type("US"), "/eod-bulk-last-day/US");
        assert_eq!(bulk_request_type("au"), "/eod-bulk-last-day/AU");
    }

    #[tokio::test]
    async fn the_gate_admits_one_refresh_and_the_second_is_refused_without_waiting() {
        let gate = RefreshGate::new();
        let held = gate.try_acquire();
        assert!(held.is_some());
        assert!(gate.try_acquire().is_none());
        drop(held);
        assert!(gate.try_acquire().is_some());
    }

    #[test]
    fn a_refusal_carries_only_the_fields_a_refusal_has() {
        let outcome = RefreshOutcome::refusal(RefreshStatus::Busy, "a refresh is already running");
        let json = serde_json::to_value(&outcome).unwrap();
        assert_eq!(json["status"], "busy");
        assert_eq!(json["units"], 0);
        assert_eq!(json["detail"], "a refresh is already running");
        assert!(json.get("market").is_none());
        assert!(json.get("sessions").is_none());
        assert!(json.get("expected_session").is_none());
    }
}
