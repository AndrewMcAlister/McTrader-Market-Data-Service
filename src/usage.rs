//! EODHD spend: priced from `dbo.RequestTypes`, recorded in `dbo.EODHDUsage`.
//!
//! # Why the table is the authority, not the code
//!
//! The per-call costs used to be literals at each call site (`units=1`, `units=100`). They
//! were measured, and the measurement lives in `dbo.RequestTypes` with its evidence — so a
//! re-measured price is a row update, not a release, and the number a reader checks is the
//! same number the code spent.
//!
//! The mapping from an internal purpose to a tariff row is here rather than at the call
//! sites, so adding a vendor endpoint means adding one entry instead of editing four
//! modules.
//!
//! # Free and unpriced are different answers
//!
//! A row with `CostEach = NULL` asserts no figure; a row with basis `free` asserts zero.
//! [`TariffEntry::price`] returns `None` for the first and `Some(0)` for the second, and the
//! ledger records both — an unpriced call is a spend surface that must stay visible, not a
//! call that never happened.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{Local, NaiveDate};

use crate::store::UsageRepository;

/// Internal purpose -> the `RequestTypes.RequestType` that prices it. A purpose whose row
/// depends on what was asked (`eod_bulk`, which names the market) is overridden at the call
/// site; the entry here is the default for a caller that names no market.
pub const PURPOSE_REQUEST_TYPE: &[(&str, &str)] = &[
    ("real_time", "/real-time/"),
    ("real_time_bulk", "/real-time?ex=US"),
    ("eod", "/eod/"),
    ("eod_bulk", "/eod-bulk-last-day/US"),
    ("intraday", "/intraday/"),
    ("screener", "/screener"),
];

/// What a call becomes when no tariff row matches it.
pub const UNKNOWN_REQUEST_TYPE: &str = "unknown";

/// What a caller becomes when none identifies itself.
pub const UNKNOWN_CALLER: &str = "unknown";

/// The request type a purpose is billed under, or `None` when the purpose is not one the
/// service knows.
pub fn request_type_for(purpose: &str) -> Option<&'static str> {
    PURPOSE_REQUEST_TYPE
        .iter()
        .find(|(name, _)| *name == purpose)
        .map(|(_, request_type)| *request_type)
}

/// One `RequestTypes` row: how this endpoint is billed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TariffEntry {
    pub request_type: String,
    pub cost_basis: String,
    pub cost_each: Option<i64>,
}

impl TariffEntry {
    pub fn new(
        request_type: impl Into<String>,
        cost_basis: impl Into<String>,
        cost_each: Option<i64>,
    ) -> Self {
        Self {
            request_type: request_type.into(),
            cost_basis: cost_basis.into(),
            cost_each,
        }
    }

    /// Units for one call, or `None` when the row is unpriced.
    pub fn price(&self, symbols: i64) -> Option<i64> {
        let cost_each = self.cost_each?;
        match self.cost_basis.as_str() {
            "free" => Some(0),
            // Batching saves wall-clock, not quota: three tickers in one request billed
            // three. Zero or negative symbols still bill one unit.
            "per_symbol" => Some(cost_each.saturating_mul(symbols.max(1))),
            // flat and per_call both bill once per call, whatever it returned.
            _ => Some(cost_each),
        }
    }
}

/// A `RequestTypes` row as the store reads it, before it becomes a [`TariffEntry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TariffRow {
    pub request_type: String,
    pub cost_basis: Option<String>,
    pub cost_each: Option<i64>,
}

/// The `RequestTypes` catalogue, loaded once and held in memory.
#[derive(Debug, Clone, Default)]
pub struct Tariff {
    by_type: BTreeMap<String, TariffEntry>,
}

impl Tariff {
    pub fn new(entries: impl IntoIterator<Item = TariffEntry>) -> Self {
        Self {
            by_type: entries
                .into_iter()
                .map(|entry| (entry.request_type.clone(), entry))
                .collect(),
        }
    }

    /// Build the catalogue from raw rows, applying the same normalisation as the source
    /// service: a missing basis is empty, and basis text is trimmed and lower-cased.
    pub fn from_rows(rows: impl IntoIterator<Item = TariffRow>) -> Self {
        Self::new(rows.into_iter().map(|row| {
            TariffEntry::new(
                row.request_type,
                row.cost_basis.unwrap_or_default().trim().to_lowercase(),
                row.cost_each,
            )
        }))
    }

    pub fn len(&self) -> usize {
        self.by_type.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_type.is_empty()
    }

    pub fn entry(&self, request_type: &str) -> Option<&TariffEntry> {
        self.by_type.get(request_type)
    }

    pub fn price(&self, request_type: &str, symbols: i64) -> Option<i64> {
        self.entry(request_type)
            .and_then(|entry| entry.price(symbols))
    }

    /// Every row, sorted by request type — the shape `/stats` and diagnostics render.
    pub fn listing(&self) -> Vec<&TariffEntry> {
        self.by_type.values().collect()
    }
}

/// The ledger's day, in local time — the same boundary the store writes under.
///
/// It was UTC, which filed a Sydney morning's spend under the previous day and made "today"
/// in the table mean yesterday at the desk. The trade, stated once: a local date cannot be
/// compared across timezones without knowing this one, and the vendor's own quota resets on
/// UTC, so the ledger's day and the vendor's day are offset by however many hours this site
/// is ahead.
pub fn today_local() -> NaiveDate {
    Local::now().date_naive()
}

/// Prices and records one vendor call, returning the units it cost.
///
/// The client treats a `None` answer as "no figure was asserted" and falls back to its own
/// literal unit count. Implementations must not let an accounting failure escape: a ledger
/// write failing is a reason to keep serving the price, not to fail it.
#[async_trait]
pub trait UsageRecorder: Send + Sync {
    async fn record(&self, request_type: &str, symbols: i64) -> Option<i64>;
}

tokio::task_local! {
    /// The caller attributed to vendor spend inside this task.
    static REQUEST_CALLER: String;
}

/// The caller name as it is recorded: trimmed, at most 128 characters, never blank.
///
/// The source service held this in a `ContextVar` because its endpoints ran on a worker
/// thread. The Rust service uses a task-local scoped around the request future ([`with_caller`]),
/// because a task may move between threads at an await point and the scope follows the future,
/// not the thread. An explicit caller ([`StoreUsageLedger::recorder_for`]) still wins, which is
/// the path the API uses.
pub fn normalise_caller(value: Option<&str>) -> String {
    let text: String = value.unwrap_or("").trim().chars().take(128).collect();
    if text.is_empty() {
        UNKNOWN_CALLER.to_string()
    } else {
        text
    }
}

/// The caller currently in scope, or `unknown` outside a [`with_caller`] scope.
pub fn current_caller() -> String {
    REQUEST_CALLER
        .try_with(Clone::clone)
        .unwrap_or_else(|_| UNKNOWN_CALLER.to_string())
}

/// Run a future with the caller that vendor spend inside it is attributed to.
///
/// The scope follows the future rather than the thread, so an endpoint can set the
/// `X-MarketData-Caller` value once and every provider call below it — across await points
/// and worker threads — sees the same attribution.
pub async fn with_caller<F>(caller: Option<&str>, future: F) -> F::Output
where
    F: std::future::Future,
{
    REQUEST_CALLER.scope(normalise_caller(caller), future).await
}

/// The tariff-backed ledger: prices one call, records it, and never fails the call.
///
/// The tariff comes from `dbo.RequestTypes`, so a re-measured price is a row update rather
/// than a release. An unpriced call is recorded at cost 0 so the spend surface stays visible
/// — it is not free, it is unpriced — and a ledger write failure is logged and swallowed:
/// serving the price is why the caller is here.
pub struct StoreUsageLedger {
    repository: Arc<dyn UsageRepository>,
    tariff: Tariff,
    unpriced_warned: Mutex<BTreeSet<String>>,
}

impl StoreUsageLedger {
    pub fn new(repository: Arc<dyn UsageRepository>, tariff: Tariff) -> Self {
        Self {
            repository,
            tariff,
            unpriced_warned: Mutex::new(BTreeSet::new()),
        }
    }

    pub fn tariff(&self) -> &Tariff {
        &self.tariff
    }

    /// Price and record one call under an explicit caller, or the caller in scope.
    pub async fn record_as(&self, request_type: &str, symbols: i64, caller: Option<&str>) -> i64 {
        let cost = match self.tariff.price(request_type, symbols) {
            Some(cost) => cost,
            None => {
                self.warn_once_unpriced(request_type);
                0
            }
        };
        let caller = caller
            .filter(|caller| !caller.is_empty())
            .map(str::to_string)
            .unwrap_or_else(current_caller);
        if let Err(error) = self
            .repository
            .record_usage(request_type, &caller, 1, symbols, cost, None)
            .await
        {
            tracing::warn!(
                "usage not recorded for {request_type} ({error}); the call itself is unaffected"
            );
        }
        cost
    }

    /// A recorder bound to one caller, for one request.
    ///
    /// Explicit rather than relying on the task-local alone: the API has the header in hand
    /// and binding it to the recorder it installs leaves nothing to inherit accidentally.
    pub fn recorder_for(self: &Arc<Self>, caller: Option<&str>) -> Arc<dyn UsageRecorder> {
        Arc::new(BoundRecorder {
            ledger: Arc::clone(self),
            caller: caller.map(str::to_string),
        })
    }

    fn warn_once_unpriced(&self, request_type: &str) {
        let mut warned = self
            .unpriced_warned
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if warned.insert(request_type.to_string()) {
            tracing::warn!(
                "{request_type} has no priced row in dbo.RequestTypes; recording the call \
                 with cost 0 so the surface stays visible. It is not free — it is unpriced."
            );
        }
    }
}

#[async_trait]
impl UsageRecorder for StoreUsageLedger {
    async fn record(&self, request_type: &str, symbols: i64) -> Option<i64> {
        Some(self.record_as(request_type, symbols, None).await)
    }
}

/// One request's ledger, bound to the caller that made it.
struct BoundRecorder {
    ledger: Arc<StoreUsageLedger>,
    caller: Option<String>,
}

#[async_trait]
impl UsageRecorder for BoundRecorder {
    async fn record(&self, request_type: &str, symbols: i64) -> Option<i64> {
        Some(
            self.ledger
                .record_as(request_type, symbols, self.caller.as_deref())
                .await,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<TariffRow> {
        vec![
            TariffRow {
                request_type: "/real-time/".into(),
                cost_basis: Some("per_symbol".into()),
                cost_each: Some(1),
            },
            TariffRow {
                request_type: "/real-time?ex=US".into(),
                cost_basis: Some("flat".into()),
                cost_each: Some(100),
            },
            TariffRow {
                request_type: "/eod/".into(),
                cost_basis: Some("per_call".into()),
                cost_each: Some(1),
            },
            TariffRow {
                request_type: "/intraday/".into(),
                cost_basis: Some("per_call".into()),
                cost_each: Some(5),
            },
            TariffRow {
                request_type: "/eod-bulk-last-day/US".into(),
                cost_basis: Some("flat".into()),
                cost_each: Some(100),
            },
            TariffRow {
                request_type: "/eod-bulk-last-day/AU".into(),
                cost_basis: Some("flat".into()),
                cost_each: Some(100),
            },
            TariffRow {
                request_type: "/screener".into(),
                cost_basis: Some("flat".into()),
                cost_each: Some(5),
            },
            TariffRow {
                request_type: "/user".into(),
                cost_basis: Some("  FREE ".into()),
                cost_each: Some(0),
            },
            TariffRow {
                request_type: "unknown".into(),
                cost_basis: Some("per_call".into()),
                cost_each: None,
            },
        ]
    }

    fn tariff() -> Tariff {
        Tariff::from_rows(rows())
    }

    #[test]
    fn per_symbol_multiplies_by_the_batch() {
        assert_eq!(tariff().price("/real-time/", 1), Some(1));
        assert_eq!(tariff().price("/real-time/", 3), Some(3));
    }

    #[test]
    fn per_symbol_treats_zero_symbols_as_one() {
        assert_eq!(tariff().price("/real-time/", 0), Some(1));
        assert_eq!(tariff().price("/real-time/", -4), Some(1));
    }

    #[test]
    fn flat_ignores_the_batch() {
        assert_eq!(tariff().price("/real-time?ex=US", 0), Some(100));
        assert_eq!(tariff().price("/real-time?ex=US", 5000), Some(100));
    }

    #[test]
    fn each_market_has_its_own_flat_bulk_price() {
        assert_eq!(tariff().price("/eod-bulk-last-day/AU", 0), Some(100));
        assert_eq!(tariff().price("/eod-bulk-last-day/AU", 5000), Some(100));
    }

    #[test]
    fn per_call_ignores_the_batch() {
        assert_eq!(tariff().price("/eod/", 0), Some(1));
        assert_eq!(tariff().price("/eod/", 40), Some(1));
        assert_eq!(tariff().price("/intraday/", 40), Some(5));
    }

    #[test]
    fn the_screener_purpose_prices_at_five_flat() {
        assert_eq!(request_type_for("screener"), Some("/screener"));
        let tariff = tariff();
        assert_eq!(tariff.price("/screener", 0), Some(5));
        assert_eq!(tariff.price("/screener", 1), Some(5));
        // A profile fan-out bills one call per uncached symbol, so the units scale with calls,
        // not with the batch a single call happens to carry.
        assert_eq!(tariff.price("/screener", 25), Some(5));
    }

    #[test]
    fn free_is_zero_not_none() {
        assert_eq!(tariff().price("/user", 3), Some(0));
    }

    #[test]
    fn an_unpriced_row_returns_none() {
        assert_eq!(tariff().price("unknown", 1), None);
    }

    #[test]
    fn an_unlisted_type_returns_none() {
        assert_eq!(tariff().price("/not-a-real-endpoint", 1), None);
    }

    #[test]
    fn a_negative_or_odd_cost_each_is_just_multiplied() {
        assert_eq!(
            TariffEntry::new("x", "per_symbol", Some(2)).price(4),
            Some(8)
        );
    }

    #[test]
    fn every_internal_purpose_maps_to_a_tariff_row() {
        let tariff = tariff();
        for (purpose, request_type) in PURPOSE_REQUEST_TYPE {
            assert!(
                tariff.entry(request_type).is_some(),
                "{purpose} -> {request_type} is not priced"
            );
        }
    }

    #[test]
    fn a_purpose_that_is_not_known_has_no_request_type() {
        assert_eq!(request_type_for("real_time"), Some("/real-time/"));
        assert_eq!(request_type_for("nonsense"), None);
    }

    #[test]
    fn the_listing_is_sorted_and_complete() {
        let tariff = tariff();
        let listing = tariff.listing();
        assert_eq!(listing.len(), rows().len());
        let names: Vec<&str> = listing
            .iter()
            .map(|entry| entry.request_type.as_str())
            .collect();
        let mut expected: Vec<String> = rows().into_iter().map(|row| row.request_type).collect();
        expected.sort();
        assert_eq!(
            names,
            expected.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_missing_basis_is_empty_and_basis_text_is_normalised() {
        let catalogue = Tariff::from_rows([TariffRow {
            request_type: "/x".into(),
            cost_basis: None,
            cost_each: Some(2),
        }]);
        assert_eq!(catalogue.entry("/x").unwrap().cost_basis, "");
        assert_eq!(tariff().entry("/user").unwrap().cost_basis, "free");
    }

    #[test]
    fn a_caller_is_trimmed_truncated_and_never_blank() {
        assert_eq!(normalise_caller(None), "unknown");
        assert_eq!(normalise_caller(Some("   ")), "unknown");
        assert_eq!(normalise_caller(Some("  mctrader ")), "mctrader");
        let long = "x".repeat(200);
        assert_eq!(normalise_caller(Some(&long)).chars().count(), 128);
    }
}
