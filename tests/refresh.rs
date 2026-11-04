//! The bulk refresh policy: when it may run, what it reports, and what it refuses.
//!
//! Ported from the source suite's `test_refresh.py`. The expensive facts are pinned here: a
//! second pull in the same local day is refused before it can spend another 100 units, a
//! concurrent caller is told the refresh is running rather than queued, and the reported unit
//! count is read from the vendor meter rather than assumed from the tariff.
//!
//! The source suite let the broken-calendar case raise; the Rust session rule is infallible
//! and reports the previous UTC date where a zone cannot be resolved, so that case has no
//! direct equivalent. It is recorded in `TODO/Loose-Ends.md`.

mod common;

use std::sync::atomic::AtomicI64;
use std::sync::Arc;

use common::{date, pinned_clock, FakeJobs, FakeStore, FakeVendor, TestClock};
use mmds::config::Settings;
use mmds::error::ProviderFailure;
use mmds::jobs::daily::{PullSummary, RebuildSummary, SessionSummary};
use mmds::presence::Presence;
use mmds::refresh::{
    bulk_request_type, is_fresh, refresh_daily_bars, RefreshFailure, RefreshGate, RefreshRequest,
    RefreshStatus,
};

fn settings() -> Settings {
    common::test_settings()
}

/// One session's row count, as the pull would report it.
fn session(day: u32) -> SessionSummary {
    SessionSummary {
        session: date(2026, 10, day).to_string(),
        received: 10,
        written: 10,
        rows_without_adjusted_close: 0,
    }
}

fn pull_of(day: u32) -> PullSummary {
    PullSummary {
        sessions: vec![session(day)],
        rows_written: 10,
    }
}

async fn run(
    store: &FakeStore,
    vendor: Option<&FakeVendor>,
    jobs: &FakeJobs,
    gate: &RefreshGate,
    clock: &TestClock,
    request: RefreshRequest,
) -> Result<mmds::refresh::RefreshOutcome, RefreshFailure> {
    let settings = settings();
    refresh_daily_bars(
        store,
        vendor.map(|vendor| vendor as &dyn mmds::refresh::RefreshVendor),
        jobs,
        &settings,
        gate,
        clock,
        request,
    )
    .await
}

// ---------------------------------------------------------------------------
// freshness: the ledger, not the newest row
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_market_with_a_pull_recorded_today_is_fresh() {
    let store = FakeStore::new();
    assert!(!is_fresh(&store, "US").await);

    store.seed_pull("US", 1);

    assert!(is_fresh(&store, "US").await);
}

#[tokio::test]
async fn each_market_consults_its_own_ledger_row() {
    // The hard-coded `/US` bug: an ASX refresh read New York's row and skipped the ASX session.
    let store = FakeStore::new();
    store.seed_pull("US", 1);

    assert!(is_fresh(&store, "US").await);
    assert!(!is_fresh(&store, "AU").await);
    assert_eq!(bulk_request_type("au"), "/eod-bulk-last-day/AU");
}

#[tokio::test]
async fn an_unreadable_ledger_is_treated_as_stale() {
    // Guessing fresh silently stops refreshing; guessing stale costs one pull.
    let store = FakeStore::new();
    store.seed_pull("US", 1);
    store.fail_ledger(true);

    assert!(!is_fresh(&store, "US").await);
}

// ---------------------------------------------------------------------------
// the four answers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_vendor_key_is_reported_before_anything_is_attempted() {
    let store = FakeStore::new();
    let jobs = FakeJobs::new();
    let gate = RefreshGate::new();
    let clock = pinned_clock();

    let outcome = run(
        &store,
        None,
        &jobs,
        &gate,
        &clock,
        RefreshRequest::default(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.status, RefreshStatus::NoVendorKey);
    assert_eq!(outcome.units, 0);
    assert_eq!(outcome.market, None);
    assert!(outcome.detail.unwrap().contains("no EODHD key"));
    assert_eq!(jobs.pulls(), 0);
}

#[tokio::test]
async fn a_pull_already_recorded_today_is_refused_without_spending() {
    let store = FakeStore::new();
    store.seed_pull("US", 1);
    let jobs = FakeJobs::new();
    let vendor = FakeVendor::new(Arc::new(AtomicI64::new(0)));
    let gate = RefreshGate::new();
    let clock = pinned_clock();

    let outcome = run(
        &store,
        Some(&vendor),
        &jobs,
        &gate,
        &clock,
        RefreshRequest::default(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.status, RefreshStatus::AlreadyFresh);
    assert_eq!(outcome.units, 0);
    assert_eq!(jobs.pulls(), 0);
    assert_eq!(vendor.bulk_calls(), 0);
    assert!(outcome.detail.unwrap().contains("force=true"));
}

#[tokio::test]
async fn force_pulls_again_over_todays_entry() {
    let store = FakeStore::new();
    store.seed_pull("US", 1);
    let jobs = FakeJobs::new();
    let vendor = FakeVendor::new(Arc::new(AtomicI64::new(0)));
    let gate = RefreshGate::new();
    let clock = pinned_clock();
    let request = RefreshRequest {
        force: true,
        ..RefreshRequest::default()
    };

    let outcome = run(&store, Some(&vendor), &jobs, &gate, &clock, request)
        .await
        .unwrap();

    assert_eq!(outcome.status, RefreshStatus::Refreshed);
    assert_eq!(jobs.pulls(), 1);
}

#[tokio::test]
async fn a_dated_backfill_bypasses_todays_freshness_entry() {
    // Today's ledger row cannot vouch for a dated pull, so an explicit date is not suppressed.
    let store = FakeStore::new();
    store.seed_pull("US", 1);
    let jobs = FakeJobs::new();
    let vendor = FakeVendor::new(Arc::new(AtomicI64::new(0)));
    let gate = RefreshGate::new();
    let clock = pinned_clock();
    let requested = date(2026, 10, 28);
    let request = RefreshRequest {
        session_date: Some(requested),
        ..RefreshRequest::default()
    };

    let outcome = run(&store, Some(&vendor), &jobs, &gate, &clock, request)
        .await
        .unwrap();

    assert_eq!(outcome.status, RefreshStatus::Refreshed);
    assert_eq!(jobs.pulls(), 1);
    assert_eq!(jobs.dates(), Some(vec![requested]));
}

#[tokio::test]
async fn a_second_caller_is_told_the_refresh_is_running() {
    let store = FakeStore::new();
    let jobs = FakeJobs::new();
    let vendor = FakeVendor::new(Arc::new(AtomicI64::new(0)));
    let gate = RefreshGate::new();
    let clock = pinned_clock();
    let held = gate.try_acquire().expect("the gate starts free");

    let outcome = run(
        &store,
        Some(&vendor),
        &jobs,
        &gate,
        &clock,
        RefreshRequest::default(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.status, RefreshStatus::Busy);
    assert_eq!(outcome.units, 0);
    assert_eq!(jobs.pulls(), 0);

    drop(held);
    let outcome = run(
        &store,
        Some(&vendor),
        &jobs,
        &gate,
        &clock,
        RefreshRequest::default(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, RefreshStatus::Refreshed);
}

#[tokio::test]
async fn the_gate_is_released_when_a_refresh_finishes() {
    let store = FakeStore::new();
    let jobs = FakeJobs::new();
    let vendor = FakeVendor::new(Arc::new(AtomicI64::new(0)));
    let gate = RefreshGate::new();
    let clock = pinned_clock();
    let forced = || RefreshRequest {
        force: true,
        ..RefreshRequest::default()
    };

    run(&store, Some(&vendor), &jobs, &gate, &clock, forced())
        .await
        .unwrap();
    let outcome = run(&store, Some(&vendor), &jobs, &gate, &clock, forced())
        .await
        .unwrap();

    assert_eq!(outcome.status, RefreshStatus::Refreshed);
    assert_eq!(jobs.pulls(), 2);
}

#[tokio::test]
async fn a_pull_failure_is_raised_and_the_gate_reopens() {
    let store = FakeStore::new();
    let jobs = FakeJobs::new().failing(RefreshFailure::Provider(ProviderFailure::new(
        "http-error",
        "500",
        Some(500),
    )));
    let vendor = FakeVendor::new(Arc::new(AtomicI64::new(0)));
    let gate = RefreshGate::new();
    let clock = pinned_clock();

    let error = run(
        &store,
        Some(&vendor),
        &jobs,
        &gate,
        &clock,
        RefreshRequest::default(),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, RefreshFailure::Provider(_)));
    assert!(gate.try_acquire().is_some());
}

// ---------------------------------------------------------------------------
// the refreshed summary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_refresh_reports_the_session_rows_and_units_it_produced() {
    let store = FakeStore::new();
    let units = Arc::new(AtomicI64::new(0));
    let vendor = FakeVendor::new(units.clone());
    let jobs = FakeJobs::new().with_pull(pull_of(29), units.clone(), 100);
    let gate = RefreshGate::new();
    let clock = pinned_clock();

    let outcome = run(
        &store,
        Some(&vendor),
        &jobs,
        &gate,
        &clock,
        RefreshRequest::default(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.status, RefreshStatus::Refreshed);
    assert_eq!(outcome.market.as_deref(), Some("US"));
    assert_eq!(outcome.sessions, Some(pull_of(29).sessions));
    assert_eq!(outcome.rows_written, Some(10));
    assert_eq!(
        outcome.expected_session,
        Presence::Value("2026-10-29".into())
    );
    assert_eq!(outcome.newest_session, Presence::Value("2026-10-29".into()));
    assert_eq!(outcome.behind_expected, Some(false));
    // Read from the client's meter, not assumed from the tariff.
    assert_eq!(outcome.units, 100);
    assert!(outcome.duration_ms.is_some());
}

#[tokio::test]
async fn a_pull_that_returns_an_older_session_is_flagged_as_behind() {
    // "The refresh ran" and "the refresh produced the expected session" are different claims.
    let store = FakeStore::new();
    let units = Arc::new(AtomicI64::new(0));
    let vendor = FakeVendor::new(units.clone());
    let jobs = FakeJobs::new().with_pull(pull_of(28), units.clone(), 100);
    let gate = RefreshGate::new();
    let clock = pinned_clock();

    let outcome = run(
        &store,
        Some(&vendor),
        &jobs,
        &gate,
        &clock,
        RefreshRequest::default(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.behind_expected, Some(true));
    assert_eq!(outcome.newest_session, Presence::Value("2026-10-28".into()));
    assert_eq!(
        outcome.expected_session,
        Presence::Value("2026-10-29".into())
    );
}

#[tokio::test]
async fn a_refresh_rebuilds_indicators_unless_told_not_to() {
    let store = FakeStore::new();
    let jobs = FakeJobs::new();
    let vendor = FakeVendor::new(Arc::new(AtomicI64::new(0)));
    let gate = RefreshGate::new();
    let clock = pinned_clock();

    let outcome = run(
        &store,
        Some(&vendor),
        &jobs,
        &gate,
        &clock,
        RefreshRequest::default(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.indicators, Some(RebuildSummary::default()));
    assert_eq!(jobs.rebuilds(), 1);

    let request = RefreshRequest {
        force: true,
        include_indicators: false,
        ..RefreshRequest::default()
    };
    let outcome = run(&store, Some(&vendor), &jobs, &gate, &clock, request)
        .await
        .unwrap();
    assert_eq!(outcome.indicators, None);
    assert_eq!(jobs.rebuilds(), 1);
}

#[tokio::test]
async fn a_market_is_pulled_by_the_name_it_was_asked_for() {
    let store = FakeStore::new();
    let jobs = FakeJobs::new();
    let vendor = FakeVendor::new(Arc::new(AtomicI64::new(0)));
    let gate = RefreshGate::new();
    let clock = pinned_clock();
    let request = RefreshRequest {
        market: "AU".to_string(),
        ..RefreshRequest::default()
    };

    let outcome = run(&store, Some(&vendor), &jobs, &gate, &clock, request)
        .await
        .unwrap();

    // Sydney has settled 2026-10-29 at 00:00 UTC on the 30th, exactly as New York has.
    assert_eq!(outcome.market.as_deref(), Some("AU"));
    assert_eq!(
        outcome.expected_session,
        Presence::Value("2026-10-29".into())
    );
}
