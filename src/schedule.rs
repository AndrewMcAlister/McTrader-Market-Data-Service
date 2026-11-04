//! The refresh schedule: bulk data at session open + 45 minutes, once, with one retry.
//!
//! # Why it is tied to the session
//!
//! The book ranks on a whole-market pull, and the pull has to be taken while the venue is
//! open. A price collected at 02:00 on a Sunday is either a stale echo or a synthetic print,
//! and both are the kind of quote a manipulator can leave lying around for a screener to
//! find. So collection is tied to the session, not to a wall clock:
//!
//! ```text
//! US   NYSE opens 09:30 New York  -> fire at 10:15 New York
//! AU   ASX  opens 10:00 Sydney    -> fire at 10:45 Sydney
//! ```
//!
//! Both are computed from the market clock, so a daylight-saving change moves the fire time
//! with the session instead of leaving it an hour out.
//!
//! # What happens when it fails
//!
//! One attempt, then exactly one retry fifteen minutes later, then nothing. No backoff, no
//! third try: after the retry a failed session is a person's problem, and `GET /schedule`
//! says so rather than retrying quietly forever. A retry at open + 60 is still inside the
//! session, which is the only window the data is worth having.
//!
//! # What it does not do
//!
//! It does not backfill. If the service was down all session, the day stays missing and the
//! next day's pull carries only its own session; the daily job's `--since` is how a gap is
//! closed by hand. And it never fires while the venue is closed: [`RefreshScheduler::tick`]
//! refuses, and the collection itself re-checks `in_session` before it writes anything,
//! because a guard on the schedule alone can be defeated by a manual run.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use tokio::task::JoinHandle;

use crate::clock::{iso_utc, MarketClock, ResolvedSession};
use crate::store::models::ExchangeRow;

/// One venue whose bulk data is collected on a schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketWindow {
    /// Vendor market code, as `/eod-bulk-last-day/{market}`.
    pub market: &'static str,
    /// Exchange code in the clock reference.
    pub exchange: &'static str,
    pub label: &'static str,
}

/// The two markets this service collects for. The clock resolves each one's own hours.
pub const MARKETS: [MarketWindow; 2] = [
    MarketWindow {
        market: "US",
        exchange: "NYSE",
        label: "US equities",
    },
    MarketWindow {
        market: "AU",
        exchange: "ASX",
        label: "Australian equities",
    },
];

/// Why the schedule could not resolve a window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleError {
    pub message: String,
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ScheduleError {}

impl ScheduleError {
    fn unknown_exchange(exchange: &str) -> Self {
        Self {
            message: format!("{exchange} is not in the clock reference"),
        }
    }

    fn no_stated_hours(exchange: &str) -> Self {
        Self {
            message: format!("{exchange} has no stated session hours"),
        }
    }
}

/// When to fire, derived from the clock rather than from a fixed hour.
#[derive(Debug, Clone)]
pub struct SessionSchedule {
    clock: Arc<MarketClock>,
    offset: Duration,
    retry: Duration,
}

impl SessionSchedule {
    pub fn new(clock: Arc<MarketClock>, offset_minutes: i64, retry_minutes: i64) -> Self {
        Self {
            clock,
            offset: Duration::minutes(offset_minutes),
            retry: Duration::minutes(retry_minutes),
        }
    }

    pub fn clock(&self) -> &MarketClock {
        &self.clock
    }

    pub fn offset(&self) -> Duration {
        self.offset
    }

    pub fn retry(&self) -> Duration {
        self.retry
    }

    fn exchange(&self, window: MarketWindow) -> Result<&ExchangeRow, ScheduleError> {
        self.clock
            .exchanges()
            .find(|row| row.code.as_deref() == Some(window.exchange))
            .ok_or_else(|| ScheduleError::unknown_exchange(window.exchange))
    }

    pub fn session(
        &self,
        window: MarketWindow,
        at: DateTime<Utc>,
    ) -> Result<ResolvedSession, ScheduleError> {
        Ok(self
            .clock
            .exchange_session(self.exchange(window)?, at, false))
    }

    pub fn in_session(&self, window: MarketWindow, at: DateTime<Utc>) -> bool {
        self.session(window, at)
            .is_ok_and(|session| session.is_open)
    }

    /// Today's fire instant for this venue: the session open plus the offset.
    ///
    /// Read off the resolved session, so it is already in UTC and already DST-correct. On a
    /// day the venue is shut, the session resolves to the *next* open, which is what the
    /// caller wants: the fire time for the day it will actually trade.
    pub fn fire_time(
        &self,
        window: MarketWindow,
        at: DateTime<Utc>,
    ) -> Result<DateTime<Utc>, ScheduleError> {
        let session = self.session(window, at)?;
        session
            .open_utc
            .map(|open| open + self.offset)
            .ok_or_else(|| ScheduleError::no_stated_hours(window.exchange))
    }

    /// The first fire instant strictly after `at` — for reporting, not for firing.
    pub fn next_fire(
        &self,
        window: MarketWindow,
        at: DateTime<Utc>,
    ) -> Result<DateTime<Utc>, ScheduleError> {
        let candidate = self.fire_time(window, at)?;
        if candidate <= at {
            return self.fire_time(window, at + Duration::days(1));
        }
        Ok(candidate)
    }
}

/// What one run of a market produced.
#[derive(Debug, Clone, PartialEq)]
pub struct MarketRun {
    pub ok: bool,
    pub status: String,
    pub detail: String,
    /// The full answer `/schedule/run` returns and the scheduler records only the summary of.
    pub payload: Value,
}

impl MarketRun {
    pub fn new(ok: bool, status: impl Into<String>, detail: impl Into<String>) -> Self {
        let status = status.into();
        let detail = detail.into();
        let payload = json!({"ok": ok, "status": status, "detail": detail});
        Self {
            ok,
            status,
            detail,
            payload,
        }
    }
}

/// The collection a market run performs.
///
/// The scheduler owns the policy — when to run, how often to retry — and this trait owns the
/// work, so a test can pin the state machine without a vendor, a database or a clock.
#[async_trait]
pub trait MarketRunner: Send + Sync {
    async fn run_market(&self, market: &str) -> MarketRun;
}

/// One recorded attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub at: DateTime<Utc>,
    pub ok: bool,
    pub status: String,
    pub detail: String,
}

/// Per-market in-memory state for the current session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarketState {
    /// The venue's date this state belongs to.
    pub session: Option<String>,
    pub attempts: Vec<Attempt>,
}

impl MarketState {
    pub fn done(&self) -> bool {
        self.attempts.iter().any(|attempt| attempt.ok)
    }

    pub fn exhausted(&self) -> bool {
        self.attempts.len() >= 2 && !self.done()
    }
}

#[derive(Debug, Default)]
struct SchedulerState {
    states: std::collections::BTreeMap<String, MarketState>,
    last_tick: Option<DateTime<Utc>>,
    last_error: Option<String>,
}

/// One attempt, one retry, then a person. Never outside the session.
pub struct RefreshScheduler {
    schedule: Arc<SessionSchedule>,
    runner: Arc<dyn MarketRunner>,
    markets: Vec<MarketWindow>,
    offset: Duration,
    retry: Duration,
    state: Mutex<SchedulerState>,
    gate: tokio::sync::Mutex<()>,
    enabled: AtomicBool,
    stop: Arc<tokio::sync::Notify>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl RefreshScheduler {
    pub fn new(
        schedule: Arc<SessionSchedule>,
        runner: Arc<dyn MarketRunner>,
        markets: Vec<MarketWindow>,
    ) -> Self {
        let states = markets
            .iter()
            .map(|window| (window.market.to_string(), MarketState::default()))
            .collect();
        let offset = schedule.offset();
        let retry = schedule.retry();
        Self {
            schedule,
            runner,
            markets,
            offset,
            retry,
            state: Mutex::new(SchedulerState {
                states,
                ..SchedulerState::default()
            }),
            gate: tokio::sync::Mutex::new(()),
            enabled: AtomicBool::new(false),
            stop: Arc::new(tokio::sync::Notify::new()),
            handle: Mutex::new(None),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, SchedulerState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The venue's own calendar date, so a new session starts with clean state.
    fn date_key(session: &ResolvedSession, at: DateTime<Utc>) -> String {
        session
            .open_utc
            .unwrap_or(at)
            .date_naive()
            .format("%Y-%m-%d")
            .to_string()
    }

    /// One scheduling pass. Returns what it did, for the log and for tests.
    pub async fn tick(&self, at: Option<DateTime<Utc>>) -> Vec<Value> {
        let now = at.unwrap_or_else(Utc::now);
        let Ok(_slot) = self.gate.try_lock() else {
            return Vec::new();
        };
        self.lock_state().last_tick = Some(now);

        let mut actions = Vec::new();
        for window in self.markets.clone() {
            let session = match self.schedule.session(window, now) {
                Ok(session) => session,
                Err(error) => {
                    self.lock_state().last_error = Some(error.message);
                    continue;
                }
            };
            if !session.is_open {
                // Closed: nothing is collected, and nothing is recorded as a failure —
                // a closed venue is not an outage.
                continue;
            }

            let key = Self::date_key(&session, now);
            let attempt_number = {
                let mut state = self.lock_state();
                let market = state.states.entry(window.market.to_string()).or_default();
                if market.session.as_deref() != Some(key.as_str()) {
                    market.session = Some(key.clone());
                    market.attempts.clear();
                }
                let due = Self::due(market, &session, now, self.offset, self.retry);
                if !due {
                    continue;
                }
                market.attempts.len() + 1
            };

            let result = self.runner.run_market(window.market).await;
            let attempt = Attempt {
                at: now,
                ok: result.ok,
                status: result.status.clone(),
                detail: result.detail.clone(),
            };
            let mut state = self.lock_state();
            let market = state.states.entry(window.market.to_string()).or_default();
            market.attempts.push(attempt);
            if !result.ok && market.attempts.len() >= 2 {
                state.last_error = Some(format!(
                    "{} failed twice; manual intervention required for session {key}",
                    window.market
                ));
            }
            drop(state);

            actions.push(json!({
                "market": window.market,
                "attempt": attempt_number,
                "ok": result.ok,
                "status": result.status,
                "detail": result.detail,
                "session": key,
                "at": iso_utc(now),
            }));
        }
        actions
    }

    fn due(
        market: &MarketState,
        session: &ResolvedSession,
        now: DateTime<Utc>,
        offset: Duration,
        retry: Duration,
    ) -> bool {
        if market.done() || market.exhausted() {
            return false;
        }
        let Some(open) = session.open_utc else {
            return false;
        };
        let fire = open + offset;
        if market.attempts.is_empty() {
            return now >= fire;
        }
        now >= fire + retry
    }

    /// Start the 20-second loop. Does nothing when it is already running.
    pub fn start(self: &Arc<Self>, tick_seconds: u64) {
        if self.enabled.swap(true, Ordering::SeqCst) {
            return;
        }
        let scheduler = Arc::clone(self);
        let stop = self.stop.clone();
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(StdDuration::from_secs(tick_seconds));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The source loop waits one interval before its first tick.
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        scheduler.tick(None).await;
                    }
                    _ = stop.notified() => break,
                }
            }
            scheduler.enabled.store(false, Ordering::SeqCst);
        });
        *self
            .handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);
    }

    pub fn stop(&self) {
        self.stop.notify_waiters();
        if let Some(handle) = self
            .handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            handle.abort();
        }
        self.enabled.store(false, Ordering::SeqCst);
    }

    /// One market's collection, triggered by a person through `POST /schedule/run`.
    ///
    /// It does not touch the attempt count: a manual run must not consume the session's one
    /// retry, and the collection itself re-checks [`SessionSchedule::in_session`].
    pub async fn run_market(&self, market: &str) -> MarketRun {
        self.runner.run_market(market).await
    }

    /// What `/schedule` answers: the state of each market, and what it did today.
    pub fn status(&self) -> Result<Value, ScheduleError> {
        let now = Utc::now();
        let mut rows = Vec::with_capacity(self.markets.len());
        for window in &self.markets {
            let session = self.schedule.session(*window, now)?;
            let state = self.lock_state();
            let market = state.states.get(window.market).cloned().unwrap_or_default();
            drop(state);
            let attempts: Vec<Value> = market
                .attempts
                .iter()
                .map(|attempt| {
                    json!({
                        "at": iso_utc(attempt.at),
                        "ok": attempt.ok,
                        "status": attempt.status,
                        "detail": attempt.detail,
                    })
                })
                .collect();
            let label = if market.done() {
                "done"
            } else if market.exhausted() {
                "manual-intervention"
            } else {
                "waiting"
            };
            rows.push(json!({
                "market": window.market,
                "label": window.label,
                "exchange": window.exchange,
                "is_open": session.is_open,
                "session_open_utc": session.open_utc.map(iso_utc),
                "session_close_utc": session.close_utc.map(iso_utc),
                "fire_at_utc": session.open_utc.map(|open| iso_utc(open + self.offset)),
                "next_fire_utc": self.schedule.next_fire(*window, now).map(iso_utc)?,
                "session": market.session,
                "attempts": attempts,
                "state": label,
            }));
        }
        let state = self.lock_state();
        Ok(json!({
            "enabled": self.enabled.load(Ordering::SeqCst),
            "last_tick_utc": state.last_tick.map(iso_utc),
            "last_error": state.last_error,
            "markets": rows,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn clock() -> Arc<MarketClock> {
        Arc::new(MarketClock::from_json(include_str!("../data/clock.json")).unwrap())
    }

    fn schedule() -> Arc<SessionSchedule> {
        Arc::new(SessionSchedule::new(clock(), 45, 15))
    }

    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    struct Recorder {
        calls: StdMutex<Vec<String>>,
        result: MarketRun,
    }

    impl Recorder {
        fn new(result: MarketRun) -> Arc<Self> {
            Arc::new(Self {
                calls: StdMutex::new(Vec::new()),
                result,
            })
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl MarketRunner for Recorder {
        async fn run_market(&self, market: &str) -> MarketRun {
            self.calls.lock().unwrap().push(market.to_string());
            self.result.clone()
        }
    }

    fn ok_runner() -> Arc<Recorder> {
        Recorder::new(MarketRun::new(true, "refreshed", ""))
    }

    #[test]
    fn the_us_fire_time_is_the_open_plus_forty_five_minutes() {
        // NYSE 09:30 New York, in daylight time, is 13:30Z -> fire 14:15Z.
        let schedule = schedule();
        let fire = schedule
            .fire_time(MARKETS[0], utc(2026, 10, 30, 15, 0))
            .unwrap();
        assert_eq!(fire, utc(2026, 10, 30, 14, 15));
    }

    #[test]
    fn the_fire_time_moves_with_the_clock_change() {
        // From 1 November New York is EST: 09:30 -> 14:30Z -> fire 15:15Z.
        let schedule = schedule();
        let fire = schedule
            .fire_time(MARKETS[0], utc(2026, 11, 2, 15, 0))
            .unwrap();
        assert_eq!(fire, utc(2026, 11, 2, 15, 15));
    }

    #[test]
    fn the_asx_fire_time_is_its_own_open_not_the_us_one() {
        // ASX 10:00 Sydney in November (AEDT) is 23:00Z the previous day -> fire 23:45Z.
        let schedule = schedule();
        let fire = schedule
            .fire_time(MARKETS[1], utc(2026, 11, 4, 1, 0))
            .unwrap();
        assert_eq!(fire, utc(2026, 11, 3, 23, 45));
    }

    #[tokio::test]
    async fn a_closed_venue_is_not_in_session_and_nothing_is_collected() {
        let schedule = schedule();
        assert!(!schedule.in_session(MARKETS[0], utc(2026, 10, 31, 12, 0)));

        let runner = ok_runner();
        let scheduler = RefreshScheduler::new(schedule.clone(), runner.clone(), MARKETS.to_vec());
        assert!(scheduler
            .tick(Some(utc(2026, 10, 31, 12, 0)))
            .await
            .is_empty());
        assert!(runner.calls().is_empty());
    }

    #[tokio::test]
    async fn the_first_attempt_lands_on_the_fire_time() {
        let runner = ok_runner();
        let scheduler = RefreshScheduler::new(schedule(), runner.clone(), MARKETS.to_vec());
        assert!(scheduler
            .tick(Some(utc(2026, 10, 30, 14, 10)))
            .await
            .is_empty());
        assert!(runner.calls().is_empty());
        let actions = scheduler.tick(Some(utc(2026, 10, 30, 14, 15))).await;
        let markets: Vec<&str> = actions
            .iter()
            .filter_map(|action| action["market"].as_str())
            .collect();
        assert_eq!(markets, vec!["US"]);
        assert_eq!(runner.calls(), vec!["US"]);
    }

    #[tokio::test]
    async fn a_failure_is_retried_once_fifteen_minutes_later_and_then_left_alone() {
        let runner = Recorder::new(MarketRun::new(false, "error", "vendor 500"));
        let scheduler = RefreshScheduler::new(schedule(), runner.clone(), MARKETS.to_vec());
        scheduler.tick(Some(utc(2026, 10, 30, 14, 15))).await;
        assert!(scheduler
            .tick(Some(utc(2026, 10, 30, 14, 20)))
            .await
            .is_empty());
        let actions = scheduler.tick(Some(utc(2026, 10, 30, 14, 30))).await;
        assert_eq!(actions[0]["attempt"], 2);
        assert!(scheduler
            .tick(Some(utc(2026, 10, 30, 14, 45)))
            .await
            .is_empty());
        assert!(scheduler
            .tick(Some(utc(2026, 10, 30, 15, 0)))
            .await
            .is_empty());
        assert_eq!(runner.calls(), vec!["US", "US"]);
        let status = scheduler.status().unwrap();
        assert_eq!(status["markets"][0]["state"], "manual-intervention");
        assert_eq!(
            status["markets"][0]["attempts"].as_array().unwrap().len(),
            2
        );
    }

    #[tokio::test]
    async fn a_success_stops_the_retry() {
        let runner = ok_runner();
        let scheduler = RefreshScheduler::new(schedule(), runner.clone(), MARKETS.to_vec());
        scheduler.tick(Some(utc(2026, 10, 30, 14, 15))).await;
        scheduler.tick(Some(utc(2026, 10, 30, 14, 30))).await;
        assert_eq!(runner.calls(), vec!["US"]);
        assert_eq!(scheduler.status().unwrap()["markets"][0]["state"], "done");
    }

    #[tokio::test]
    async fn both_markets_are_scheduled_even_though_only_one_is_open() {
        // At 23:45Z the ASX is opening and New York is closed — one fires, one does not.
        let runner = ok_runner();
        let scheduler = RefreshScheduler::new(schedule(), runner.clone(), MARKETS.to_vec());
        let actions = scheduler.tick(Some(utc(2026, 11, 3, 23, 45))).await;
        let markets: Vec<&str> = actions
            .iter()
            .filter_map(|action| action["market"].as_str())
            .collect();
        assert_eq!(markets, vec!["AU"]);
        assert_eq!(runner.calls(), vec!["AU"]);
    }

    #[test]
    fn the_next_fire_rolls_to_monday_over_a_weekend() {
        let schedule = schedule();
        let fire = schedule
            .next_fire(MARKETS[0], utc(2026, 10, 31, 12, 0))
            .unwrap();
        assert_eq!(fire, utc(2026, 11, 2, 15, 15));
    }

    #[tokio::test]
    async fn a_new_session_starts_with_clean_state() {
        let runner = ok_runner();
        let scheduler = RefreshScheduler::new(schedule(), runner.clone(), MARKETS.to_vec());
        scheduler.tick(Some(utc(2026, 10, 30, 14, 15))).await;
        assert_eq!(scheduler.status().unwrap()["markets"][0]["state"], "done");
        // Monday is a different session, so it waits again rather than inheriting "done".
        scheduler.tick(Some(utc(2026, 11, 2, 15, 15))).await;
        assert_eq!(
            scheduler.status().unwrap()["markets"][0]["attempts"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
