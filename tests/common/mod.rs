//! Fakes shared by the products and refresh suites.
//!
//! Nothing here touches a database, a keyring or a vendor: the products layer takes its
//! repositories, its router and its clock as arguments, so every cache decision can be pinned
//! against in-memory state. That is the same rule the source suite followed — these tests
//! spend no EODHD quota.
//!
//! Each fake mirrors the production contract rather than a convenient simplification:
//!
//! * [`FakeStore`] keys bars and indicators by `(exchange, symbol)`, because `CBA` at the ASX
//!   and `CBA` in New York are different instruments. Its daily upsert applies the same
//!   `COALESCE(raw_close, existing)` rule the SQL `MERGE` does, so "the backfill runs once"
//!   is a real assertion rather than an artefact of the fake.
//! * [`FakeProvider`] answers through the real [`ProviderRouter`], so rejection, fallback and
//!   provenance are exercised rather than bypassed.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};

use mmds::clock::Clock;
use mmds::config::{NoKeyring, Settings};
use mmds::domain::asset_class::AssetClass;
use mmds::error::ProviderFailure;
use mmds::jobs::daily::{PullSummary, RebuildSummary};
use mmds::products::Products;
use mmds::providers::base::{Provider, ProviderBar, ProviderQuote, WholeMarketQuotes};
use mmds::providers::router::ProviderRouter;
use mmds::refresh::{bulk_request_type, RefreshFailure, RefreshJobs, RefreshStore, RefreshVendor};
use mmds::store::models::{
    CachedPrice, DailyBarWrite, IndicatorWrite, Movers, PriceRow, SeriesKey, SessionSymbolCount,
    StoredBar, StoredIndicator, UsageSummary,
};
use mmds::store::{
    DailyBarRepository, IndicatorRepository, PriceRepository, StoreError, UsageRepository,
};
use mmds::usage::TariffRow;

/// A UTC instant, for a fixed [`TestClock`].
pub fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
        .expect("the test instant is a real UTC time")
}

/// A calendar date, for seeded history.
pub fn date(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).expect("the test date is real")
}

/// The instant the products suite is pinned at.
///
/// New York has settled the 2026-10-29 session by this time (19:00 ET + 3 h), so
/// [`latest_completed_session`] answers 2026-10-29 for the US without relying on the wall
/// clock or on a monkeypatched module global.
pub fn pinned_clock() -> Arc<TestClock> {
    Arc::new(TestClock::new(utc(2026, 10, 30, 0, 0)))
}

/// A clock a test can move forward.
pub struct TestClock {
    now: Mutex<DateTime<Utc>>,
}

impl TestClock {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self {
            now: Mutex::new(now),
        }
    }

    pub fn advance_seconds(&self, seconds: i64) {
        let mut now = self
            .now
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *now += Duration::seconds(seconds);
    }

    pub fn advance_days(&self, days: i64) {
        self.advance_seconds(days * 86_400);
    }
}

impl Clock for TestClock {
    fn now_utc(&self) -> DateTime<Utc> {
        *self
            .now
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Settings with every default and no key, so [`Products::new`] never builds a vendor client.
pub fn test_settings() -> Settings {
    Settings::from_lookup(&|_name: &str| None, None, &NoKeyring)
}

/// The products layer over the fakes, with no vendor client and no live provider.
pub async fn products(
    store: Arc<FakeStore>,
    provider: &Arc<FakeProvider>,
    clock: Arc<TestClock>,
) -> Products {
    Products::new(
        test_settings(),
        store,
        None,
        Some(router_with(provider)),
        clock,
    )
    .await
    .expect("the products layer builds over the fakes")
}

/// A router holding only the fake, so no request can leave the process.
pub fn router_with(provider: &Arc<FakeProvider>) -> Arc<ProviderRouter> {
    let answering: Arc<dyn Provider> = provider.clone();
    let whole_market: Arc<dyn WholeMarketQuotes> = provider.clone();
    Arc::new(ProviderRouter::new(vec![answering]).with_whole_market(whole_market))
}

/// In-memory repositories, keyed the way the SQL tables are.
#[derive(Default)]
pub struct FakeStore {
    state: Mutex<State>,
    bulk_counts: Mutex<HashMap<String, i64>>,
    ledger_fails: AtomicBool,
}

#[derive(Default)]
struct State {
    prices: HashMap<(String, String), PriceRow>,
    price_writes: usize,
    daily: HashMap<(String, String), BTreeMap<NaiveDate, DailyBarWrite>>,
    daily_writes: usize,
    indicators: HashMap<(String, String), StoredIndicator>,
    indicator_writes: Vec<IndicatorWrite>,
    tariff: Vec<TariffRow>,
    usage: Vec<RecordedUsage>,
}

/// One row the ledger was asked to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedUsage {
    pub request_type: String,
    pub caller: String,
    pub calls: i64,
    pub symbols: i64,
    pub cost: i64,
}

impl FakeStore {
    pub fn new() -> Self {
        let state = State {
            tariff: vec![
                tariff_row("/real-time/", "per_symbol", Some(1)),
                tariff_row("/real-time?ex=US", "flat", Some(100)),
                tariff_row("/eod/", "per_call", Some(1)),
                tariff_row("/eod-bulk-last-day/US", "flat", Some(100)),
                tariff_row("/eod-bulk-last-day/AU", "flat", Some(100)),
            ],
            ..State::default()
        };
        Self {
            state: Mutex::new(state),
            bulk_counts: Mutex::new(HashMap::new()),
            ledger_fails: AtomicBool::new(false),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Stand in for "a pull is already recorded today" without fabricating a price row.
    pub fn seed_pull(&self, market: &str, calls: i64) {
        let mut counts = self
            .bulk_counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        counts.insert(bulk_request_type(market), calls);
    }

    /// Make the usage ledger unreadable, the way a dropped connection would.
    pub fn fail_ledger(&self, fail: bool) {
        self.ledger_fails.store(fail, Ordering::Relaxed);
    }

    pub fn price_writes(&self) -> usize {
        self.state().price_writes
    }

    pub fn daily_writes(&self) -> usize {
        self.state().daily_writes
    }

    pub fn indicator_writes(&self) -> Vec<IndicatorWrite> {
        self.state().indicator_writes.clone()
    }

    pub fn has_price(&self, symbol: &str, venue: &str) -> bool {
        self.state()
            .prices
            .contains_key(&(symbol.to_string(), venue.to_string()))
    }

    /// Age a cached price, so a caller's lower TTL can be shown to matter.
    pub fn backdate_price(&self, symbol: &str, venue: &str, fetched_at: NaiveDateTime) {
        let mut state = self.state();
        if let Some(row) = state
            .prices
            .get_mut(&(symbol.to_string(), venue.to_string()))
        {
            row.fetched_at = Some(fetched_at);
        }
    }

    /// Age every price at a venue, which leaves them stale while the in-memory whole-market
    /// snapshot is still inside its own lifetime.
    pub fn backdate_venue_prices(&self, venue: &str, fetched_at: NaiveDateTime) {
        let mut state = self.state();
        for ((_, row_venue), row) in state.prices.iter_mut() {
            if row_venue == venue {
                row.fetched_at = Some(fetched_at);
            }
        }
    }

    /// Put daily bars into the store without counting them as a write, as a previous process
    /// would have left them.
    pub fn seed_daily(&self, exchange: &str, symbol: &str, rows: Vec<DailyBarWrite>) {
        let mut state = self.state();
        let bucket = state
            .daily
            .entry((exchange.to_string(), symbol.to_string()))
            .or_default();
        for row in rows {
            bucket.insert(row.session, row);
        }
    }

    /// `sessions` calendar-daily rows ending at `newest`, oldest first.
    pub fn seed_history(&self, exchange: &str, symbol: &str, sessions: usize, newest: NaiveDate) {
        let rows = (0..sessions)
            .map(|offset| {
                let session = newest - Duration::days(i64::try_from(offset).unwrap_or(i64::MAX));
                DailyBarWrite {
                    symbol: symbol.to_string(),
                    exchange: Some(exchange.to_string()),
                    session,
                    open: 100.0,
                    high: 101.0,
                    low: 99.0,
                    close: 100.5,
                    raw_close: Some(100.6),
                    volume: Some(1_000),
                    source: Some("seed".to_string()),
                    adjusted: Some(true),
                }
            })
            .collect();
        self.seed_daily(exchange, symbol, rows);
    }

    pub fn daily_rows(&self, exchange: &str, symbol: &str) -> Vec<DailyBarWrite> {
        let state = self.state();
        state
            .daily
            .get(&(exchange.to_string(), symbol.to_string()))
            .map(|bucket| bucket.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn newest_daily_session(&self, exchange: &str, symbol: &str) -> Option<NaiveDate> {
        let state = self.state();
        state
            .daily
            .get(&(exchange.to_string(), symbol.to_string()))
            .and_then(|bucket| bucket.keys().next_back().copied())
    }

    /// The venues a symbol's daily rows were filed under.
    pub fn daily_exchanges(&self, symbol: &str) -> Vec<String> {
        let state = self.state();
        let mut exchanges: Vec<String> = state
            .daily
            .keys()
            .filter(|(_, row_symbol)| row_symbol == symbol)
            .map(|(exchange, _)| exchange.clone())
            .collect();
        exchanges.sort();
        exchanges.dedup();
        exchanges
    }

    /// Remove every raw close, modelling rows written before the column existed.
    pub fn strip_raw_closes(&self, exchange: &str, symbol: &str) {
        let mut state = self.state();
        if let Some(bucket) = state
            .daily
            .get_mut(&(exchange.to_string(), symbol.to_string()))
        {
            for row in bucket.values_mut() {
                row.raw_close = None;
            }
        }
    }

    /// The venue the latest indicator write for a symbol carried.
    pub fn indicator_exchange(&self, symbol: &str) -> Option<String> {
        self.state()
            .indicator_writes
            .iter()
            .rev()
            .find(|write| write.symbol == symbol)
            .and_then(|write| write.exchange.clone())
    }

    pub fn recorded_usage(&self) -> Vec<RecordedUsage> {
        self.state().usage.clone()
    }
}

fn tariff_row(request_type: &str, basis: &str, each: Option<i64>) -> TariffRow {
    TariffRow {
        request_type: request_type.to_string(),
        cost_basis: Some(basis.to_string()),
        cost_each: each,
    }
}

fn stored_bar(row: &DailyBarWrite) -> StoredBar {
    StoredBar {
        session: row.session,
        open: Some(row.open),
        high: Some(row.high),
        low: Some(row.low),
        close: Some(row.close),
        volume: row.volume,
        source: row.source.clone(),
        adjusted: row.adjusted.unwrap_or(true),
        raw_close: row.raw_close,
    }
}

fn cached_price(row: &PriceRow) -> CachedPrice {
    CachedPrice {
        symbol: row.symbol.clone(),
        venue: row.venue.clone().unwrap_or_default(),
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

#[async_trait]
impl PriceRepository for FakeStore {
    async fn get_prices(
        &self,
        symbols: &[String],
        venue: &str,
    ) -> Result<HashMap<String, CachedPrice>, StoreError> {
        let state = self.state();
        let mut out = HashMap::with_capacity(symbols.len());
        for symbol in symbols {
            let key = (symbol.trim().to_uppercase(), venue.to_string());
            if let Some(row) = state.prices.get(&key) {
                out.insert(key.0, cached_price(row));
            }
        }
        Ok(out)
    }

    async fn upsert_prices(&self, rows: Vec<PriceRow>) -> Result<usize, StoreError> {
        let mut state = self.state();
        let written = rows.len();
        for row in rows {
            let venue = row.venue.clone().unwrap_or_default();
            state.prices.insert((row.symbol.clone(), venue), row);
        }
        state.price_writes += written;
        Ok(written)
    }

    async fn big_movers(
        &self,
        _venue: &str,
        _limit: i64,
        _min_price: f64,
        _min_dollar_volume: f64,
        _since: Option<NaiveDateTime>,
    ) -> Result<Movers, StoreError> {
        Ok(Movers::default())
    }

    async fn latest_venue_price(&self, venue: &str) -> Result<Option<NaiveDateTime>, StoreError> {
        let state = self.state();
        Ok(state
            .prices
            .iter()
            .filter(|((_, row_venue), _)| row_venue == venue)
            .filter_map(|(_, row)| row.fetched_at)
            .max())
    }

    async fn top_symbols_by_dollar_volume(
        &self,
        _venue: &str,
        _limit: i64,
        _min_breadth: i64,
    ) -> Result<Vec<String>, StoreError> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl DailyBarRepository for FakeStore {
    async fn get_daily_bars(
        &self,
        symbol: &str,
        sessions: i64,
        exchange: &str,
    ) -> Result<Vec<StoredBar>, StoreError> {
        let mut rows = self
            .get_daily_bars_many(&[symbol.to_string()], sessions, exchange)
            .await?;
        Ok(rows
            .remove(&symbol.trim().to_uppercase())
            .unwrap_or_default())
    }

    async fn get_daily_bars_many(
        &self,
        symbols: &[String],
        sessions: i64,
        exchange: &str,
    ) -> Result<HashMap<String, Vec<StoredBar>>, StoreError> {
        let state = self.state();
        let take = usize::try_from(sessions.max(0)).unwrap_or(usize::MAX);
        let mut out = HashMap::with_capacity(symbols.len());
        for symbol in symbols {
            let key = symbol.trim().to_uppercase();
            let rows = state
                .daily
                .get(&(exchange.to_string(), key.clone()))
                .map(|bucket| {
                    let all: Vec<StoredBar> = bucket.values().map(stored_bar).collect();
                    let start = all.len().saturating_sub(take);
                    all[start..].to_vec()
                })
                .unwrap_or_default();
            out.insert(key, rows);
        }
        Ok(out)
    }

    async fn daily_bar_depth(
        &self,
        symbols: &[String],
        exchange: &str,
        since: Option<NaiveDate>,
    ) -> Result<HashMap<String, i64>, StoreError> {
        let state = self.state();
        let mut out = HashMap::with_capacity(symbols.len());
        for symbol in symbols {
            let key = symbol.trim().to_uppercase();
            let held = state
                .daily
                .get(&(exchange.to_string(), key.clone()))
                .map(|bucket| {
                    bucket
                        .keys()
                        .filter(|session| since.is_none_or(|since| **session >= since))
                        .count()
                })
                .unwrap_or(0);
            out.insert(key, i64::try_from(held).unwrap_or(i64::MAX));
        }
        Ok(out)
    }

    async fn upsert_daily(&self, rows: Vec<DailyBarWrite>) -> Result<usize, StoreError> {
        let mut state = self.state();
        let written = rows.len();
        for mut row in rows {
            let exchange = row.exchange.clone().unwrap_or_default();
            let bucket = state
                .daily
                .entry((exchange, row.symbol.clone()))
                .or_default();
            // The SQL MERGE keeps a known RawClose when the writer's is NULL, so a BarHourly
            // seed cannot erase the price the vendor collected.
            if let Some(existing) = bucket.get(&row.session) {
                if row.raw_close.is_none() {
                    row.raw_close = existing.raw_close;
                }
            }
            bucket.insert(row.session, row);
        }
        state.daily_writes += written;
        Ok(written)
    }

    async fn latest_session(&self) -> Result<Option<NaiveDate>, StoreError> {
        let state = self.state();
        Ok(state
            .daily
            .values()
            .filter_map(|bucket| bucket.keys().next_back().copied())
            .max())
    }

    async fn session_symbol_counts(
        &self,
        limit: i64,
    ) -> Result<Vec<SessionSymbolCount>, StoreError> {
        let state = self.state();
        let mut counts: BTreeMap<(NaiveDate, String), i64> = BTreeMap::new();
        for ((exchange, _), bucket) in &state.daily {
            for session in bucket.keys() {
                *counts.entry((*session, exchange.clone())).or_insert(0) += 1;
            }
        }
        let limit = usize::try_from(limit.max(0)).unwrap_or(usize::MAX);
        let mut rows: Vec<SessionSymbolCount> = counts
            .into_iter()
            .rev()
            .take(limit)
            .map(|((session, exchange), symbols)| SessionSymbolCount {
                session,
                exchange,
                symbols,
            })
            .collect();
        rows.sort_by_key(|left| std::cmp::Reverse(left.session));
        Ok(rows)
    }

    async fn symbols_with_min_sessions(&self, minimum: i64) -> Result<Vec<SeriesKey>, StoreError> {
        let state = self.state();
        let mut keys: Vec<SeriesKey> = state
            .daily
            .iter()
            .filter(|(_, bucket)| i64::try_from(bucket.len()).unwrap_or(i64::MAX) >= minimum)
            .map(|((exchange, symbol), _)| SeriesKey {
                symbol: symbol.clone(),
                exchange: exchange.clone(),
            })
            .collect();
        keys.sort();
        Ok(keys)
    }
}

#[async_trait]
impl IndicatorRepository for FakeStore {
    async fn get_indicators(
        &self,
        symbols: &[String],
        exchange: &str,
    ) -> Result<HashMap<String, StoredIndicator>, StoreError> {
        let state = self.state();
        let mut out = HashMap::with_capacity(symbols.len());
        for symbol in symbols {
            let key = symbol.trim().to_uppercase();
            if let Some(row) = state.indicators.get(&(exchange.to_string(), key.clone())) {
                out.insert(key, row.clone());
            }
        }
        Ok(out)
    }

    async fn upsert_indicators(&self, rows: Vec<IndicatorWrite>) -> Result<usize, StoreError> {
        let mut state = self.state();
        let written = rows.len();
        for row in rows {
            let exchange = row.exchange.clone().unwrap_or_default();
            state.indicators.insert(
                (exchange, row.symbol.clone()),
                StoredIndicator {
                    symbol: row.symbol.clone(),
                    values: row.values.clone(),
                    reference_price: row.reference_price,
                    sessions_used: row.sessions_used,
                    as_of_session: row.as_of_session,
                    method: row.method.clone(),
                    smoothing: row.smoothing.clone(),
                    updated_at: None,
                },
            );
            state.indicator_writes.push(row);
        }
        Ok(written)
    }
}

#[async_trait]
impl UsageRepository for FakeStore {
    async fn request_types(&self) -> Result<Vec<TariffRow>, StoreError> {
        Ok(self.state().tariff.clone())
    }

    async fn record_usage(
        &self,
        request_type: &str,
        caller: &str,
        calls: i64,
        symbols: i64,
        cost: i64,
        _usage_date: Option<NaiveDate>,
    ) -> Result<(), StoreError> {
        self.state().usage.push(RecordedUsage {
            request_type: request_type.to_string(),
            caller: caller.to_string(),
            calls,
            symbols,
            cost,
        });
        let mut counts = self
            .bulk_counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *counts.entry(request_type.to_string()).or_insert(0) += calls;
        Ok(())
    }

    async fn bulk_pull_count(
        &self,
        request_type: &str,
        _usage_date: Option<NaiveDate>,
    ) -> Result<i64, StoreError> {
        if self.ledger_fails.load(Ordering::Relaxed) {
            return Err(StoreError::database("ledger unavailable"));
        }
        let counts = self
            .bulk_counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(*counts.get(request_type).unwrap_or(&0))
    }

    async fn usage_summary(
        &self,
        usage_date: Option<NaiveDate>,
    ) -> Result<UsageSummary, StoreError> {
        Ok(UsageSummary {
            date: usage_date.unwrap_or_else(|| date(2026, 10, 29)),
            calls: 0,
            symbols: 0,
            cost: 0,
            rows: 0,
            by_request_type: Vec::new(),
            by_caller: Vec::new(),
        })
    }
}

/// One provider and one whole-market feed, both counted.
pub struct FakeProvider {
    name: &'static str,
    quote_calls: AtomicUsize,
    bars_calls: AtomicUsize,
    whole_market_calls: AtomicUsize,
    quote_failure: AtomicBool,
    bars_failure: AtomicBool,
    bars_per_symbol: AtomicUsize,
    newest: Mutex<NaiveDate>,
    bars_widths: Mutex<Vec<usize>>,
    whole_market: Mutex<Vec<ProviderQuote>>,
    quote_price: f64,
    previous_close: f64,
    as_of: NaiveDateTime,
}

impl FakeProvider {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            quote_calls: AtomicUsize::new(0),
            bars_calls: AtomicUsize::new(0),
            whole_market_calls: AtomicUsize::new(0),
            quote_failure: AtomicBool::new(false),
            bars_failure: AtomicBool::new(false),
            bars_per_symbol: AtomicUsize::new(40),
            newest: Mutex::new(date(2026, 10, 27)),
            bars_widths: Mutex::new(Vec::new()),
            whole_market: Mutex::new(Vec::new()),
            quote_price: 100.0,
            previous_close: 98.0,
            as_of: date(2026, 10, 29)
                .and_hms_opt(20, 0, 0)
                .expect("the observation time is real"),
        }
    }

    pub fn set_quote_failure(&self, fail: bool) {
        self.quote_failure.store(fail, Ordering::Relaxed);
    }

    pub fn set_bars_failure(&self, fail: bool) {
        self.bars_failure.store(fail, Ordering::Relaxed);
    }

    pub fn set_bars_per_symbol(&self, count: usize) {
        self.bars_per_symbol.store(count, Ordering::Relaxed);
    }

    /// The newest session every bar answer will carry.
    pub fn set_newest(&self, newest: NaiveDate) {
        *self
            .newest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = newest;
    }

    pub fn set_whole_market(&self, quotes: Vec<ProviderQuote>) {
        *self
            .whole_market
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = quotes;
    }

    pub fn quote_calls(&self) -> usize {
        self.quote_calls.load(Ordering::Relaxed)
    }

    pub fn bars_calls(&self) -> usize {
        self.bars_calls.load(Ordering::Relaxed)
    }

    pub fn whole_market_calls(&self) -> usize {
        self.whole_market_calls.load(Ordering::Relaxed)
    }

    /// The session count each `/eod/` call asked for. The vendor bills one unit whatever the
    /// width, so this is the only visible evidence of the history policy.
    pub fn bars_widths(&self) -> Vec<usize> {
        self.bars_widths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn series(&self) -> Vec<ProviderBar> {
        let count = self.bars_per_symbol.load(Ordering::Relaxed);
        let newest = *self
            .newest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (0..count)
            .map(|index| {
                let offset = i64::try_from(count - 1 - index).unwrap_or(i64::MAX);
                let base = 100.0 + index as f64;
                ProviderBar {
                    session: newest - Duration::days(offset),
                    open: base,
                    high: base + 2.0,
                    low: base - 2.0,
                    // Deliberately two scales: a payload that echoed one close for both
                    // fields could not tell adjusted from raw.
                    close: base + 0.4,
                    volume: 1_000,
                    adjusted: true,
                    raw_close: Some(base + 0.5),
                }
            })
            .collect()
    }
}

#[async_trait]
impl Provider for FakeProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supports(&self, _asset_class: AssetClass) -> bool {
        true
    }

    async fn quote(
        &self,
        symbol: &str,
        _asset_class: AssetClass,
        _venue: Option<&str>,
    ) -> Result<Option<ProviderQuote>, ProviderFailure> {
        self.quote_calls.fetch_add(1, Ordering::Relaxed);
        if self.quote_failure.load(Ordering::Relaxed) {
            return Err(ProviderFailure::new("http-error", "404", Some(404)));
        }
        Ok(Some(ProviderQuote {
            symbol: symbol.to_string(),
            price: self.quote_price,
            provider: self.name.to_string(),
            previous_close: Some(self.previous_close),
            as_of: Some(self.as_of),
            currency: None,
            raw_ticker: Some(format!("{symbol}.US")),
            volume: 1_000,
        }))
    }

    async fn bars(
        &self,
        _symbol: &str,
        _asset_class: AssetClass,
        sessions: usize,
        _venue: Option<&str>,
    ) -> Result<Vec<ProviderBar>, ProviderFailure> {
        self.bars_calls.fetch_add(1, Ordering::Relaxed);
        self.bars_widths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(sessions);
        if self.bars_failure.load(Ordering::Relaxed) {
            return Err(ProviderFailure::new("http-error", "500", Some(500)));
        }
        Ok(self.series())
    }
}

#[async_trait]
impl WholeMarketQuotes for FakeProvider {
    async fn whole_market_quotes(
        &self,
        _venue: Option<&str>,
    ) -> Result<Vec<ProviderQuote>, ProviderFailure> {
        self.whole_market_calls.fetch_add(1, Ordering::Relaxed);
        Ok(self
            .whole_market
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone())
    }
}

/// `count` whole-market rows named `SYM0000`.., all priced alike.
pub fn bulk_quotes(count: usize) -> Vec<ProviderQuote> {
    let as_of = date(2026, 10, 29)
        .and_hms_opt(20, 0, 0)
        .expect("the observation time is real");
    (0..count)
        .map(|index| ProviderQuote {
            symbol: format!("SYM{index:04}"),
            price: 100.0,
            provider: "eodhd".to_string(),
            previous_close: Some(98.0),
            as_of: Some(as_of),
            currency: None,
            raw_ticker: None,
            volume: 1_000,
        })
        .collect()
}

/// A pull that spends the units a vendor call would.
pub struct FakeVendor {
    units: Arc<AtomicI64>,
    bulk_calls: AtomicUsize,
}

impl FakeVendor {
    pub fn new(units: Arc<AtomicI64>) -> Self {
        Self {
            units,
            bulk_calls: AtomicUsize::new(0),
        }
    }

    pub fn units(&self) -> Arc<AtomicI64> {
        self.units.clone()
    }

    pub fn bulk_calls(&self) -> usize {
        self.bulk_calls.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl RefreshVendor for FakeVendor {
    async fn bulk_last_day(
        &self,
        _market: &str,
        _session_date: Option<NaiveDate>,
    ) -> Result<Vec<serde_json::Value>, ProviderFailure> {
        self.bulk_calls.fetch_add(1, Ordering::Relaxed);
        Ok(Vec::new())
    }

    fn units_spent(&self) -> i64 {
        self.units.load(Ordering::Relaxed)
    }
}

/// The pull and rebuild the refresh policy calls, recording what they were asked for.
pub struct FakeJobs {
    pull_result: Mutex<Result<PullSummary, RefreshFailure>>,
    rebuild_result: Mutex<Result<RebuildSummary, RefreshFailure>>,
    units: Option<Arc<AtomicI64>>,
    units_to_spend: i64,
    pulls: AtomicUsize,
    rebuilds: AtomicUsize,
    dates: Mutex<Option<Vec<NaiveDate>>>,
}

impl FakeJobs {
    pub fn new() -> Self {
        Self {
            pull_result: Mutex::new(Ok(PullSummary::default())),
            rebuild_result: Mutex::new(Ok(RebuildSummary::default())),
            units: None,
            units_to_spend: 0,
            pulls: AtomicUsize::new(0),
            rebuilds: AtomicUsize::new(0),
            dates: Mutex::new(None),
        }
    }

    /// A pull of one session, and how many units the vendor counter moves during it.
    pub fn with_pull(mut self, summary: PullSummary, units: Arc<AtomicI64>, spend: i64) -> Self {
        self.pull_result = Mutex::new(Ok(summary));
        self.units = Some(units);
        self.units_to_spend = spend;
        self
    }

    pub fn failing(mut self, failure: RefreshFailure) -> Self {
        self.pull_result = Mutex::new(Err(failure));
        self
    }

    pub fn pulls(&self) -> usize {
        self.pulls.load(Ordering::Relaxed)
    }

    pub fn rebuilds(&self) -> usize {
        self.rebuilds.load(Ordering::Relaxed)
    }

    pub fn dates(&self) -> Option<Vec<NaiveDate>> {
        self.dates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Default for FakeJobs {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RefreshJobs for FakeJobs {
    async fn pull(
        &self,
        _store: &dyn RefreshStore,
        _vendor: &dyn RefreshVendor,
        _market: &str,
        dates: Option<Vec<NaiveDate>>,
    ) -> Result<PullSummary, RefreshFailure> {
        self.pulls.fetch_add(1, Ordering::Relaxed);
        *self
            .dates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = dates;
        if let Some(units) = &self.units {
            units.fetch_add(self.units_to_spend, Ordering::Relaxed);
        }
        self.pull_result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    async fn rebuild_indicators(
        &self,
        _store: &dyn RefreshStore,
        _settings: &Settings,
    ) -> Result<RebuildSummary, RefreshFailure> {
        self.rebuilds.fetch_add(1, Ordering::Relaxed);
        self.rebuild_result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}
