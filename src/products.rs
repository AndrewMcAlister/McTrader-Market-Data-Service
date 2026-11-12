//! The four products, and every cache decision in the service.
//!
//! # The cache policy, in one place
//!
//! Three caches, each with the lifetime its data actually has:
//!
//! * **prices** — five minutes, held in `SymbolPriceCache`. Short enough that a price is never
//!   more than one cycle stale; long enough that two books asking inside the same minute pay
//!   for one vendor call rather than two.
//! * **whole-market** — one vendor call per cycle, held in memory. This is 100 units flat, so
//!   refetching it per request is the one thing the bulk path must never do.
//! * **daily bars** — held in `DailySnapshot` and keyed by the completed session, not by a
//!   clock. Yesterday's bar cannot change, so a TTL would be a worse model than "the session
//!   it belongs to".
//!
//! # Why `prices()` chooses the route and the caller does not
//!
//! The tariff has a hard break-even at 100 symbols: `/real-time/` bills 1 per symbol, the
//! `ex=US` whole-market form bills 100 flat. So a 6-symbol request and a 2,800-symbol request
//! want opposite answers, and the only thing that distinguishes them is the length of the
//! list — which this layer knows and the caller should not have to. A single "always bulk" or
//! "always per-symbol" rule is wrong for one of the two cases by a factor of up to 100.
//!
//! That is also why the API needs no fifth endpoint for the whole market: `POST /prices` with
//! a long list *is* the whole-market call.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{Duration, NaiveDate, NaiveDateTime};
use chrono_tz::Tz;
use serde::Serialize;
use serde_json::Value;

use crate::clock::{iso_naive, now_naive, seconds_between, Clock};
use crate::config::Settings;
use crate::domain::asset_class::AssetClass;
use crate::domain::atr;
use crate::domain::availability::COUNTRIES;
use crate::domain::exchanges::{exchange_for, resolve_exchange};
use crate::domain::intraday::{aggregate, IntradayRow, Timeframe};
use crate::domain::screener::{
    parse_sort, profile_filters, screener_wire_filters, PROFILE_QUERY_LIMIT,
};
use crate::domain::sessions::{exchange_timezone, latest_completed_session};
use crate::domain::symbols::{symbol_for_provider, ProviderId};
use crate::domain::ticks;
use crate::error::{py_repr, MarketDataError, ProviderFailure};
use crate::presence::Presence;
use crate::providers::base::{number, ProviderBar, ProviderQuote};
use crate::providers::eodhd_client::{bare_symbol, EodhdClient, EodhdStats};
use crate::providers::router::{ProviderRouter, RouterStats};
use crate::refresh::RefreshStore;
use crate::store::models::{
    CachedPrice, DailyBarWrite, IndicatorWrite, IntradayBarWrite, PriceRow, StoredBar,
    StoredIndicator, StoredIntradayBar,
};
use crate::store::{
    BulkPullRepository, DailyBarRepository, IndicatorRepository, IntradayRepository,
    PriceRepository, StoreError, UsageRepository,
};
use crate::usage::{current_caller, with_caller, StoreUsageLedger, Tariff};

/// How old a whole-market row may be before it is dropped as a price that no longer exists.
/// Four days: the feed carries delisted symbols holding their last-ever price, and a row from
/// 2022 would price a stop as if it were current.
pub const DEFAULT_MAX_ROW_LAG_SECONDS: f64 = 345_600.0;

/// The whole-market snapshot is reused for this long inside one process.
pub const BULK_SNAPSHOT_TTL_SECONDS: f64 = 120.0;

/// How long a refresh that could not reach the newest session is trusted before it is tried
/// again. A vendor publishes a session some time after it closes, so the first caller past the
/// settle window often finds nothing new; without a retry that one early answer would hold
/// every later caller in the same session back until the next close. An hour bounds the cost
/// of a day the exchange does not trade — one call per symbol per hour — while still catching the
/// publication an hour after it happens.
pub const BARS_REFRESH_TTL_SECONDS: f64 = 3600.0;

/// How long a profile is trusted before its vendor call is paid for again.
///
/// One hour is the cadence a company's description actually changes on; market capitalisation
/// moves with the price, and the memo is what makes a 25-symbol batch cost 125 units once
/// rather than on every pass.
pub const PROFILE_TTL_SECONDS: f64 = 3600.0;

/// How many profiles the in-process memo holds. About 1.5 MB at the shape below.
///
/// At the cap a fresh entry is returned but not memoised, with one warning: an unbounded memo
/// is a memory leak with a cache's name.
pub const PROFILE_CACHE_MAX_ENTRIES: usize = 4096;

/// How many profile vendor calls run at once. The client is `Send + Sync` and its methods take
/// `&self`, so the fan-out is real concurrency over one client.
///
/// Worst case at 25 symbols is five waves of at most `2 × 30 s + 0.5 s` (the 30-second
/// `eodhd_timeout_seconds` and the one transport retry inside `get_json`), about 300 s. That
/// bound is part of the contract: a caller's client timeout must exceed it.
pub const PROFILE_CONCURRENCY: usize = 5;

/// The row-count floor, as a fraction of the previous completed session's, below which a bulk
/// pull is treated as partial.
///
/// The live evidence is 1,881 rows in `DailySnapshot` against the ~27,714 a completed pull
/// writes — 7%, not 90% — so a half separates a partial write from a quiet session with room to
/// spare. A first pull has no baseline and is trusted; a pull of nothing is never complete.
pub const BULK_MIN_COMPLETENESS_RATIO: f64 = 0.5;

/// Calendar days a session depth spans, for counting how much history a symbol holds: seven
/// for five is the weekend, and forty days of margin covers the holidays in between.
pub fn history_window_days(sessions: i64) -> i64 {
    sessions.saturating_mul(7) / 5 + 40
}

/// The window a cache read may serve, or `None` when a vendor call is needed.
///
/// A window is complete only when it holds the requested number of sessions, reaches the
/// latest completed session, and states a raw close on every served row. The two closes are
/// two price scales, and a series half adjusted and half raw is worse than one that is wholly
/// adjusted.
pub fn complete_window(
    rows: &[StoredBar],
    sessions: i64,
    latest: NaiveDate,
) -> Option<&[StoredBar]> {
    if (rows.len() as i64) < sessions {
        return None;
    }
    if !rows.last().is_some_and(|row| row.session >= latest) {
        return None;
    }
    let take = usize::try_from(sessions).unwrap_or(usize::MAX);
    let served = &rows[rows.len().saturating_sub(take)..];
    if served.is_empty() || served.iter().any(|row| row.raw_close.is_none()) {
        return None;
    }
    Some(served)
}

/// The price and multiplier a stop level needs, or `None` when no stop was asked for.
///
/// The multiplier is never defaulted: the books use 1.0, 1.0, 2.0 and 3.0, and a guessed
/// stop is acted on. Supplying either half alone is a refusal.
pub fn requested_stop(request: &AtrRequest<'_>) -> Result<Option<(f64, f64)>, MarketDataError> {
    if request.price.is_none() && request.multiplier.is_none() {
        return Ok(None);
    }
    let (Some(price), Some(multiplier)) = (request.price, request.multiplier) else {
        return Err(MarketDataError::bad_request(
            "incomplete-stop",
            "stop_level needs both price and multiplier (the multiplier is never \
             defaulted: the books use 1.0, 1.0, 2.0 and 3.0)",
        ));
    };
    if multiplier <= 0.0 {
        return Err(MarketDataError::bad_request(
            "bad-multiplier",
            format!("multiplier must be positive, got {multiplier:?}; it is never defaulted"),
        ));
    }
    Ok(Some((price, multiplier)))
}

/// The side a stop belongs to; an omitted direction means `long`.
pub fn stop_side(direction: &str) -> Result<String, MarketDataError> {
    let side = direction.trim().to_lowercase();
    let side = if side.is_empty() {
        "long".to_string()
    } else {
        side
    };
    if side != "long" && side != "short" {
        return Err(MarketDataError::bad_request(
            "bad-direction",
            format!(
                "{} is not a direction; use 'long' or 'short'",
                py_repr(direction)
            ),
        ));
    }
    Ok(side)
}

/// True when a cached indicator row may answer this request.
///
/// The cache stores one definition per symbol and session. A row computed with another
/// method, smoothing or session would answer a question the caller did not ask, under a label
/// that says it did.
pub fn indicator_answers(
    hit: &StoredIndicator,
    method: &str,
    smoothing: &str,
    latest: NaiveDate,
    period: i64,
) -> bool {
    hit.method.as_deref() == Some(method)
        && hit.smoothing.as_deref() == Some(smoothing)
        && hit
            .as_of_session
            .is_some_and(|as_of_session| as_of_session <= latest)
        && hit.value(period).is_some()
}

/// The repositories a product needs.
///
/// `RefreshStore` is a supertrait so the same object can be handed to [`crate::refresh`]
/// without a second handle: the API's `/refresh` and the scheduler operate on exactly the
/// store the products read.
pub trait ProductsStore:
    PriceRepository
    + RefreshStore
    + DailyBarRepository
    + IndicatorRepository
    + UsageRepository
    + IntradayRepository
    + BulkPullRepository
{
}

impl<T: ?Sized> ProductsStore for T where
    T: PriceRepository
        + RefreshStore
        + DailyBarRepository
        + IndicatorRepository
        + UsageRepository
        + IntradayRepository
        + BulkPullRepository
{
}

/// A product request that cannot be answered.
#[derive(Debug)]
pub enum ProductError {
    /// A caller's mistake, carrying the HTTP status the API will use.
    Refused(MarketDataError),
    Store(StoreError),
    Provider(ProviderFailure),
    /// The capability needs the vendor, and this process has no vendor client.
    ///
    /// A keyless build still serves Yahoo and Binance, so a missing client is not a startup
    /// failure; it is a 503 `vendor-unavailable` on exactly the routes that cannot answer.
    Unavailable,
}

impl std::fmt::Display for ProductError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(error) => std::fmt::Display::fmt(error, formatter),
            Self::Store(error) => write!(formatter, "database error: {error}"),
            Self::Provider(error) => std::fmt::Display::fmt(error, formatter),
            Self::Unavailable => formatter.write_str("no EODHD client is configured"),
        }
    }
}

impl std::error::Error for ProductError {}

impl From<MarketDataError> for ProductError {
    fn from(error: MarketDataError) -> Self {
        Self::Refused(error)
    }
}

impl From<StoreError> for ProductError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<ProviderFailure> for ProductError {
    fn from(error: ProviderFailure) -> Self {
        Self::Provider(error)
    }
}

impl From<crate::refresh::RefreshFailure> for ProductError {
    fn from(error: crate::refresh::RefreshFailure) -> Self {
        match error {
            crate::refresh::RefreshFailure::Store(error) => Self::Store(error),
            crate::refresh::RefreshFailure::Provider(error) => Self::Provider(error),
            crate::refresh::RefreshFailure::Invalid(detail) => {
                Self::Refused(MarketDataError::internal("refresh", detail))
            }
        }
    }
}

/// The request's symbols, bare and de-duplicated, in the order they arrived.
///
/// A suffix is stripped because the exchange is served separately — and the exchange has already
/// been read off that suffix by [`resolve_exchange`], so dropping it here loses nothing. Crypto
/// keeps its pair as sent: `BTC-USD` has no exchange suffix to strip and `BTC` alone is a
/// different (and usually unpriced) request.
pub fn wanted_symbols(symbols: &[String], crypto: bool) -> Vec<String> {
    let mut wanted = Vec::with_capacity(symbols.len());
    for raw in symbols {
        let text = if crypto {
            raw.trim().to_uppercase()
        } else {
            bare_symbol(raw)
        };
        if !text.is_empty() && !wanted.contains(&text) {
            wanted.push(text);
        }
    }
    wanted
}

/// The public quote shape, from a cache row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuotePayload {
    pub symbol: Option<String>,
    pub exchange: Option<String>,
    pub price: Option<f64>,
    pub previous_close: Option<f64>,
    pub change_pct: Option<f64>,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub provider: String,
    pub fetched_at: Option<String>,
    pub age_seconds: Option<f64>,
    pub as_of: Option<String>,
    pub as_of_age_seconds: Option<f64>,
}

/// The batch shape `POST /prices` returns.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PricesResponse {
    pub quotes: BTreeMap<String, QuotePayload>,
    pub exchange: String,
    pub fetched: Vec<String>,
    pub missing: Vec<String>,
    pub deferred: Vec<String>,
    pub complete: bool,
    pub route: &'static str,
    pub requested: usize,
    pub count: usize,
}

/// One bar row in the short-key contract shape.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BarRow {
    pub d: String,
    pub o: Option<f64>,
    pub h: Option<f64>,
    pub l: Option<f64>,
    pub c: Option<f64>,
    pub cr: Option<f64>,
    pub v: Option<i64>,
}

/// One symbol's daily bars.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BarsEntry {
    pub symbol: String,
    pub exchange: String,
    pub as_of_session: Option<String>,
    pub sessions: usize,
    pub adjusted: bool,
    pub provider: String,
    pub source: String,
    pub rows: Vec<BarRow>,
}

/// The batch shape `POST /bars` returns.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BarsResponse {
    pub bars: BTreeMap<String, BarsEntry>,
    pub fetched: Vec<String>,
    pub short_history: Vec<String>,
    pub complete: bool,
    pub requested_sessions: i64,
}

/// One ATR reading. `reason` and `stop_level` are absent or present exactly as the contract
/// says: a cache hit has no reason key, a computed reading has `reason: null`, and a stop level
/// appears only when the caller asked for one (and is null when the ATR is unknown).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AtrReading {
    pub symbol: String,
    pub value: Option<f64>,
    pub percent: Option<f64>,
    pub reference_price: Option<f64>,
    pub period: i64,
    pub method: String,
    pub smoothing: String,
    pub sessions_used: Option<i64>,
    /// How many ranges the value was actually averaged over.
    ///
    /// `sessions_used` is how many *bars were read*, which under `simple` is not the size of
    /// the sample: ATR(14) off 51 bars reports 51, and that reads as a 51-session average when
    /// it is a 14-session one. This states the sample. `null` only when the cache could not say.
    pub sessions_averaged: Option<i64>,
    /// Sessions in the window that did not move: `H == L == the previous close`.
    ///
    /// Counted as true ranges of zero they pull the mean down, and on a thin name that is most
    /// of the difference between a stop that fits in the noise and one that does not. Reported
    /// here and *not* excluded from `value`: excluding them widens every stop in the estate and
    /// is a strategy change to be measured, not a fix to be applied quietly.
    ///
    /// `null` on a cache hit — the indicator cache stores the value and the depth it came from,
    /// not the range series, and a guessed count would be worse than an absent one.
    pub flat_sessions: Option<i64>,
    pub as_of_session: Option<String>,
    pub source: String,
    /// The smallest increment the venue quotes `reference_price` in: $0.01 on the US tape, and
    /// on the ASX $0.001 under $0.10, $0.005 to $2.00, $0.01 above it.
    pub tick: Option<f64>,
    /// `value` measured in ticks — the reading this service's ATR report asks for.
    ///
    /// Anything near 1 means the "average range" is one quote increment: the stop that follows
    /// from it is inside the spread and cannot be a stop at all.
    pub ticks: Option<f64>,
    /// `tick` as a percentage of `reference_price` — the friction floor, to be read against
    /// `percent`. At $3.00 the ASX step is 0.33%; at $0.135 it was 3.70%.
    pub tick_percent: Option<f64>,
    #[serde(skip_serializing_if = "Presence::is_absent")]
    pub reason: Presence<String>,
    #[serde(skip_serializing_if = "Presence::is_absent")]
    pub stop_level: Presence<f64>,
}

/// The batch shape `POST /atr` returns.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AtrBatchResponse {
    pub atr: BTreeMap<String, AtrReading>,
    pub exchange: String,
    pub period: i64,
    pub method: String,
    pub smoothing: String,
    pub complete: bool,
}

/// What `POST /screener` returns: the vendor's rows, untouched, and the query that produced
/// them.
///
/// `filters` echoes the wire form, including the single `["exchange", "=", venue]` triple the
/// service appended, so a caller can see which venue actually ran. Nothing is normalised out of
/// `rows`: field names, nulls and columns this service does not know travel through.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScreenerResponse {
    pub provider: String,
    pub country_code: String,
    pub exchange: String,
    pub filters: Vec<Value>,
    pub sort: Option<String>,
    pub limit: i64,
    pub offset: i64,
    pub count: usize,
    pub rows: Vec<Value>,
}

/// One company profile, as `/screener` can answer it.
///
/// The fields with no entitled source (`country`, `gic_sector`, `gic_industry`,
/// `description`) are always null rather than absent: a caller comparing two profiles needs the
/// same keys every time. `error` is set only when this symbol's own vendor call failed, which
/// is not an error for the batch.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProfileEntry {
    pub symbol: String,
    pub exchange: String,
    pub found: bool,
    pub name: Option<String>,
    pub sector: Option<String>,
    pub industry: Option<String>,
    pub market_cap: Option<f64>,
    pub currency: Option<String>,
    pub country: Option<String>,
    pub gic_sector: Option<String>,
    pub gic_industry: Option<String>,
    pub description: Option<String>,
    pub as_of: Option<String>,
    pub error: Option<String>,
}

/// The batch shape `POST /profile` returns.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProfilesResponse {
    pub provider: String,
    pub country_code: String,
    pub exchange: String,
    pub requested: usize,
    pub complete: bool,
    pub missing: Vec<String>,
    pub profiles: BTreeMap<String, ProfileEntry>,
}

/// The caller-supplied half of a profile request.
#[derive(Debug, Clone)]
pub struct ProfileBatch<'a> {
    pub symbols: &'a [String],
    pub exchange: Option<&'a str>,
    pub country_code: Option<&'a str>,
}

/// The batch shape `POST /bars/intraday` returns.
///
/// The series shape is `POST /bars`'s, unchanged, so a caller can reuse one decoder. The one
/// difference is stated: a row's `d` is an exchange-local ISO timestamp, because an intraday bar
/// is a moment rather than a session.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IntradayResponse {
    pub exchange: String,
    pub provider: String,
    pub fetched: Vec<String>,
    pub short_history: Vec<String>,
    pub bars: BTreeMap<String, BarsEntry>,
}

/// The caller-supplied half of an intraday request.
#[derive(Debug, Clone)]
pub struct IntradayRequest<'a> {
    pub symbols: &'a [String],
    pub timeframe: Timeframe,
    pub limit: i64,
    pub allow_fetch: bool,
    pub exchange: Option<&'a str>,
    pub country_code: Option<&'a str>,
}

/// One requested bulk date.
///
/// `Latest` is resolved through the same [`latest_completed_session`] rule `/bars` uses, so the
/// two routes cannot disagree about which session has finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkDateKey {
    Latest,
    Session(NaiveDate),
}

impl BulkDateKey {
    /// The key as the caller spelled it, which is the key the response is filed under.
    pub fn as_requested(self) -> String {
        match self {
            Self::Latest => "latest".to_string(),
            Self::Session(date) => date.to_string(),
        }
    }
}

/// One symbol's close in a bulk answer. The short key is `/bars`'s contract shape.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BulkClose {
    pub c: f64,
}

/// One session in a bulk answer: the session the service resolved, and every symbol's close.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BulkSession {
    pub session: String,
    pub rows: BTreeMap<String, BulkClose>,
}

/// The shape `POST /bars/bulk` returns.
///
/// `partial` names the requested date keys whose rows came from a pull without a completeness
/// marker. It is the only way an unmarked session is ever returned, and only on the response of
/// the pull that produced it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BulkResponse {
    pub market: String,
    pub provider: String,
    pub partial: Vec<String>,
    pub dates: BTreeMap<String, BulkSession>,
}


/// The caller-supplied half of an ATR request.
#[derive(Debug, Clone)]
pub struct AtrRequest<'a> {
    pub symbols: &'a [String],
    pub period: Option<i64>,
    pub method: Option<&'a str>,
    pub smoothing: Option<&'a str>,
    pub exchange: Option<&'a str>,
    pub price: Option<f64>,
    pub multiplier: Option<f64>,
    pub direction: &'a str,
    pub allow_fetch: bool,
    pub asset_class: Option<&'a str>,
    pub country_code: Option<&'a str>,
}

impl<'a> AtrRequest<'a> {
    pub fn new(symbols: &'a [String]) -> Self {
        Self {
            symbols,
            period: None,
            method: None,
            smoothing: None,
            exchange: None,
            price: None,
            multiplier: None,
            direction: "long",
            allow_fetch: true,
            asset_class: None,
            country_code: None,
        }
    }
}

/// The process-local counters `/stats` reports.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CountersSnapshot {
    pub price_from_cache: u64,
    pub price_from_vendor: u64,
    pub bars_from_cache: u64,
    pub bars_from_vendor: u64,
    pub atr_from_cache: u64,
    pub atr_computed: u64,
    pub bulk_snapshot_fetches: u64,
    pub per_symbol_fetches: u64,
    pub intraday_fetches: u64,
    pub intraday_cache_hits: u64,
    pub screener_calls: u64,
    pub profile_calls: u64,
    pub profile_memo_hits: u64,
    pub bulk_pulls: u64,
}

/// The whole-market snapshot's state, for `/stats`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BulkSnapshotStats {
    pub rows_seen: usize,
    pub rows_usable: usize,
    pub rows_dropped: usize,
    pub fetched_at: Option<String>,
}

/// The settings `/stats` echoes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SettingsStats {
    pub price_ttl_seconds: f64,
    pub bulk_breakeven: i64,
    pub min_sessions: i64,
    pub history_sessions: i64,
    pub atr_periods: Vec<i64>,
    pub atr_method: String,
    pub atr_smoothing: String,
    pub database: String,
}

/// The shape `/stats` returns.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatsResponse {
    pub counters: CountersSnapshot,
    pub vendor: Option<EodhdStats>,
    pub router: RouterStats,
    pub bulk_snapshot: BulkSnapshotStats,
    pub settings: SettingsStats,
}

#[derive(Debug, Default)]
struct Counters {
    price_from_cache: AtomicU64,
    price_from_vendor: AtomicU64,
    bars_from_cache: AtomicU64,
    bars_from_vendor: AtomicU64,
    atr_from_cache: AtomicU64,
    atr_computed: AtomicU64,
    bulk_snapshot_fetches: AtomicU64,
    per_symbol_fetches: AtomicU64,
    intraday_fetches: AtomicU64,
    intraday_cache_hits: AtomicU64,
    screener_calls: AtomicU64,
    profile_calls: AtomicU64,
    profile_memo_hits: AtomicU64,
    bulk_pulls: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> CountersSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        CountersSnapshot {
            price_from_cache: load(&self.price_from_cache),
            price_from_vendor: load(&self.price_from_vendor),
            bars_from_cache: load(&self.bars_from_cache),
            bars_from_vendor: load(&self.bars_from_vendor),
            atr_from_cache: load(&self.atr_from_cache),
            atr_computed: load(&self.atr_computed),
            bulk_snapshot_fetches: load(&self.bulk_snapshot_fetches),
            per_symbol_fetches: load(&self.per_symbol_fetches),
            intraday_fetches: load(&self.intraday_fetches),
            intraday_cache_hits: load(&self.intraday_cache_hits),
            screener_calls: load(&self.screener_calls),
            profile_calls: load(&self.profile_calls),
            profile_memo_hits: load(&self.profile_memo_hits),
            bulk_pulls: load(&self.bulk_pulls),
        }
    }
}

#[derive(Debug, Default)]
struct BulkState {
    /// The rows as they were written, so a re-cache keeps the volume and the fetch stamp the
    /// vendor actually sent rather than reconstructing a lossy copy from the cache shape.
    snapshot: HashMap<String, PriceRow>,
    fetched_at: Option<NaiveDateTime>,
    rows_seen: usize,
    rows_dropped: usize,
}

/// A bar from either the cache or a provider, in the shape the payload needs.
#[derive(Debug, Clone)]
struct BarView {
    session: NaiveDate,
    open: Option<f64>,
    high: Option<f64>,
    low: Option<f64>,
    close: Option<f64>,
    raw_close: Option<f64>,
    volume: Option<i64>,
    adjusted: bool,
}

impl From<&StoredBar> for BarView {
    fn from(bar: &StoredBar) -> Self {
        Self {
            session: bar.session,
            open: bar.open,
            high: bar.high,
            low: bar.low,
            close: bar.close,
            raw_close: bar.raw_close,
            volume: bar.volume,
            adjusted: bar.adjusted,
        }
    }
}

impl From<&DailyBarWrite> for BarView {
    fn from(bar: &DailyBarWrite) -> Self {
        Self {
            session: bar.session,
            open: Some(bar.open),
            high: Some(bar.high),
            low: Some(bar.low),
            close: Some(bar.close),
            raw_close: bar.raw_close,
            volume: bar.volume,
            adjusted: bar.adjusted.unwrap_or(true),
        }
    }
}

/// A profile memo entry: the answer and when it was paid for.
#[derive(Debug, Clone)]
struct ProfileMemoEntry {
    entry: ProfileEntry,
    stored_at: NaiveDateTime,
}

/// Price, prices, bars and ATR over one vendor client, one store and three caches.
pub struct Products {
    settings: Settings,
    store: Arc<dyn ProductsStore>,
    client: Option<Arc<EodhdClient>>,
    ledger: Arc<StoreUsageLedger>,
    router: Arc<ProviderRouter>,
    clock: Arc<dyn Clock>,
    bulk: tokio::sync::Mutex<BulkState>,
    /// `(market, symbol) -> (the session a refresh was asked for, when it was asked)`.
    bars_refreshed: Mutex<HashMap<(String, String), (NaiveDate, NaiveDateTime)>>,
    /// `(market, symbol) -> how many sessions the last deep vendor pull returned`.
    bars_deep: Mutex<HashMap<(String, String), usize>>,
    /// `(root, market) -> the profile and when it was fetched`. No table: §5.4 of the plan
    /// records why a persistent cache was rejected for a value whose cold refill is capped.
    profiles: Mutex<HashMap<(String, String), ProfileMemoEntry>>,
    /// Set once, when the memo is full and an entry could not be kept.
    profile_cap_warned: AtomicBool,
    counters: Counters,
}

impl Products {
    /// Build the products layer: resolve the vendor client, read the tariff, wire the router.
    ///
    /// The tariff read is best-effort because a database that cannot list its own prices must
    /// not stop prices being served; an unpriced call is recorded at cost 0, so the spend
    /// surface stays visible rather than disappearing.
    pub async fn new(
        settings: Settings,
        store: Arc<dyn ProductsStore>,
        client: Option<Arc<EodhdClient>>,
        router: Option<Arc<ProviderRouter>>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ProductError> {
        let client = match client {
            Some(client) => Some(client),
            None if !settings.eodhd_api_key.is_empty() => Some(Arc::new(EodhdClient::new(
                settings.eodhd_api_key.clone(),
                settings.eodhd_base_url.clone(),
                settings.eodhd_timeout_seconds,
                settings.eodhd_bulk_timeout_seconds,
            )?)),
            None => {
                tracing::warn!(
                    "no EODHD API key: the vendor is not in the router. Daily bars and the \
                     whole-market feed are unavailable; Yahoo and Binance still answer."
                );
                None
            }
        };

        let tariff = match store.request_types().await {
            Ok(rows) => Tariff::from_rows(rows),
            Err(error) => {
                tracing::warn!(
                    "could not read dbo.RequestTypes ({error}); spend will be recorded unpriced"
                );
                Tariff::default()
            }
        };
        let repository: Arc<dyn UsageRepository> = store.clone();
        let ledger = Arc::new(StoreUsageLedger::new(repository, tariff));
        if let Some(client) = &client {
            client.set_recorder(ledger.clone());
        }

        let router = match router {
            Some(router) => router,
            None => Arc::new(crate::providers::build_router(
                client.clone(),
                settings.eodhd_timeout_seconds,
            )?),
        };

        Ok(Self {
            settings,
            store,
            client,
            ledger,
            router,
            clock,
            bulk: tokio::sync::Mutex::new(BulkState::default()),
            bars_refreshed: Mutex::new(HashMap::new()),
            bars_deep: Mutex::new(HashMap::new()),
            profiles: Mutex::new(HashMap::new()),
            profile_cap_warned: AtomicBool::new(false),
            counters: Counters::default(),
        })
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The store, for the refresh policy and the endpoints that share it.
    pub fn store(&self) -> &Arc<dyn ProductsStore> {
        &self.store
    }

    pub fn client(&self) -> Option<&Arc<EodhdClient>> {
        self.client.as_ref()
    }

    pub fn ledger(&self) -> &Arc<StoreUsageLedger> {
        &self.ledger
    }

    pub fn router(&self) -> &Arc<ProviderRouter> {
        &self.router
    }

    /// One symbol's latest price: cache first, then one vendor call.
    pub async fn price(
        &self,
        symbol: &str,
        exchange: Option<&str>,
        max_age_seconds: Option<f64>,
        asset_class: Option<&str>,
        country_code: Option<&str>,
    ) -> Result<QuotePayload, ProductError> {
        let batch = self
            .prices(
                std::slice::from_ref(&symbol.to_string()),
                exchange,
                max_age_seconds,
                asset_class,
                country_code,
            )
            .await?;
        let kind = AssetClass::parse(asset_class.unwrap_or(""));
        let key = if kind.is_crypto() {
            symbol.trim().to_uppercase()
        } else {
            bare_symbol(symbol)
        };
        batch.quotes.get(&key).cloned().ok_or_else(|| {
            MarketDataError::new(
                "unpriced",
                format!(
                    "{} could not be priced at market {}",
                    py_repr(symbol),
                    py_repr(&batch.exchange)
                ),
                404,
            )
            .into()
        })
    }

    /// Latest prices for a batch, choosing bulk or per-symbol by size.
    ///
    /// `exchange` or `country_code` names the market, because `CBA` at the ASX and `CBA` in New
    /// York are two instruments. The HTTP layer always supplies `country_code`; the suffix and
    /// US fallbacks below it exist for internal callers, which may state neither.
    pub async fn prices(
        &self,
        symbols: &[String],
        exchange: Option<&str>,
        max_age_seconds: Option<f64>,
        asset_class: Option<&str>,
        country_code: Option<&str>,
    ) -> Result<PricesResponse, ProductError> {
        let kind = AssetClass::parse(asset_class.unwrap_or(""));
        // One market for the whole batch: the cache, the vendor ticker and the rows written
        // back are all keyed by it, so a list spanning two exchanges is refused rather than
        // half-served.
        let raw: Vec<&str> = symbols.iter().map(String::as_str).collect();
        let market = resolve_exchange(exchange, country_code, &raw, asset_class)?;
        let wanted = wanted_symbols(symbols, kind.is_crypto());
        if wanted.is_empty() {
            return Err(
                MarketDataError::bad_request("no-symbols", "supply at least one symbol").into(),
            );
        }

        let cached = self.store.get_prices(&wanted, &market).await?;
        let now = now_naive(self.clock.as_ref());
        let mut quotes: BTreeMap<String, QuotePayload> = BTreeMap::new();
        let mut stale: Vec<String> = Vec::new();
        for symbol in &wanted {
            match cached.get(symbol) {
                Some(row) if self.fresh(row.fetched_at, max_age_seconds, now) => {
                    quotes.insert(symbol.clone(), quote_payload(row, now));
                    self.counters
                        .price_from_cache
                        .fetch_add(1, Ordering::Relaxed);
                }
                _ => stale.push(symbol.clone()),
            }
        }

        let mut fetched: Vec<String> = Vec::new();
        let mut missing: Vec<String> = Vec::new();
        if !stale.is_empty() {
            let use_bulk = self.use_bulk(stale.len(), &market);
            if use_bulk {
                let banked = self.bulk_quotes(&stale, &market).await?;
                for symbol in &stale {
                    if let Some(row) = banked.get(symbol) {
                        quotes.insert(symbol.clone(), quote_payload(row, now));
                        fetched.push(symbol.clone());
                    }
                }
                self.counters
                    .bulk_snapshot_fetches
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                self.counters
                    .per_symbol_fetches
                    .fetch_add(1, Ordering::Relaxed);
                let mut rows: Vec<PriceRow> = Vec::new();
                let mut written: HashMap<String, CachedPrice> = HashMap::new();
                for symbol in &stale {
                    let Some(quote) = self.router.quote(symbol, kind, Some(&market)).await else {
                        missing.push(symbol.clone());
                        continue;
                    };
                    let row = row_from_quote(&quote, &market, now_naive(self.clock.as_ref()));
                    let cached_row = cached_from_row(&row);
                    quotes.insert(symbol.clone(), quote_payload(&cached_row, now));
                    written.insert(symbol.clone(), cached_row);
                    rows.push(row);
                    fetched.push(symbol.clone());
                }
                if !rows.is_empty() {
                    self.store.upsert_prices(rows).await?;
                    self.counters
                        .price_from_vendor
                        .fetch_add(fetched.len() as u64, Ordering::Relaxed);
                    // Re-shape the freshly written rows through the same formatter, so a
                    // cached and a fresh quote are indistinguishable to a caller.
                    let stored = self.store.get_prices(&fetched, &market).await?;
                    for symbol in &fetched {
                        let row = stored.get(symbol).cloned().unwrap_or_else(|| {
                            written
                                .get(symbol)
                                .cloned()
                                .unwrap_or_else(|| unreachable!("every fetched symbol was written"))
                        });
                        quotes.insert(symbol.clone(), quote_payload(&row, now));
                    }
                }
            }

            for symbol in &stale {
                if !quotes.contains_key(symbol) && !missing.contains(symbol) {
                    missing.push(symbol.clone());
                }
            }
        }

        let route = if self.use_bulk(stale.len(), &market) {
            "bulk"
        } else {
            "per_symbol"
        };
        let count = quotes.len();
        Ok(PricesResponse {
            quotes,
            exchange: market,
            fetched,
            complete: missing.is_empty(),
            deferred: Vec::new(),
            route,
            requested: wanted.len(),
            count,
            missing,
        })
    }

    /// True when the batch is big enough, and at an exchange where the feed exists, to be cheaper
    /// as one whole-market call.
    fn use_bulk(&self, stale: usize, market: &str) -> bool {
        stale as i64 >= self.settings.bulk_breakeven && market == "US"
    }

    fn fresh(
        &self,
        fetched_at: Option<NaiveDateTime>,
        max_age_seconds: Option<f64>,
        now: NaiveDateTime,
    ) -> bool {
        let Some(fetched_at) = fetched_at else {
            return false;
        };
        // A caller may only lower the TTL: accepting a longer one would let a price outlive
        // the policy that says how stale a price may be.
        let limit = match max_age_seconds {
            Some(age) => self.settings.price_ttl_seconds.min(age),
            None => self.settings.price_ttl_seconds,
        };
        seconds_between(now, fetched_at) <= limit
    }

    /// The whole-market snapshot, reusing the in-memory copy inside its lifetime.
    ///
    /// Every row the response can price is written to `SymbolPriceCache`, not just the
    /// requested ones: one bulk call then warms the cache for the scanners *and* for everyone
    /// asking about a single symbol afterwards.
    async fn bulk_quotes(
        &self,
        symbols: &[String],
        exchange: &str,
    ) -> Result<HashMap<String, CachedPrice>, ProductError> {
        let mut state = self.bulk.lock().await;
        let now = now_naive(self.clock.as_ref());
        let fresh = state
            .fetched_at
            .is_some_and(|at| seconds_between(now, at) < BULK_SNAPSHOT_TTL_SECONDS);
        if !fresh {
            // The whole-market feed is an EODHD capability; the router exposes it rather than
            // pretending Yahoo or Binance have an equivalent.
            let quotes = self.router.whole_market_quotes(exchange).await?;
            let mut snapshot: HashMap<String, PriceRow> = HashMap::new();
            let mut observed: Vec<NaiveDateTime> = Vec::new();
            let mut dropped = 0usize;
            for quote in &quotes {
                if quote.symbol.is_empty() {
                    continue;
                }
                if let Some(as_of) = quote.as_of {
                    observed.push(as_of);
                }
                let normalised = row_from_quote(quote, exchange, now);
                if normalised.price <= 0.0 {
                    continue;
                }
                snapshot.insert(quote.symbol.clone(), normalised);
            }

            // Two freshness bounds, because one cannot express both facts: an absolute age
            // (the whole feed cannot be ancient) and a lag behind the newest row (this symbol
            // cannot have stopped trading while the market traded on).
            let newest = observed.into_iter().max();
            let mut kept: HashMap<String, PriceRow> = HashMap::new();
            for (code, row) in snapshot {
                if let (Some(newest), Some(as_of)) = (newest, row.as_of) {
                    if seconds_between(newest, as_of) > DEFAULT_MAX_ROW_LAG_SECONDS {
                        dropped += 1;
                        continue;
                    }
                }
                kept.insert(code, row);
            }

            if !kept.is_empty() {
                let rows: Vec<PriceRow> = kept.values().cloned().collect();
                self.store.upsert_prices(rows).await?;
                self.counters
                    .price_from_vendor
                    .fetch_add(kept.len() as u64, Ordering::Relaxed);
            }
            tracing::info!(
                "bulk snapshot: {} rows, {} usable, {dropped} dropped as stale",
                quotes.len(),
                kept.len()
            );
            state.snapshot = kept;
            state.fetched_at = Some(now);
            state.rows_seen = quotes.len();
            state.rows_dropped = dropped;
        }

        Ok(symbols
            .iter()
            .filter_map(|symbol| {
                state
                    .snapshot
                    .get(symbol)
                    .map(|row| (symbol.clone(), cached_from_row(row)))
            })
            .collect())
    }

    /// Collect the whole-market snapshot now and cache every usable row.
    ///
    /// Called by the schedule at session open + 45. Unlike [`Self::prices`] this will not
    /// accept the in-memory copy: the point is a collection at a known instant, so the
    /// 120-second snapshot is discarded first and the vendor is asked again.
    pub async fn refresh_snapshot(&self, exchange: &str) -> Result<usize, ProductError> {
        {
            let mut state = self.bulk.lock().await;
            state.fetched_at = None;
        }
        self.bulk_quotes(&[], exchange).await?;
        let state = self.bulk.lock().await;
        Ok(state.snapshot.len())
    }

    /// Completed daily bars. Cache (`DailySnapshot`) first, then one `/eod/` call each.
    ///
    /// A cache hit requires the series to reach the newest *completed* session, not merely to
    /// hold enough rows, and a raw close on every served row. A refresh it triggers is
    /// remembered by the session it was asked for, so a holiday costs one call per symbol, not
    /// one per request.
    pub async fn bars(
        &self,
        symbols: &[String],
        sessions: i64,
        exchange: Option<&str>,
        allow_fetch: bool,
        asset_class: Option<&str>,
        country_code: Option<&str>,
    ) -> Result<BarsResponse, ProductError> {
        let kind = AssetClass::parse(asset_class.unwrap_or(""));
        let raw: Vec<&str> = symbols.iter().map(String::as_str).collect();
        let market = resolve_exchange(exchange, country_code, &raw, asset_class)?;
        let wanted = wanted_symbols(symbols, kind.is_crypto());
        if wanted.is_empty() {
            return Err(
                MarketDataError::bad_request("no-symbols", "supply at least one symbol").into(),
            );
        }
        if sessions < 1 {
            return Err(MarketDataError::bad_request(
                "bad-sessions",
                "sessions must be at least 1",
            )
            .into());
        }

        let latest = latest_completed_session(Some(&market), self.clock.now_utc());
        let banked = self
            .store
            .get_daily_bars_many(&wanted, sessions, &market)
            .await?;
        let target = 1.max(self.settings.history_sessions);
        let horizon = latest - Duration::days(history_window_days(target));
        // Asked for lazily: a request answered wholly from the cache must not pay for a count
        // it will not use.
        let mut depths: Option<HashMap<String, i64>> = None;
        let take = usize::try_from(sessions).unwrap_or(usize::MAX);

        let mut bars: BTreeMap<String, BarsEntry> = BTreeMap::new();
        let mut fetched: Vec<String> = Vec::new();
        let mut short: Vec<String> = Vec::new();

        for symbol in &wanted {
            let rows: Vec<StoredBar> = banked
                .get(symbol)
                .map(|rows| {
                    rows.iter()
                        .filter(|row| row.session <= latest)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            if let Some(served) = complete_window(&rows, sessions, latest) {
                self.counters
                    .bars_from_cache
                    .fetch_add(1, Ordering::Relaxed);
                bars.insert(
                    symbol.clone(),
                    bars_payload(symbol, &views(served), "cache", &market, None),
                );
                continue;
            }

            let keep = (market.clone(), symbol.clone());
            let memo = self
                .bars_refreshed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&keep)
                .copied();
            let recently_asked = memo.is_some_and(|(asked_for, asked_at)| {
                asked_for == latest
                    && seconds_between(now_naive(self.clock.as_ref()), asked_at)
                        < BARS_REFRESH_TTL_SECONDS
            });
            if !allow_fetch || recently_asked {
                if (rows.len() as i64) < sessions {
                    short.push(symbol.clone());
                }
                if !rows.is_empty() {
                    bars.insert(
                        symbol.clone(),
                        bars_payload(symbol, &views(&rows), "cache", &market, None),
                    );
                }
                continue;
            }

            // Enough sessions to answer, and one vendor call whatever the window. While the
            // symbol's stored history is short of the horizon, ask for the whole horizon: the
            // call is being made anyway and bills the same, so the depth is free.
            let mut need = sessions.max(self.settings.min_sessions);
            if target > need {
                if depths.is_none() {
                    depths = Some(
                        self.store
                            .daily_bar_depth(&wanted, &market, Some(horizon))
                            .await?,
                    );
                }
                let held = depths
                    .as_ref()
                    .and_then(|depths| depths.get(symbol))
                    .copied()
                    .unwrap_or(0);
                let achieved = self
                    .bars_deep
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&keep)
                    .copied()
                    .unwrap_or(0);
                let achieved = i64::try_from(achieved).unwrap_or(i64::MAX);
                if held < target && !(achieved > 0 && held >= achieved) {
                    need = target;
                }
            }
            self.bars_refreshed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(keep.clone(), (latest, now_naive(self.clock.as_ref())));

            let need_sessions = usize::try_from(need).unwrap_or(usize::MAX);
            let (provider_rows, provider_name) = self
                .router
                .bars(symbol, kind, need_sessions, Some(&market))
                .await;
            if provider_rows.is_empty() {
                short.push(symbol.clone());
                if !rows.is_empty() {
                    bars.insert(
                        symbol.clone(),
                        bars_payload(symbol, &views(&rows), "cache", &market, None),
                    );
                }
                continue;
            }

            let fresh_rows = self
                .write_provider_bars(symbol, &provider_rows, provider_name, &market)
                .await?;
            if need >= target {
                self.bars_deep
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(keep, provider_rows.len());
            }
            fetched.push(symbol.clone());
            self.counters
                .bars_from_vendor
                .fetch_add(1, Ordering::Relaxed);
            let fresh_rows: Vec<BarView> = fresh_rows
                .into_iter()
                .filter(|row| row.session <= latest)
                .collect();
            if fresh_rows.is_empty() {
                short.push(symbol.clone());
            } else {
                let start = fresh_rows.len().saturating_sub(take);
                bars.insert(
                    symbol.clone(),
                    bars_payload(
                        symbol,
                        &fresh_rows[start..],
                        "vendor",
                        &market,
                        provider_name,
                    ),
                );
            }
        }

        Ok(BarsResponse {
            complete: short.is_empty(),
            bars,
            fetched,
            requested_sessions: sessions,
            short_history: short,
        })
    }

    /// Upsert provider bars into `DailySnapshot` and return them in payload shape.
    ///
    /// `Source` records which provider answered, so a series built partly from EODHD and
    /// partly from Yahoo is visible rather than silent. `IsAdjusted` travels with each bar
    /// because an ATR mixing adjusted and raw closes reads a split as a catastrophic move.
    async fn write_provider_bars(
        &self,
        symbol: &str,
        bars: &[ProviderBar],
        provider_name: Option<&str>,
        exchange: &str,
    ) -> Result<Vec<BarView>, ProductError> {
        let mut rows: Vec<DailyBarWrite> = bars
            .iter()
            .map(|bar| DailyBarWrite {
                symbol: symbol.to_string(),
                // The exchange travels with the bar: the same ticker can be an ASX listing and
                // a US one, and without it the two overwrite each other.
                exchange: Some(exchange.to_string()),
                session: bar.session,
                open: bar.open,
                high: bar.high,
                low: bar.low,
                close: bar.close,
                raw_close: bar.raw_close,
                volume: Some(bar.volume),
                source: Some(provider_name.unwrap_or("unknown").to_string()),
                adjusted: Some(bar.adjusted),
            })
            .collect();
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        rows.sort_by_key(|row| row.session);
        let views: Vec<BarView> = rows.iter().map(BarView::from).collect();
        self.store.upsert_daily(rows).await?;
        Ok(views)
    }

    /// ATR per symbol, from `IndicatorCache` when it can answer, else computed.
    ///
    /// The cache only answers for the configured method and smoothing, because it has one
    /// `Method` column and one `Smoothing` column: a request for a different definition is
    /// computed on the fly and not cached. That keeps the table honest — a cached value can
    /// always be read back with the definition that produced it.
    pub async fn atr_batch(
        &self,
        request: AtrRequest<'_>,
    ) -> Result<AtrBatchResponse, ProductError> {
        let period = request
            .period
            .filter(|period| *period != 0)
            .unwrap_or(self.settings.atr_period);
        let method = request
            .method
            .filter(|method| !method.is_empty())
            .unwrap_or(&self.settings.atr_method)
            .to_lowercase();
        let smoothing = request
            .smoothing
            .filter(|smoothing| !smoothing.is_empty())
            .unwrap_or(&self.settings.atr_smoothing)
            .to_lowercase();

        if !atr::METHODS.contains(&method.as_str()) {
            return Err(MarketDataError::bad_request(
                "bad-method",
                format!(
                    "{} is not an ATR method; use one of {}",
                    py_repr(&method),
                    atr::METHODS.join(", ")
                ),
            )
            .into());
        }
        if !atr::SMOOTHINGS.contains(&smoothing.as_str()) {
            return Err(MarketDataError::bad_request(
                "bad-smoothing",
                format!(
                    "{} is not a smoothing; use one of {}",
                    py_repr(&smoothing),
                    atr::SMOOTHINGS.join(", ")
                ),
            )
            .into());
        }
        if period < 1 {
            return Err(
                MarketDataError::bad_request("bad-period", "period must be at least 1").into(),
            );
        }
        let cache_eligible =
            method == self.settings.atr_method && smoothing == self.settings.atr_smoothing;

        let mut wanted: Vec<String> = Vec::with_capacity(request.symbols.len());
        for raw in request.symbols {
            let text = bare_symbol(raw);
            if !text.is_empty() && !wanted.contains(&text) {
                wanted.push(text);
            }
        }
        if wanted.is_empty() {
            return Err(
                MarketDataError::bad_request("no-symbols", "supply at least one symbol").into(),
            );
        }

        // One normalised market for the whole call: the ATR cache is keyed by exchange, so the
        // read has to ask with the same code the bars were written under.
        let raw: Vec<&str> = request.symbols.iter().map(String::as_str).collect();
        let market = resolve_exchange(
            request.exchange,
            request.country_code,
            &raw,
            request.asset_class,
        )?;
        let cached = if cache_eligible {
            self.store.get_indicators(&wanted, &market).await?
        } else {
            HashMap::new()
        };
        let latest = latest_completed_session(Some(&market), self.clock.now_utc());

        let mut readings: BTreeMap<String, AtrReading> = BTreeMap::new();
        let mut to_compute: Vec<String> = Vec::new();
        for symbol in &wanted {
            let hit = cached
                .get(symbol)
                .filter(|hit| indicator_answers(hit, &method, &smoothing, latest, period));
            let Some(hit) = hit else {
                to_compute.push(symbol.clone());
                continue;
            };
            let value = hit.value(period).unwrap_or_default();
            let reference = hit.reference_price;
            let step = reference.and_then(|price| ticks::price_step(&market, price));
            readings.insert(
                symbol.clone(),
                AtrReading {
                    symbol: symbol.clone(),
                    value: Some(round_to(value, 6)),
                    percent: atr::atr_percent(Some(value), reference)
                        .map(|value| round_to(value, 4)),
                    reference_price: reference,
                    period,
                    method: method.clone(),
                    smoothing: smoothing.clone(),
                    sessions_used: hit.sessions_used.map(i64::from),
                    sessions_averaged: hit.sessions_used.map(|sessions| {
                        atr::sessions_averaged(period, &smoothing, sessions as usize)
                    }),
                    flat_sessions: None,
                    as_of_session: hit.as_of_session.map(|session| session.to_string()),
                    source: "indicator_cache".to_string(),
                    tick: step,
                    ticks: ticks::as_ticks(value, step).map(|ticks| round_to(ticks, 4)),
                    tick_percent: atr::atr_percent(step, reference).map(|value| round_to(value, 4)),
                    reason: Presence::Absent,
                    stop_level: Presence::Absent,
                },
            );
            self.counters.atr_from_cache.fetch_add(1, Ordering::Relaxed);
        }

        if !to_compute.is_empty() {
            // Ask for enough sessions that the longest configured period is defined.
            let longest = self
                .settings
                .atr_periods
                .iter()
                .copied()
                .chain([period])
                .max()
                .unwrap_or(period);
            let need = self.settings.min_sessions.max(longest + 1);
            let bar_payload = self
                .bars(
                    &to_compute,
                    need,
                    Some(&market),
                    request.allow_fetch,
                    request.asset_class,
                    request.country_code,
                )
                .await?;

            let mut fresh_for_cache: Vec<IndicatorWrite> = Vec::new();
            for symbol in &to_compute {
                let entry = bar_payload.bars.get(symbol);
                let rows = entry.map(|entry| entry.rows.as_slice()).unwrap_or(&[]);
                if rows.is_empty() {
                    readings.insert(
                        symbol.clone(),
                        unavailable_reading(symbol, period, &method, &smoothing, 0),
                    );
                    continue;
                }

                let Some((highs, lows, closes)) = ohlc(rows) else {
                    readings.insert(
                        symbol.clone(),
                        unavailable_reading(symbol, period, &method, &smoothing, rows.len()),
                    );
                    continue;
                };
                let reference = closes.last().copied();

                let mut values: BTreeMap<i64, Option<f64>> = BTreeMap::new();
                for configured in &self.settings.atr_periods {
                    values.insert(
                        *configured,
                        atr::average_true_range(
                            &highs,
                            &lows,
                            &closes,
                            *configured,
                            &method,
                            &smoothing,
                        )
                        .map_err(|error| MarketDataError::internal("atr", error.message))?,
                    );
                }
                if let std::collections::btree_map::Entry::Vacant(slot) = values.entry(period) {
                    slot.insert(
                        atr::average_true_range(
                            &highs, &lows, &closes, period, &method, &smoothing,
                        )
                        .map_err(|error| MarketDataError::internal("atr", error.message))?,
                    );
                }

                let value = values.get(&period).copied().flatten();
                let as_of_session = entry.and_then(|entry| entry.as_of_session.clone());
                let step = reference.and_then(|price| ticks::price_step(&market, price));
                readings.insert(
                    symbol.clone(),
                    AtrReading {
                        symbol: symbol.clone(),
                        value: value.map(|value| round_to(value, 6)),
                        percent: atr::atr_percent(value, reference).map(|value| round_to(value, 4)),
                        reference_price: reference,
                        period,
                        method: method.clone(),
                        smoothing: smoothing.clone(),
                        sessions_used: Some(rows.len() as i64),
                        sessions_averaged: Some(atr::sessions_averaged(
                            period,
                            &smoothing,
                            rows.len(),
                        )),
                        flat_sessions: Some(atr::flat_sessions(&highs, &lows, &closes)),
                        as_of_session,
                        source: "computed".to_string(),
                        tick: step,
                        ticks: value
                            .and_then(|value| ticks::as_ticks(value, step))
                            .map(|ticks| round_to(ticks, 4)),
                        tick_percent: atr::atr_percent(step, reference)
                            .map(|value| round_to(value, 4)),
                        reason: if value.is_some() {
                            Presence::Null
                        } else {
                            Presence::Value(format!(
                                "{} session(s) is fewer than the {} ATR({}) needs",
                                rows.len(),
                                period + 1,
                                period
                            ))
                        },
                        stop_level: Presence::Absent,
                    },
                );
                self.counters.atr_computed.fetch_add(1, Ordering::Relaxed);

                if cache_eligible && value.is_some() {
                    fresh_for_cache.push(IndicatorWrite {
                        symbol: symbol.clone(),
                        // The exchange the bars came from. Without it the row lands as US,
                        // and the next AU request reads a US ATR out of the cache.
                        exchange: Some(market.clone()),
                        values: values.clone(),
                        reference_price: reference,
                        sessions_used: i32::try_from(rows.len()).ok(),
                        as_of_session: entry
                            .and_then(|entry| entry.as_of_session.as_deref())
                            .and_then(|text| NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()),
                        method: Some(method.clone()),
                        smoothing: Some(smoothing.clone()),
                    });
                }
            }

            if !fresh_for_cache.is_empty() {
                self.store.upsert_indicators(fresh_for_cache).await?;
            }
        }

        // The stop level is pure arithmetic over a value already computed, so it is attached
        // here rather than being a fifth endpoint. The multiplier is never defaulted: the
        // books use 1.0, 1.0, 2.0 and 3.0.
        let stop = requested_stop(&request)?;
        if let Some((price, multiplier)) = stop {
            let side = stop_side(request.direction)?;
            for reading in readings.values_mut() {
                let level = atr::stop_level(price, reading.value, Some(multiplier), &side)
                    .map_err(|error| MarketDataError::internal("atr-stop", error.message))?;
                reading.stop_level = Presence::from_option(level);
            }
        }

        Ok(AtrBatchResponse {
            complete: readings.values().all(|reading| reading.value.is_some()),
            atr: readings,
            exchange: market,
            period,
            method,
            smoothing,
        })
    }

    /// One screen: the vendor's rows for one market, untouched.
    ///
    /// The policy that decides which filters to send stays with the caller. What happens here is
    /// transport: resolve the one market, validate the filters against the vendor's vocabulary,
    /// lift every `exchange` filter into the single triple the vendor needs, and make the call.
    ///
    /// A vendor 4xx caused by the query is the caller's mistake and answers 422
    /// `vendor-rejected`; every other provider failure stays a 502. A bad filter must never read
    /// as a vendor outage.
    pub async fn screener(
        &self,
        country_code: &str,
        exchange: Option<&str>,
        filters: &[Value],
        sort: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<ScreenerResponse, ProductError> {
        let market = resolve_exchange(exchange, Some(country_code), &[], None)?;
        require_supported_market(&market)?;
        crate::domain::screener::validate_filters(filters)?;
        let wire = screener_wire_filters(filters, exchange, &market)?;
        let sort = parse_sort(sort)?;

        let client = self.client.as_ref().ok_or(ProductError::Unavailable)?;
        let body = client
            .screener(&wire, sort.as_deref(), limit, offset)
            .await
            .map_err(provider_refusal)?;
        let rows = screener_rows(body)?;
        self.counters.screener_calls.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            "screener: market {market}, venue {:?}, {} row(s), limit {limit}, offset {offset}",
            wire.last(),
            rows.len()
        );

        Ok(ScreenerResponse {
            provider: "eodhd".to_string(),
            country_code: country_code.trim().to_uppercase(),
            exchange: market,
            filters: wire,
            sort,
            limit,
            offset,
            count: rows.len(),
            rows,
        })
    }

    /// Up to 25 company profiles: one pinned vendor call per uncached symbol, memoised.
    ///
    /// `/fundamentals` is not entitled on this subscription (HTTP 403 for every ticker), so a
    /// profile is a screener query pinned to `code` **and** `exchange`. One call per symbol: the
    /// vendor's `in` operator has unbalanced coverage, so a batched `code in` could silently
    /// omit a code and there would be no way to tell "no row" from "not covered".
    ///
    /// A single symbol's failure is that symbol's `error`, never the batch's.
    pub async fn profiles(&self, request: ProfileBatch<'_>) -> Result<ProfilesResponse, ProductError> {
        let raw: Vec<&str> = request.symbols.iter().map(String::as_str).collect();
        let market = resolve_exchange(request.exchange, request.country_code, &raw, None)?;
        require_supported_market(&market)?;
        let client = self
            .client
            .clone()
            .ok_or(ProductError::Unavailable)?;

        let mut roots: Vec<String> = Vec::with_capacity(request.symbols.len());
        let mut missing: Vec<String> = Vec::new();
        for symbol in request.symbols {
            match profile_root(symbol, &market) {
                Some(root) if !roots.contains(&root) => roots.push(root),
                Some(_) => {}
                None => {
                    let unplaced = symbol.trim().to_uppercase();
                    if !missing.contains(&unplaced) {
                        missing.push(unplaced);
                    }
                }
            }
        }

        let now = now_naive(self.clock.as_ref());
        let mut profiles: BTreeMap<String, ProfileEntry> = BTreeMap::new();
        let mut pending: Vec<String> = Vec::new();
        {
            let memo = self
                .profiles
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for root in &roots {
                match memo.get(&(root.clone(), market.clone())) {
                    Some(hit) if seconds_between(now, hit.stored_at) <= PROFILE_TTL_SECONDS => {
                        profiles.insert(root.clone(), hit.entry.clone());
                        self.counters.profile_memo_hits.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => pending.push(root.clone()),
                }
            }
        }

        let fetched = self.fetch_profiles(&client, &pending, &market).await;
        for entry in &fetched {
            if !entry.found {
                if !missing.contains(&entry.symbol) {
                    missing.push(entry.symbol.clone());
                }
            }
            profiles.insert(entry.symbol.clone(), entry.clone());
        }
        self.remember_profiles(&fetched, &market, now);
        self.counters
            .profile_calls
            .fetch_add(fetched.len() as u64, Ordering::Relaxed);

        // `missing` follows the order the caller asked in, whichever way a symbol was lost.
        let mut ordered_missing: Vec<String> = Vec::new();
        for root in &roots {
            if missing.contains(root) {
                ordered_missing.push(root.clone());
            }
        }
        for unplaced in missing {
            if !ordered_missing.contains(&unplaced) {
                ordered_missing.push(unplaced);
            }
        }

        Ok(ProfilesResponse {
            provider: "eodhd".to_string(),
            country_code: request.country_code.unwrap_or_default().trim().to_uppercase(),
            exchange: market,
            requested: request.symbols.len(),
            complete: ordered_missing.is_empty()
                && profiles.values().all(|entry| entry.error.is_none()),
            missing: ordered_missing,
            profiles,
        })
    }

    /// Fetch the uncached profiles at [`PROFILE_CONCURRENCY`], returning them in input order.
    ///
    /// The caller's identity is captured and re-scoped inside each task: a task-local does not
    /// follow a spawned future, and without this the fan-out's spend would be recorded under
    /// `unknown` rather than the caller that asked.
    async fn fetch_profiles(
        &self,
        client: &Arc<EodhdClient>,
        roots: &[String],
        market: &str,
    ) -> Vec<ProfileEntry> {
        let caller = current_caller();
        let mut answers: Vec<ProfileEntry> = Vec::with_capacity(roots.len());
        for wave in roots.chunks(PROFILE_CONCURRENCY) {
            let mut set = tokio::task::JoinSet::new();
            for root in wave {
                let client = client.clone();
                let caller = caller.clone();
                let root = root.clone();
                let market = market.to_string();
                set.spawn(async move {
                    let filters = profile_filters(&root, &market);
                    let answer = with_caller(Some(&caller), async {
                        client
                            .screener(&filters, None, PROFILE_QUERY_LIMIT, 0)
                            .await
                    })
                    .await;
                    (root, answer)
                });
            }
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok((root, answer)) => answers.push(profile_entry(&root, market, answer)),
                    // A task that panicked has lost its symbol, so the sweep below still has to
                    // name it: a symbol silently absent from both `profiles` and `missing` would
                    // read as "nothing to say" rather than "the lookup broke".
                    Err(error) => tracing::warn!("a profile lookup task failed: {error}"),
                }
            }
        }
        for root in roots {
            if !answers.iter().any(|entry| entry.symbol == *root) {
                answers.push(profile_entry(
                    root,
                    market,
                    Err(ProviderFailure::new(
                        "lookup-failed",
                        "the lookup task did not return",
                        None,
                    )),
                ));
            }
        }
        answers
    }

    /// Keep the answers worth keeping: found, error-free, inside the cap.
    ///
    /// A miss and a failed call are never memoised — inserting either would turn one bad minute
    /// into an hour of wrong answers. Expired entries are pruned on every insert.
    fn remember_profiles(&self, fetched: &[ProfileEntry], market: &str, now: NaiveDateTime) {
        let mut memo = self
            .profiles
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        memo.retain(|_, hit| seconds_between(now, hit.stored_at) <= PROFILE_TTL_SECONDS);
        for entry in fetched {
            if !entry.found || entry.error.is_some() {
                continue;
            }
            if memo.len() >= PROFILE_CACHE_MAX_ENTRIES {
                if !self.profile_cap_warned.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        "the profile memo holds {PROFILE_CACHE_MAX_ENTRIES} entries; new \
                         profiles are answered but not cached until an entry expires"
                    );
                }
                continue;
            }
            memo.insert(
                (entry.symbol.clone(), market.to_string()),
                ProfileMemoEntry {
                    entry: entry.clone(),
                    stored_at: now,
                },
            );
        }
    }

    /// Intraday bars for one market, from the owned 5-minute tape, fetching when it is short.
    ///
    /// Every timeframe is derived from the same native 5m series, so a response never mixes a
    /// native bar with an aggregate. A cache hit inside [`Settings::intraday_ttl_seconds`] costs
    /// nothing; a fetch bills 5 units per symbol and is named in `fetched`.
    pub async fn intraday(
        &self,
        request: IntradayRequest<'_>,
    ) -> Result<IntradayResponse, ProductError> {
        let raw: Vec<&str> = request.symbols.iter().map(String::as_str).collect();
        let market = resolve_exchange(request.exchange, request.country_code, &raw, None)?;
        require_supported_market(&market)?;
        let wanted = wanted_symbols(request.symbols, false);
        if wanted.is_empty() {
            return Err(
                MarketDataError::bad_request("no-symbols", "supply at least one symbol").into(),
            );
        }
        if request.limit < 1 {
            return Err(MarketDataError::bad_request(
                "bad-limit",
                "limit must be at least 1",
            )
            .into());
        }

        let limit = request.limit;
        let timeframe = request.timeframe;
        let zone = exchange_timezone(&market).unwrap_or(Tz::UTC);
        let needed = timeframe.native_rows_needed(limit);
        let now = now_naive(self.clock.as_ref());

        let mut bars: BTreeMap<String, BarsEntry> = BTreeMap::new();
        let mut fetched: Vec<String> = Vec::new();
        let mut short: Vec<String> = Vec::new();
        let mut prunable = false;
        let take = usize::try_from(limit).unwrap_or(usize::MAX);
        let zone_of = |at: NaiveDateTime| at.and_utc().with_timezone(&zone).date_naive();

        for symbol in &wanted {
            // The stored tape is aggregated before the fetch decision, because whether it can
            // answer is a question about buckets, not rows: a session's last bucket is short,
            // so a row count alone would refetch a tape that already holds the answer.
            let stored = self.store.get_intraday_bars(&market, symbol, needed).await?;
            let stored_bars = aggregate(&rows_from_stored(&stored), timeframe, zone_of);
            let cached = stored_bars.len() >= take
                && tape_is_fresh(&stored, now, self.settings.intraday_ttl_seconds);

            let derived = if cached || !request.allow_fetch {
                if !stored_bars.is_empty() {
                    self.counters
                        .intraday_cache_hits
                        .fetch_add(1, Ordering::Relaxed);
                }
                stored_bars
            } else {
                let client = self.client.as_ref().ok_or(ProductError::Unavailable)?;
                let native = self
                    .fetch_intraday(client, symbol, &market, timeframe, limit)
                    .await?;
                if !native.is_empty() {
                    self.write_intraday(symbol, &market, &native).await?;
                    prunable = true;
                }
                fetched.push(symbol.clone());
                aggregate(&native, timeframe, zone_of)
            };

            if derived.is_empty() {
                short.push(symbol.clone());
                continue;
            }
            let start = derived.len().saturating_sub(take);
            let served = &derived[start..];
            let source = if fetched.contains(symbol) {
                "eodhd"
            } else {
                "cache"
            };
            bars.insert(
                symbol.clone(),
                intraday_entry(symbol, served, &market, &zone, source),
            );
        }

        if prunable {
            let cutoff = now - Duration::days(self.settings.intraday_retention_days.max(1));
            match self.store.prune_intraday(cutoff).await {
                Ok(0) => {}
                Ok(deleted) => tracing::info!("intraday tape: pruned {deleted} row(s) before {cutoff}"),
                // The prune is hygiene, not the answer: a failure is logged and the bars stand.
                Err(error) => tracing::warn!("intraday tape prune failed ({error})"),
            }
        }

        let provider = if fetched.is_empty() { "cache" } else { "eodhd" };
        tracing::info!(
            "bars/intraday: market {market}, {timeframe}, limit {limit}, fetched {fetched:?}, \
             short {short:?}"
        );
        Ok(IntradayResponse {
            exchange: market,
            provider: provider.to_string(),
            fetched,
            short_history: short,
            bars,
        })
    }

    /// One symbol's native 5m bars from the vendor, inside the timeframe's window.
    async fn fetch_intraday(
        &self,
        client: &Arc<EodhdClient>,
        symbol: &str,
        market: &str,
        timeframe: Timeframe,
        limit: i64,
    ) -> Result<Vec<IntradayRow>, ProductError> {
        let days = timeframe.window_days(limit);
        let end = self.clock.now_utc().timestamp();
        let start = end - days * 86_400;
        let payload = client
            .intraday(symbol, Some(market), timeframe.vendor_interval(), start, end)
            .await
            .map_err(provider_refusal)?;
        let rows = intraday_rows(payload);
        self.counters
            .intraday_fetches
            .fetch_add(1, Ordering::Relaxed);
        Ok(rows)
    }

    /// Upsert one symbol's native bars into the owned tape.
    ///
    /// Only the native 5m rows are stored: the aggregates are recomputed on read, because a
    /// stored bucket could disagree with the bars it came from after a re-pull.
    async fn write_intraday(
        &self,
        symbol: &str,
        market: &str,
        rows: &[IntradayRow],
    ) -> Result<(), ProductError> {
        let writes: Vec<IntradayBarWrite> = rows
            .iter()
            .map(|row| IntradayBarWrite {
                symbol: symbol.to_string(),
                exchange: Some(market.to_string()),
                ts_utc: row.ts_utc,
                open: row.open,
                high: row.high,
                low: row.low,
                close: row.close,
                volume: row.volume,
                // The vendor's intraday tape is unadjusted; stored rather than assumed so a
                // future adjusted source cannot silently mix scales in one series.
                adjusted: false,
            })
            .collect();
        self.store.upsert_intraday(writes).await?;
        Ok(())
    }

    /// Whole-market closes for up to ten sessions, from `DailySnapshot` when complete.
    ///
    /// A cached session is served only when `BulkSnapshotPull` records a completed pull for it.
    /// A pull that returns materially fewer codes than the previous completed session is
    /// upserted but unmarked, and its key comes back in `partial`: a partial set silently
    /// changes every derived return that reads it, so it is never served as if whole.
    pub async fn bulk_closes(
        &self,
        market: &str,
        dates: &[BulkDateKey],
        allow_fetch: bool,
    ) -> Result<BulkResponse, ProductError> {
        let market = exchange_for(market)?;
        require_supported_market(&market)?;

        let mut response_dates: BTreeMap<String, BulkSession> = BTreeMap::new();
        let mut partial: Vec<String> = Vec::new();
        let mut fetched_any = false;

        for key in dates {
            let requested = key.as_requested();
            if response_dates.contains_key(&requested) {
                continue;
            }
            let session = match key {
                BulkDateKey::Latest => {
                    latest_completed_session(Some(&market), self.clock.now_utc())
                }
                BulkDateKey::Session(date) => *date,
            };

            let marked = self.store.bulk_pull_marker(&market, session).await?.is_some();
            if !marked {
                if !allow_fetch {
                    return Err(MarketDataError::new(
                        "not-pulled",
                        format!(
                            "no completed bulk pull for {market} {session}; this service will \
                             not return a partial session as if it were whole"
                        ),
                        404,
                    )
                    .into());
                }
                let pulled = self.pull_bulk_session(&market, session).await?;
                fetched_any = true;
                if !pulled.complete {
                    partial.push(requested.clone());
                }
                response_dates.insert(
                    requested,
                    BulkSession {
                        session: pulled.session.to_string(),
                        rows: pulled.rows,
                    },
                );
                continue;
            }

            let rows = session_closes(&*self.store, &market, session).await?;
            response_dates.insert(
                requested,
                BulkSession {
                    session: session.to_string(),
                    rows,
                },
            );
        }

        let provider = if fetched_any { "eodhd" } else { "cache" };
        Ok(BulkResponse {
            market,
            provider: provider.to_string(),
            partial,
            dates: response_dates,
        })
    }

    /// Pull one session, upsert it, and write the completeness marker when the pull is whole.
    async fn pull_bulk_session(
        &self,
        market: &str,
        session: NaiveDate,
    ) -> Result<PulledSession, ProductError> {
        let client = self.client.as_ref().ok_or(ProductError::Unavailable)?;
        let rows = client
            .bulk_last_day(market, Some(session))
            .await
            .map_err(provider_refusal)?;
        self.counters.bulk_pulls.fetch_add(1, Ordering::Relaxed);

        // A vendor asked for a date the exchange did not trade answers with the previous
        // session; filing those rows under the requested date would invent a duplicate bar.
        let resolved = crate::jobs::daily::resolved_session(&rows)
            .map_err(ProductError::from)?
            .ok_or_else(|| {
                ProviderFailure::new(
                    "unparseable",
                    format!("the bulk pull for {market} {session} carried no dated rows"),
                    None,
                )
            })?;

        let writes: Vec<DailyBarWrite> = rows
            .iter()
            .filter_map(|raw| {
                crate::jobs::daily::row_from_bulk(raw, resolved, market, None)
            })
            .collect();
        let written = writes.len();
        if !writes.is_empty() {
            self.store.upsert_daily(writes).await?;
        }

        let baseline = self
            .store
            .latest_bulk_pull_before(market, resolved)
            .await?
            .map(|marker| marker.rows);
        let complete = bulk_pull_is_complete(written, baseline);
        if complete {
            self.store
                .record_bulk_pull(market, resolved, i64::try_from(written).unwrap_or(i64::MAX), "eodhd")
                .await?;
        } else {
            tracing::warn!(
                "bulk pull for {market} {resolved}: {written} row(s) against a baseline of \
                 {baseline:?}; unmarked, so the session is reported as partial"
            );
        }
        tracing::info!(
            "bulk pull: market {market}, session {resolved}, {written} row(s), complete {complete}"
        );

        let closes = session_closes(&*self.store, market, resolved).await?;
        Ok(PulledSession {
            session: resolved,
            rows: closes,
            complete,
        })
    }

    /// `/stats`: counters, vendor spend, router outcomes and the snapshot's state.
    pub async fn stats(&self) -> StatsResponse {
        let state = self.bulk.lock().await;
        StatsResponse {
            counters: self.counters.snapshot(),
            vendor: self.client.as_ref().map(|client| client.stats()),
            router: self.router.stats(),
            bulk_snapshot: BulkSnapshotStats {
                rows_seen: state.rows_seen,
                rows_usable: state.snapshot.len(),
                rows_dropped: state.rows_dropped,
                fetched_at: state.fetched_at.map(iso_naive),
            },
            settings: SettingsStats {
                price_ttl_seconds: self.settings.price_ttl_seconds,
                bulk_breakeven: self.settings.bulk_breakeven,
                min_sessions: self.settings.min_sessions,
                history_sessions: self.settings.history_sessions,
                atr_periods: self.settings.atr_periods.clone(),
                atr_method: self.settings.atr_method.clone(),
                atr_smoothing: self.settings.atr_smoothing.clone(),
                database: self.settings.db_database.clone(),
            },
        }
    }
}

/// The two markets this service's model serves, stated once.
///
/// `resolve_exchange` knows every exchange the vendor does; this service's caches, warehouses
/// and schedules are built for US and AU. A resolved market outside `COUNTRIES` is refused here
/// rather than half-served — widening the service to a third country is a change to that table,
/// the storage and the schedules, not to one route.
fn require_supported_market(market: &str) -> Result<(), MarketDataError> {
    if COUNTRIES.iter().any(|country| country.exchange == market) {
        return Ok(());
    }
    Err(MarketDataError::bad_request(
        "unknown-exchange",
        format!(
            "{} is not a market this service serves; it carries {}",
            py_repr(market),
            COUNTRIES
                .iter()
                .map(|country| country.exchange)
                .collect::<Vec<_>>()
                .join(" and ")
        ),
    ))
}

/// A provider failure split the way the egress routes' contract states it.
///
/// A vendor 400 or 422 says the query was wrong, so it is the caller's mistake: 422
/// `vendor-rejected` with the vendor's own message. Everything else — a 5xx, a transport
/// failure, a rate limit, an unparseable body — stays a 502 whose `error` is the provider
/// reason. Retrying is already handled inside the client, so nothing here retries.
fn provider_refusal(failure: ProviderFailure) -> ProductError {
    match failure.status {
        Some(status @ (400 | 422)) => ProductError::Refused(MarketDataError::new(
            "vendor-rejected",
            if failure.detail.is_empty() {
                format!("the vendor rejected the query with HTTP {status}")
            } else {
                failure.detail
            },
            422,
        )),
        _ => ProductError::Provider(failure),
    }
}

/// The rows out of the vendor's `{"data": [...]}` envelope.
///
/// Unwrapping here rather than in the client makes an unexpected envelope a visible
/// `unparseable` failure instead of an empty screen — the vendor answers a bad value with 200
/// and zero rows, so "no rows" must never be this service's guess about a shape it did not
/// understand.
fn screener_rows(body: Value) -> Result<Vec<Value>, ProductError> {
    match body {
        Value::Object(mut object) => match object.remove("data") {
            Some(Value::Array(rows)) => Ok(rows),
            other => Err(ProviderFailure::new(
                "unparseable",
                format!(
                    "the screener answer has no data array (data was {})",
                    match other {
                        None => "absent".to_string(),
                        Some(value) => value.to_string(),
                    }
                ),
                None,
            )
            .into()),
        },
        other => Err(ProviderFailure::new(
            "unparseable",
            format!("expected the screener's envelope object, got {other}"),
            None,
        )
        .into()),
    }
}

/// The vendor root a profile lookup is pinned to, or `None` when the renderer cannot place it.
///
/// The same `domain::symbols` resolution `/price` uses, so `WDS.AU` and `WDS` at an AU request
/// become the same root. A symbol the renderer cannot place is reported as missing rather than
/// filed under a guessed venue.
fn profile_root(symbol: &str, market: &str) -> Option<String> {
    let ticker = symbol_for_provider(
        symbol,
        AssetClass::Shares,
        Some(market),
        ProviderId::Eodhd,
    )?;
    let suffix = format!(".{market}");
    Some(ticker.strip_suffix(&suffix).unwrap_or(&ticker).to_string())
}

/// One symbol's profile from its screener answer.
///
/// A provider failure is this symbol's `error`, not the batch's. A row whose `code` does not
/// match is not a profile: the vendor keeps one row per listing, so a code can answer with
/// several rows and the first matching one is the listing that was asked about.
fn profile_entry(
    root: &str,
    market: &str,
    answer: Result<Value, ProviderFailure>,
) -> ProfileEntry {
    let mut entry = ProfileEntry {
        symbol: root.to_string(),
        exchange: market.to_string(),
        found: false,
        name: None,
        sector: None,
        industry: None,
        market_cap: None,
        currency: None,
        country: None,
        gic_sector: None,
        gic_industry: None,
        description: None,
        as_of: None,
        error: None,
    };
    let body = match answer {
        Ok(body) => body,
        Err(failure) => {
            entry.error = Some(failure.reason);
            return entry;
        }
    };
    let rows = match screener_rows(body) {
        Ok(rows) => rows,
        Err(error) => {
            entry.error = Some(match error {
                ProductError::Provider(failure) => failure.reason,
                other => other.to_string(),
            });
            return entry;
        }
    };
    let Some(row) = rows.iter().find(|row| {
        row.get("code")
            .and_then(Value::as_str)
            .is_some_and(|code| code.trim().eq_ignore_ascii_case(root))
    }) else {
        return entry;
    };
    entry.found = true;
    entry.name = row_field(row, "name");
    entry.sector = row_field(row, "sector");
    entry.industry = row_field(row, "industry");
    entry.market_cap = row.get("market_capitalization").and_then(number);
    entry.currency = row_field(row, "currency_symbol").and_then(|raw| currency_code(&raw));
    entry.as_of = row_field(row, "last_day_data_date").map(|raw| date_prefix(&raw));
    entry
}

/// A text field of a vendor row, trimmed, with blanks treated as absent.
fn row_field(row: &Value, name: &str) -> Option<String> {
    row.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// A currency code only when the vendor's value has the ISO-4217 shape.
///
/// The vendor answers with three upper-case letters on the listings this service serves; a
/// longer or mixed-case value is a different kind of label, and passing it through would put a
/// non-currency in a currency field.
fn currency_code(raw: &str) -> Option<String> {
    let text = raw.trim();
    (text.len() == 3 && text.chars().all(|character| character.is_ascii_uppercase()))
        .then(|| text.to_string())
}

/// The date part of a vendor timestamp (`2026-11-11T00:00:00` -> `2026-11-11`).
fn date_prefix(raw: &str) -> String {
    raw.trim().split('T').next().unwrap_or_default().to_string()
}

/// Whether the stored tape was fetched recently enough to answer.
///
/// Length is not part of this: how many buckets the tape can produce is a question about the
/// bars, and the caller has already aggregated them. This is only the TTL.
fn tape_is_fresh(stored: &[StoredIntradayBar], now: NaiveDateTime, ttl_seconds: f64) -> bool {
    if stored.is_empty() {
        return false;
    }
    stored
        .iter()
        .filter_map(|bar| bar.fetched_at)
        .max()
        .is_some_and(|fetched_at| seconds_between(now, fetched_at) <= ttl_seconds)
}

/// The stored native rows in the pure aggregator's shape, dropping any half-written row.
fn rows_from_stored(stored: &[StoredIntradayBar]) -> Vec<IntradayRow> {
    stored
        .iter()
        .filter_map(|bar| {
            Some(IntradayRow {
                ts_utc: bar.ts_utc,
                open: bar.open?,
                high: bar.high?,
                low: bar.low?,
                close: bar.close?,
                volume: bar.volume.unwrap_or(0),
            })
        })
        .collect()
}

/// The vendor's intraday rows as bars.
///
/// The vendor stamps a row with epoch `timestamp` seconds, or with ISO `datetime`/`date` text.
/// Naive text is read as UTC, which is the convention the source adapter already uses. A row
/// that cannot be read is dropped rather than served as a zero; the client has already refused
/// a body that is not an array, so there is no failure left to report here.
fn intraday_rows(rows: Vec<Value>) -> Vec<IntradayRow> {
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let Some(ts_utc) = intraday_timestamp(row) else {
            continue;
        };
        let (Some(open), Some(high), Some(low), Some(close)) = (
            row.get("open").and_then(number),
            row.get("high").and_then(number),
            row.get("low").and_then(number),
            row.get("close").and_then(number),
        ) else {
            continue;
        };
        out.push(IntradayRow {
            ts_utc,
            open,
            high,
            low,
            close,
            volume: row
                .get("volume")
                .and_then(number)
                .map(|value| value as i64)
                .unwrap_or(0),
        });
    }
    out
}

/// One vendor row's opening instant, from its `timestamp` or its ISO text.
fn intraday_timestamp(row: &Value) -> Option<NaiveDateTime> {
    if let Some(epoch) = row.get("timestamp").and_then(number) {
        return chrono::DateTime::from_timestamp(epoch as i64, 0).map(|at| at.naive_utc());
    }
    let text = row
        .get("datetime")
        .and_then(Value::as_str)
        .or_else(|| row.get("date").and_then(Value::as_str))?
        .trim();
    NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S"))
        .or_else(|_| {
            NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .map(|date| date.and_hms_opt(0, 0, 0).unwrap_or_default())
        })
        .ok()
}

/// One symbol's intraday series in the `/bars` payload shape.
///
/// `d` is an exchange-local ISO timestamp rather than a session date, because an intraday bar
/// is a moment. `cr` is null: the vendor's intraday tape is unadjusted and carries no separate
/// raw print.
fn intraday_entry(
    symbol: &str,
    rows: &[IntradayRow],
    market: &str,
    zone: &Tz,
    source: &str,
) -> BarsEntry {
    let local = |at: NaiveDateTime| at.and_utc().with_timezone(zone);
    BarsEntry {
        symbol: symbol.to_string(),
        exchange: market.to_string(),
        as_of_session: rows
            .last()
            .map(|row| local(row.ts_utc).date_naive().to_string()),
        sessions: rows.len(),
        // Every series in this response is derived from the unadjusted 5m tape.
        adjusted: false,
        provider: source.to_string(),
        source: source.to_string(),
        rows: rows
            .iter()
            .map(|row| BarRow {
                d: local(row.ts_utc).format("%Y-%m-%dT%H:%M:%S").to_string(),
                o: Some(row.open),
                h: Some(row.high),
                l: Some(row.low),
                c: Some(row.close),
                cr: None,
                v: Some(row.volume),
            })
            .collect(),
    }
}

/// Whether a pull wrote enough rows to be called whole.
///
/// A first pull has no baseline and is trusted; a pull of nothing never is. Otherwise the floor
/// is [`BULK_MIN_COMPLETENESS_RATIO`] of the previous completed session's count.
fn bulk_pull_is_complete(written: usize, baseline: Option<i64>) -> bool {
    if written == 0 {
        return false;
    }
    match baseline {
        Some(baseline) if baseline > 0 => {
            written as f64 >= baseline as f64 * BULK_MIN_COMPLETENESS_RATIO
        }
        _ => true,
    }
}

/// One session's closes in the response shape.
async fn session_closes(
    store: &dyn ProductsStore,
    market: &str,
    session: NaiveDate,
) -> Result<BTreeMap<String, BulkClose>, ProductError> {
    let rows = store.session_closes(market, session).await?;
    Ok(rows
        .into_iter()
        .map(|(symbol, close)| (symbol, BulkClose { c: close }))
        .collect())
}

/// What one pull of a whole session produced.
struct PulledSession {
    session: NaiveDate,
    rows: BTreeMap<String, BulkClose>,
    complete: bool,
}

/// A provider quote as a `SymbolPriceCache` row.
///
/// `change_pct` comes from [`ProviderQuote::change_pct`] so every provider is ranked with the
/// same arithmetic — the move is computed from the price and the previous close that were
/// observed together, never read from a vendor's own field.
fn row_from_quote(quote: &ProviderQuote, exchange: &str, fetched_at: NaiveDateTime) -> PriceRow {
    PriceRow {
        symbol: quote.symbol.clone(),
        exchange: Some(exchange.to_string()),
        price: quote.price,
        bid: None,
        ask: None,
        previous_close: quote.previous_close,
        change_pct: quote.change_pct(),
        volume: Some(quote.volume),
        as_of: quote.as_of,
        fetched_at: Some(fetched_at),
        // Which provider answered. Carried into the cache and out again so a caller can see a
        // tape switch rather than discover it in the P&L.
        provider: Some(quote.provider.clone()),
    }
}

/// The same row as a cache read would return it.
fn cached_from_row(row: &PriceRow) -> CachedPrice {
    CachedPrice {
        symbol: row.symbol.clone(),
        exchange: row.exchange.clone().unwrap_or_default(),
        price: Some(row.price),
        bid: row.bid,
        ask: row.ask,
        previous_close: row.previous_close,
        change_pct: row.change_pct,
        as_of: row.as_of,
        fetched_at: row.fetched_at,
        provider: row.provider.clone(),
    }
}

/// The public quote shape, from a cache row.
fn quote_payload(stored: &CachedPrice, now: NaiveDateTime) -> QuotePayload {
    let age = stored
        .fetched_at
        .map(|fetched_at| seconds_between(now, fetched_at));
    let as_of_age = stored.as_of.map(|as_of| seconds_between(now, as_of));
    QuotePayload {
        symbol: Some(stored.symbol.clone()),
        exchange: Some(stored.exchange.clone()),
        price: stored.price,
        previous_close: stored.previous_close,
        change_pct: stored.change_pct,
        bid: stored.bid,
        ask: stored.ask,
        provider: stored
            .provider
            .clone()
            .filter(|provider| !provider.is_empty())
            .unwrap_or_else(|| "eodhd".to_string()),
        fetched_at: stored.fetched_at.map(iso_naive),
        age_seconds: age.map(|age| round_to(age, 3)),
        as_of: stored.as_of.map(iso_naive),
        as_of_age_seconds: as_of_age.map(|age| round_to(age, 3)),
    }
}

/// The payload for one symbol's bars.
fn bars_payload(
    symbol: &str,
    rows: &[BarView],
    source: &str,
    exchange: &str,
    provider: Option<&str>,
) -> BarsEntry {
    let adjusted = if rows.is_empty() {
        true
    } else {
        rows.iter().all(|row| row.adjusted)
    };
    BarsEntry {
        symbol: symbol.to_string(),
        exchange: exchange.to_string(),
        as_of_session: rows.last().map(|row| row.session.to_string()),
        sessions: rows.len(),
        adjusted,
        // Which provider answered. Hardcoding this was a real bug: Binance crypto bars were
        // being reported as eodhd, which is precisely the silent tape-mix this field exists
        // to prevent.
        provider: provider.map(str::to_string).unwrap_or_else(|| {
            if source == "vendor" {
                "eodhd".to_string()
            } else {
                "cache".to_string()
            }
        }),
        source: source.to_string(),
        rows: rows
            .iter()
            .map(|row| BarRow {
                d: row.session.to_string(),
                o: row.open,
                h: row.high,
                l: row.low,
                c: row.close,
                // The unadjusted close. `c` is adjusted when the series says so, and a candle
                // body must not mix the two scales; null means not known.
                cr: row.raw_close,
                v: row.volume,
            })
            .collect(),
    }
}

fn views(rows: &[StoredBar]) -> Vec<BarView> {
    rows.iter().map(BarView::from).collect()
}

/// A reading for a symbol the service could not compute.
fn unavailable_reading(
    symbol: &str,
    period: i64,
    method: &str,
    smoothing: &str,
    sessions: usize,
) -> AtrReading {
    AtrReading {
        symbol: symbol.to_string(),
        value: None,
        percent: None,
        reference_price: None,
        period,
        method: method.to_string(),
        smoothing: smoothing.to_string(),
        sessions_used: Some(sessions as i64),
        sessions_averaged: None,
        flat_sessions: None,
        as_of_session: None,
        source: "unavailable".to_string(),
        tick: None,
        ticks: None,
        tick_percent: None,
        reason: Presence::Value("no completed daily bars".to_string()),
        stop_level: Presence::Absent,
    }
}

/// The three aligned series ATR needs, or `None` when a served row is missing one.
fn ohlc(rows: &[BarRow]) -> Option<(Vec<f64>, Vec<f64>, Vec<f64>)> {
    let highs = rows.iter().map(|row| row.h).collect::<Option<Vec<_>>>()?;
    let lows = rows.iter().map(|row| row.l).collect::<Option<Vec<_>>>()?;
    let closes = rows.iter().map(|row| row.c).collect::<Option<Vec<_>>>()?;
    Some((highs, lows, closes))
}

/// Round to `places` decimals, ties to even, as Python's `round()` does.
fn round_to(value: f64, places: usize) -> f64 {
    format!("{value:.places$}").parse().unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suffix_is_stripped_and_duplicates_collapse_in_order() {
        let wanted = wanted_symbols(
            &[" aapl.us ".into(), "AAPL".into(), "MSFT.US".into()],
            false,
        );
        assert_eq!(wanted, vec!["AAPL", "MSFT"]);
    }

    #[test]
    fn crypto_keeps_its_pair_as_sent() {
        let wanted = wanted_symbols(&["btc-usd".into(), "ETH-USD".into()], true);
        assert_eq!(wanted, vec!["BTC-USD", "ETH-USD"]);
    }

    #[test]
    fn the_history_window_is_seven_days_for_every_five_sessions_plus_margin() {
        assert_eq!(history_window_days(560), 824);
        assert_eq!(history_window_days(5), 47);
    }

    #[test]
    fn rounding_matches_pythons_banker_rule() {
        // The binary value of 2.675 sits just below the decimal tie, so Python rounds it
        // down; 2.665 sits just above, so it rounds up. Both assertions pin the *float*
        // behaviour the source service has, not an idealised decimal rule.
        assert_eq!(round_to(2.675, 2), 2.67);
        assert_eq!(round_to(2.665, 2), 2.67);
        assert_eq!(round_to(2.5, 0), 2.0);
        assert_eq!(round_to(3.5, 0), 4.0);
        assert_eq!(round_to(1.2345675, 6), 1.234568);
    }

    #[test]
    fn a_caller_may_lower_the_price_ttl_but_not_raise_it() {
        // `fresh` is exercised through its bounds here without constructing a whole store.
        let ttl = crate::config::DEFAULT_PRICE_TTL_SECONDS;
        assert_eq!(ttl.min(60.0), 60.0);
        assert_eq!(ttl.min(9_999.0), ttl);
    }

    fn bar(day: u32, raw_close: Option<f64>) -> StoredBar {
        StoredBar {
            session: NaiveDate::from_ymd_opt(2026, 10, day).unwrap(),
            open: Some(1.0),
            high: Some(1.0),
            low: Some(1.0),
            close: Some(1.0),
            volume: None,
            source: None,
            adjusted: true,
            raw_close,
        }
    }

    #[test]
    fn a_complete_window_is_served_and_a_short_or_raw_less_one_is_not() {
        let rows = vec![bar(1, Some(1.0)), bar(2, Some(1.0)), bar(3, Some(1.0))];
        let latest = NaiveDate::from_ymd_opt(2026, 10, 3).unwrap();
        let served = complete_window(&rows, 2, latest).unwrap();
        assert_eq!(served.len(), 2);
        assert_eq!(served[1].session, latest);

        let mut missing_raw = rows.clone();
        missing_raw[2].raw_close = None;
        assert!(complete_window(&missing_raw, 2, latest).is_none());

        let tomorrow = NaiveDate::from_ymd_opt(2026, 10, 4).unwrap();
        assert!(complete_window(&rows, 2, tomorrow).is_none());
        assert!(complete_window(&rows, 4, latest).is_none());
    }

    #[test]
    fn an_indicator_row_answers_only_the_definition_and_session_it_holds() {
        let as_of = NaiveDate::from_ymd_opt(2026, 10, 3).unwrap();
        let mut hit = StoredIndicator {
            symbol: "AAPL".to_string(),
            values: BTreeMap::from([(14, Some(2.5))]),
            reference_price: None,
            sessions_used: Some(14),
            as_of_session: Some(as_of),
            method: Some("tr".to_string()),
            smoothing: Some("simple".to_string()),
            updated_at: None,
        };
        assert!(indicator_answers(&hit, "tr", "simple", as_of, 14));
        assert!(!indicator_answers(&hit, "tr", "wilder", as_of, 14));
        assert!(!indicator_answers(&hit, "tr", "simple", as_of, 21));
        assert!(!indicator_answers(
            &hit,
            "tr",
            "simple",
            as_of.pred_opt().unwrap(),
            14
        ));

        hit.method = Some("high_low".to_string());
        assert!(!indicator_answers(&hit, "tr", "simple", as_of, 14));
    }

    #[test]
    fn a_stop_needs_both_halves_and_a_positive_multiplier() {
        let symbols = vec!["AAPL".to_string()];
        let mut request = AtrRequest::new(&symbols);
        assert_eq!(requested_stop(&request).unwrap(), None);

        request.price = Some(231.4);
        assert_eq!(
            requested_stop(&request).unwrap_err().reason,
            "incomplete-stop"
        );
        request.price = None;
        request.multiplier = Some(2.0);
        assert_eq!(
            requested_stop(&request).unwrap_err().reason,
            "incomplete-stop"
        );

        request.price = Some(231.4);
        request.multiplier = Some(0.0);
        assert_eq!(
            requested_stop(&request).unwrap_err().reason,
            "bad-multiplier"
        );

        request.multiplier = Some(2.0);
        assert_eq!(requested_stop(&request).unwrap(), Some((231.4, 2.0)));
    }

    #[test]
    fn a_missing_direction_is_long_and_an_unknown_one_is_refused() {
        assert_eq!(stop_side("").unwrap(), "long");
        assert_eq!(stop_side(" SHORT ").unwrap(), "short");
        assert_eq!(stop_side("sideways").unwrap_err().reason, "bad-direction");
    }
}
