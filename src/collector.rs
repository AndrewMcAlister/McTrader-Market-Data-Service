//! The production market run: what the scheduler and `POST /schedule/run` actually do.
//!
//! One collection, shaped exactly like the source service's `_build_schedule.run_market`:
//!
//! 1. refuse an unknown market, then refuse a closed venue — a manual run obeys the same
//!    session guard as the schedule, or the guard is decorative;
//! 2. pull the settled session into `DailySnapshot` through the one refresh implementation
//!    `/refresh` also uses;
//! 3. put live prices into `SymbolPriceCache`: one whole-market snapshot for the US (the only
//!    venue EODHD publishes one for), or a bounded top-of-book universe priced per symbol
//!    anywhere else;
//! 4. report whether today's data is *now available*, which is the question the retry exists
//!    to answer — "the refresh ran" is not the same claim.
//!
//! The runner is total: a store or vendor failure becomes an `ok = false` result rather than
//! an error escaping into the scheduler loop, because one bad run must not stop the clock.
//! The source service's scheduler caught the same exception around `run_market`; that catch
//! lives here now, which also lets a manual run answer rather than raise.
//!
//! # The two seams
//!
//! [`CollectionProducts`] is the product layer's collection capabilities, below the HTTP
//! layer, so this module never imports `api`. [`DailyRefresh`] is the daily pull itself; the
//! production implementation is [`StoreRefresh`], and its only reason to exist is that a test
//! of the run's shaping needs neither a store nor a vendor.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use crate::clock::{iso_utc, Clock};
use crate::config::Settings;
use crate::domain::availability::{check, country_key, Availability, ExchangeSession};
use crate::error::py_repr;
use crate::jobs::daily::LibraryJobs;
use crate::products::{PricesResponse, ProductError, Products};
use crate::refresh::{
    refresh_daily_bars, RefreshFailure, RefreshGate, RefreshJobs, RefreshOutcome, RefreshRequest,
    RefreshStatus, RefreshStore, RefreshVendor,
};
use crate::schedule::{MarketRun, MarketRunner, MarketWindow, SessionSchedule, MARKETS};
use crate::store::prices::PriceRepository;

/// The breadth floor the derived universe uses, in settled sessions.
///
/// The same fifty the source service's `top_symbols_by_dollar_volume` defaulted to: a name
/// seen on fewer sessions has no stable dollar-volume rank to seed a live price pass from.
const UNIVERSE_MIN_BREADTH: i64 = 50;

/// The product operations a market run needs, below the HTTP layer.
///
/// [`crate::api::layer::ProductLayer`] extends this, so the API's `Arc<dyn ProductLayer>` is
/// handed to the collector unchanged and both sides see one product layer, not two.
#[async_trait]
pub trait CollectionProducts: Send + Sync {
    /// Write live prices for the whole US market. The only venue with such a feed.
    async fn refresh_snapshot(&self, venue: &str) -> Result<usize, ProductError>;

    /// Price a bounded symbol set at one venue through the ordinary provider router.
    async fn prices(
        &self,
        symbols: &[String],
        venue: Option<&str>,
        max_age_seconds: Option<f64>,
        asset_class: Option<&str>,
        country_code: Option<&str>,
    ) -> Result<PricesResponse, ProductError>;

    /// The vendor client the daily pull spends units through.
    fn vendor(&self) -> Option<Arc<dyn RefreshVendor>>;
}

#[async_trait]
impl CollectionProducts for Products {
    async fn refresh_snapshot(&self, venue: &str) -> Result<usize, ProductError> {
        Products::refresh_snapshot(self, venue).await
    }

    async fn prices(
        &self,
        symbols: &[String],
        venue: Option<&str>,
        max_age_seconds: Option<f64>,
        asset_class: Option<&str>,
        country_code: Option<&str>,
    ) -> Result<PricesResponse, ProductError> {
        Products::prices(
            self,
            symbols,
            venue,
            max_age_seconds,
            asset_class,
            country_code,
        )
        .await
    }

    fn vendor(&self) -> Option<Arc<dyn RefreshVendor>> {
        self.client()
            .map(|client| client.clone() as Arc<dyn RefreshVendor>)
    }
}

/// The daily pull a market run performs, as a seam.
#[async_trait]
pub trait DailyRefresh: Send + Sync {
    async fn refresh(&self, market: &str) -> Result<RefreshOutcome, RefreshFailure>;
}

/// The production pull: [`refresh_daily_bars`] over the process's store, vendor and gate.
pub struct StoreRefresh {
    store: Arc<dyn RefreshStore>,
    vendor: Option<Arc<dyn RefreshVendor>>,
    jobs: Arc<dyn RefreshJobs>,
    settings: Settings,
    gate: Arc<RefreshGate>,
    clock: Arc<dyn Clock>,
}

impl StoreRefresh {
    pub fn new(
        store: Arc<dyn RefreshStore>,
        vendor: Option<Arc<dyn RefreshVendor>>,
        settings: Settings,
        gate: Arc<RefreshGate>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            vendor,
            jobs: Arc::new(LibraryJobs),
            settings,
            gate,
            clock,
        }
    }
}

#[async_trait]
impl DailyRefresh for StoreRefresh {
    async fn refresh(&self, market: &str) -> Result<RefreshOutcome, RefreshFailure> {
        refresh_daily_bars(
            self.store.as_ref(),
            self.vendor.as_deref(),
            self.jobs.as_ref(),
            &self.settings,
            &self.gate,
            self.clock.as_ref(),
            RefreshRequest {
                market: market.to_string(),
                session_date: None,
                force: false,
                include_indicators: true,
            },
        )
        .await
    }
}

/// One market's collection: the daily pull, then its venue's price feed.
pub struct Collector {
    products: Arc<dyn CollectionProducts>,
    prices: Arc<dyn PriceRepository>,
    refresh: Arc<dyn DailyRefresh>,
    clock: Arc<dyn Clock>,
    schedule: Arc<SessionSchedule>,
    universe_limit: i64,
}

impl Collector {
    pub fn new(
        products: Arc<dyn CollectionProducts>,
        prices: Arc<dyn PriceRepository>,
        refresh: Arc<dyn DailyRefresh>,
        clock: Arc<dyn Clock>,
        schedule: Arc<SessionSchedule>,
        universe_limit: i64,
    ) -> Self {
        Self {
            products,
            prices,
            refresh,
            clock,
            schedule,
            universe_limit,
        }
    }

    /// Price a venue's most-traded universe and answer the availability question.
    ///
    /// A venue with no whole-market real-time feed has no other way to put a post-open
    /// observation into `SymbolPriceCache`: EODHD publishes that feed for the US only. The
    /// seed is the top of the last settled session by dollar volume, so it needs no configured
    /// list and is the set the books actually rank. A name the vendor cannot price is reported
    /// in `missing` rather than making the venue's data absent.
    async fn price_universe(&self, window: MarketWindow, now: DateTime<Utc>) -> Value {
        let venue = window.market;
        let symbols = match self
            .prices
            .top_symbols_by_dollar_volume(venue, self.universe_limit, UNIVERSE_MIN_BREADTH)
            .await
        {
            Ok(symbols) => symbols,
            Err(error) => return pricing_error(venue, format!("StoreError: {error}")),
        };
        if symbols.is_empty() {
            return json!({
                "venue": venue,
                "universe": 0,
                "available": false,
                "reason": "no-universe",
            });
        }
        let batch = match self
            .products
            .prices(&symbols, Some(venue), None, None, None)
            .await
        {
            Ok(batch) => batch,
            Err(error) => return pricing_error(venue, error.to_string()),
        };
        let verdict = match self.availability(window, now).await {
            Ok(verdict) => verdict,
            Err(detail) => return pricing_error(venue, detail),
        };
        // `count` is every symbol with a quote, cache hits included; `fetched` is only the
        // names this call bought. A retry inside the five-minute TTL prices the universe with
        // fetched=0, and reporting that as "priced 0" would read as a failure when it is the
        // opposite.
        json!({
            "venue": venue,
            "universe": symbols.len(),
            "requested": batch.requested,
            "priced": batch.count,
            "fetched": batch.fetched.len(),
            "missing": batch.missing.len(),
            "available": verdict.available,
            "latest_price_utc": verdict.latest_utc.map(iso_utc),
        })
    }

    /// The venue's availability verdict, read from the price cache and the clock reference.
    async fn availability(
        &self,
        window: MarketWindow,
        now: DateTime<Utc>,
    ) -> Result<Availability, String> {
        let country = country_key(window.market).map_err(|error| error.message)?;
        let session = self.session(country.exchange, now).ok_or_else(|| {
            format!(
                "KeyError: {}",
                py_repr(&format!(
                    "{} is not in the clock reference",
                    country.exchange
                ))
            )
        })?;
        let latest = self
            .prices
            .latest_venue_price(country.venue)
            .await
            .map_err(|error| format!("StoreError: {error}"))?;
        check(country.key, latest, &session).map_err(|error| error.message)
    }

    fn session(&self, exchange: &str, now: DateTime<Utc>) -> Option<ExchangeSession> {
        self.schedule.clock().availability_session(exchange, now)
    }
}

#[async_trait]
impl MarketRunner for Collector {
    async fn run_market(&self, market: &str) -> MarketRun {
        // No normalization: the endpoint uppercases (`/schedule/run?market=au`) exactly as the
        // source did, and anything the scheduler hands us is already a window code.
        let Some(window) = window(market) else {
            return refused(
                market,
                "unknown-market",
                format!("market must be one of {}", market_list()),
            );
        };
        let now = self.clock.now_utc();
        if !self.schedule.in_session(window, now) {
            return refused(
                market,
                "closed",
                "the venue is not open; nothing was collected",
            );
        }

        let daily = match self.refresh.refresh(market).await {
            Ok(daily) => daily,
            Err(error) => return refused(market, "error", error.to_string()),
        };
        let status = daily.status.as_str();
        let mut payload = Map::new();
        payload.insert("market".to_string(), json!(market));
        payload.insert(
            "daily".to_string(),
            serde_json::to_value(&daily).unwrap_or(Value::Null),
        );
        payload.insert("status".to_string(), json!(status));
        let mut ok = matches!(
            daily.status,
            RefreshStatus::Refreshed | RefreshStatus::AlreadyFresh
        );

        let detail = if window.market == "US" {
            // The whole-market snapshot is US-only; a failure is reported, not fatal to the
            // run, because the settled daily pull above already succeeded.
            match self.products.refresh_snapshot("US").await {
                Ok(rows) => {
                    payload.insert("snapshot_rows".to_string(), json!(rows));
                }
                Err(error) => {
                    payload.insert("snapshot_error".to_string(), json!(error.to_string()));
                }
            }
            String::new()
        } else {
            let pricing = self.price_universe(window, now).await;
            let available = pricing["available"].as_bool().unwrap_or(false);
            ok &= available;
            let priced = pricing["priced"].as_i64().unwrap_or(0);
            let universe = pricing["universe"].as_i64().unwrap_or(0);
            // `detail` is the only field the scheduler logs and `/schedule` shows, so it says
            // what happened rather than "no detail".
            let detail = format!(
                "daily {status}; priced {priced}/{universe} of the {market} universe; \
                 available {}",
                if available { "true" } else { "false" }
            );
            payload.insert("price_refresh".to_string(), pricing);
            detail
        };

        payload.insert("ok".to_string(), json!(ok));
        if !detail.is_empty() {
            payload.insert("detail".to_string(), json!(detail));
        }
        MarketRun {
            ok,
            status: status.to_string(),
            detail,
            payload: Value::Object(payload),
        }
    }
}

fn window(market: &str) -> Option<MarketWindow> {
    MARKETS
        .iter()
        .copied()
        .find(|window| window.market == market)
}

/// `market must be one of ['AU', 'US']`, sorted and single-quoted as Python rendered it.
fn market_list() -> String {
    let mut markets: Vec<&str> = MARKETS.iter().map(|window| window.market).collect();
    markets.sort_unstable();
    let rendered: Vec<String> = markets.iter().map(|market| format!("'{market}'")).collect();
    format!("[{}]", rendered.join(", "))
}

fn refused(market: &str, status: &str, detail: impl Into<String>) -> MarketRun {
    let detail = detail.into();
    MarketRun {
        ok: false,
        status: status.to_string(),
        detail: detail.clone(),
        payload: json!({
            "ok": false,
            "status": status,
            "market": market,
            "detail": detail,
        }),
    }
}

fn pricing_error(venue: &str, detail: String) -> Value {
    json!({"venue": venue, "available": false, "error": detail})
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Mutex as StdMutex;

    use chrono::{NaiveDateTime, TimeZone};
    use serde_json::json;

    use super::*;
    use crate::clock::MarketClock;
    use crate::error::MarketDataError;
    use crate::presence::Presence;
    use crate::store::models::{CachedPrice, Movers, PriceRow};
    use crate::store::StoreError;

    // -- the fakes ----------------------------------------------------------

    #[derive(Default)]
    struct FakeProducts {
        calls: StdMutex<Vec<String>>,
        snapshot_rows: usize,
        snapshot_error: Option<String>,
        prices_error: Option<String>,
    }

    impl FakeProducts {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CollectionProducts for FakeProducts {
        async fn refresh_snapshot(&self, venue: &str) -> Result<usize, ProductError> {
            self.calls.lock().unwrap().push(format!("snapshot:{venue}"));
            match &self.snapshot_error {
                Some(message) => Err(ProductError::Refused(MarketDataError::new(
                    message.clone(),
                    "",
                    502,
                ))),
                None => Ok(self.snapshot_rows),
            }
        }

        async fn prices(
            &self,
            symbols: &[String],
            venue: Option<&str>,
            _max_age_seconds: Option<f64>,
            _asset_class: Option<&str>,
            _country_code: Option<&str>,
        ) -> Result<PricesResponse, ProductError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("prices:{symbols:?}:{venue:?}"));
            if let Some(message) = &self.prices_error {
                return Err(ProductError::Refused(MarketDataError::new(
                    message.clone(),
                    "",
                    502,
                )));
            }
            let venue = venue.unwrap_or("US").to_string();
            // One name is missing and one is fetched, the shape the source suite pinned.
            let priced = symbols.len().saturating_sub(1);
            Ok(PricesResponse {
                count: priced,
                quotes: BTreeMap::new(),
                venue,
                fetched: symbols[..priced].to_vec(),
                missing: symbols.last().cloned().into_iter().collect(),
                deferred: Vec::new(),
                complete: true,
                route: "per-symbol",
                requested: symbols.len(),
            })
        }

        fn vendor(&self) -> Option<Arc<dyn RefreshVendor>> {
            None
        }
    }

    #[derive(Default)]
    struct FakePrices {
        symbols: StdMutex<Vec<String>>,
        latest: StdMutex<Option<NaiveDateTime>>,
        seeded: StdMutex<Vec<(String, i64, i64)>>,
    }

    impl FakePrices {
        fn with(symbols: &[&str], latest: Option<NaiveDateTime>) -> Arc<Self> {
            let fake = FakePrices::default();
            *fake.symbols.lock().unwrap() = symbols.iter().map(|s| s.to_string()).collect();
            *fake.latest.lock().unwrap() = latest;
            Arc::new(fake)
        }

        fn seeded(&self) -> Vec<(String, i64, i64)> {
            self.seeded.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl PriceRepository for FakePrices {
        async fn get_prices(
            &self,
            _symbols: &[String],
            _venue: &str,
        ) -> Result<HashMap<String, CachedPrice>, StoreError> {
            Ok(HashMap::new())
        }

        async fn upsert_prices(&self, _rows: Vec<PriceRow>) -> Result<usize, StoreError> {
            Ok(0)
        }

        async fn big_movers(
            &self,
            _venue: &str,
            _limit: i64,
            _min_price: f64,
            _min_dollar_volume: f64,
            _since: Option<NaiveDateTime>,
        ) -> Result<Movers, StoreError> {
            Ok(Movers {
                winners: Vec::new(),
                losers: Vec::new(),
            })
        }

        async fn latest_venue_price(
            &self,
            _venue: &str,
        ) -> Result<Option<NaiveDateTime>, StoreError> {
            Ok(*self.latest.lock().unwrap())
        }

        async fn top_symbols_by_dollar_volume(
            &self,
            venue: &str,
            limit: i64,
            min_breadth: i64,
        ) -> Result<Vec<String>, StoreError> {
            self.seeded
                .lock()
                .unwrap()
                .push((venue.to_string(), limit, min_breadth));
            Ok(self.symbols.lock().unwrap().clone())
        }
    }

    struct FakeRefresh {
        outcome: Result<RefreshOutcome, RefreshFailure>,
        calls: StdMutex<Vec<String>>,
    }

    impl FakeRefresh {
        fn new(outcome: Result<RefreshOutcome, RefreshFailure>) -> Arc<Self> {
            Arc::new(Self {
                outcome,
                calls: StdMutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl DailyRefresh for FakeRefresh {
        async fn refresh(&self, market: &str) -> Result<RefreshOutcome, RefreshFailure> {
            self.calls.lock().unwrap().push(market.to_string());
            self.outcome.clone()
        }
    }

    struct Fixed(DateTime<Utc>);

    impl Clock for Fixed {
        fn now_utc(&self) -> DateTime<Utc> {
            self.0
        }
    }

    // -- the harness --------------------------------------------------------

    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn naive(at: DateTime<Utc>) -> NaiveDateTime {
        at.naive_utc()
    }

    fn refreshed() -> RefreshOutcome {
        RefreshOutcome {
            status: RefreshStatus::Refreshed,
            market: Some("US".to_string()),
            sessions: Some(Vec::new()),
            rows_written: Some(42),
            expected_session: Presence::Value("2026-11-03".to_string()),
            newest_session: Presence::Value("2026-11-03".to_string()),
            behind_expected: Some(false),
            indicators: None,
            units: 3,
            duration_ms: Some(12),
            detail: None,
        }
    }

    fn schedule() -> Arc<SessionSchedule> {
        let clock = MarketClock::from_json(include_str!("../data/clock.json")).unwrap();
        Arc::new(SessionSchedule::new(Arc::new(clock), 45, 15))
    }

    fn collector<'a>(
        products: &'a Arc<FakeProducts>,
        prices: &'a Arc<FakePrices>,
        refresh: &'a Arc<FakeRefresh>,
        now: DateTime<Utc>,
    ) -> Collector {
        Collector::new(
            products.clone(),
            prices.clone(),
            refresh.clone(),
            Arc::new(Fixed(now)),
            schedule(),
            100,
        )
    }

    // -- the tests ----------------------------------------------------------

    #[tokio::test]
    async fn an_unknown_market_is_named_before_anything_is_collected() {
        let products = Arc::new(FakeProducts::default());
        let prices = FakePrices::with(&[], None);
        let refresh = FakeRefresh::new(Ok(refreshed()));
        let run = collector(&products, &prices, &refresh, utc(2026, 11, 2, 15, 0))
            .run_market("NZ")
            .await;

        assert!(!run.ok);
        assert_eq!(run.status, "unknown-market");
        assert_eq!(run.detail, "market must be one of ['AU', 'US']");
        assert_eq!(
            run.payload,
            json!({
                "ok": false,
                "status": "unknown-market",
                "market": "NZ",
                "detail": "market must be one of ['AU', 'US']",
            })
        );
        assert!(refresh.calls().is_empty());
        assert!(products.calls().is_empty());
    }

    #[tokio::test]
    async fn a_closed_venue_collects_nothing() {
        // Saturday: the NYSE session resolves to Monday's open.
        let products = Arc::new(FakeProducts::default());
        let prices = FakePrices::with(&[], None);
        let refresh = FakeRefresh::new(Ok(refreshed()));
        let run = collector(&products, &prices, &refresh, utc(2026, 10, 31, 12, 0))
            .run_market("US")
            .await;

        assert!(!run.ok);
        assert_eq!(run.status, "closed");
        assert_eq!(run.detail, "the venue is not open; nothing was collected");
        assert!(refresh.calls().is_empty());
    }

    #[tokio::test]
    async fn a_us_run_pulls_the_daily_session_then_the_whole_market_snapshot() {
        let products = Arc::new(FakeProducts {
            snapshot_rows: 7,
            ..FakeProducts::default()
        });
        let prices = FakePrices::with(&["AAPL"], None);
        let refresh = FakeRefresh::new(Ok(refreshed()));
        let run = collector(&products, &prices, &refresh, utc(2026, 11, 2, 15, 0))
            .run_market("US")
            .await;

        assert!(run.ok);
        assert_eq!(run.status, "refreshed");
        assert_eq!(run.detail, "");
        assert_eq!(run.payload["snapshot_rows"], 7);
        assert_eq!(run.payload["daily"]["status"], "refreshed");
        assert!(run.payload.get("detail").is_none());
        assert!(run.payload.get("price_refresh").is_none());
        assert_eq!(refresh.calls(), vec!["US"]);
        assert_eq!(products.calls(), vec!["snapshot:US"]);
    }

    #[tokio::test]
    async fn a_snapshot_failure_is_reported_and_does_not_fail_the_run() {
        let products = Arc::new(FakeProducts {
            snapshot_error: Some("vendor refused".to_string()),
            ..FakeProducts::default()
        });
        let prices = FakePrices::with(&[], None);
        let refresh = FakeRefresh::new(Ok(refreshed()));
        let run = collector(&products, &prices, &refresh, utc(2026, 11, 2, 15, 0))
            .run_market("US")
            .await;

        assert!(run.ok);
        assert_eq!(run.payload["snapshot_error"], "vendor refused");
        assert!(run.payload.get("snapshot_rows").is_none());
    }

    #[tokio::test]
    async fn an_au_run_prices_the_derived_universe_and_reports_availability() {
        // ASX 10:00-16:00 Sydney in November is 23:00-05:00Z; 00:30Z is inside it.
        let products = Arc::new(FakeProducts::default());
        let prices = FakePrices::with(&["BHP", "CBA", "COL"], Some(naive(utc(2026, 11, 2, 0, 10))));
        let refresh = FakeRefresh::new(Ok(refreshed()));
        let run = collector(&products, &prices, &refresh, utc(2026, 11, 2, 0, 30))
            .run_market("AU")
            .await;

        assert!(run.ok);
        assert_eq!(
            run.detail,
            "daily refreshed; priced 2/3 of the AU universe; available true"
        );
        assert_eq!(prices.seeded(), vec![("AU".to_string(), 100, 50)]);
        assert_eq!(run.payload["price_refresh"]["universe"], 3);
        assert_eq!(run.payload["price_refresh"]["requested"], 3);
        assert_eq!(run.payload["price_refresh"]["priced"], 2);
        assert_eq!(run.payload["price_refresh"]["fetched"], 2);
        assert_eq!(run.payload["price_refresh"]["missing"], 1);
        assert_eq!(run.payload["price_refresh"]["available"], true);
        assert_eq!(
            run.payload["price_refresh"]["latest_price_utc"],
            "2026-11-02T00:10:00+00:00"
        );
        assert_eq!(
            products.calls(),
            vec![r#"prices:["BHP", "CBA", "COL"]:Some("AU")"#]
        );
    }

    #[tokio::test]
    async fn an_unavailable_au_price_makes_the_run_not_ok_even_after_a_good_daily_pull() {
        let products = Arc::new(FakeProducts::default());
        // Five minutes before the open: yesterday's echo, not today's observation.
        let prices = FakePrices::with(&["BHP"], Some(naive(utc(2026, 11, 1, 22, 50))));
        let refresh = FakeRefresh::new(Ok(refreshed()));
        let run = collector(&products, &prices, &refresh, utc(2026, 11, 2, 0, 30))
            .run_market("AU")
            .await;

        assert!(!run.ok);
        assert_eq!(run.status, "refreshed");
        assert_eq!(
            run.detail,
            "daily refreshed; priced 0/1 of the AU universe; available false"
        );
        assert_eq!(run.payload["price_refresh"]["available"], false);
    }

    #[tokio::test]
    async fn an_empty_universe_is_named_and_prices_nothing() {
        let products = Arc::new(FakeProducts::default());
        let prices = FakePrices::with(&[], None);
        let refresh = FakeRefresh::new(Ok(refreshed()));
        let run = collector(&products, &prices, &refresh, utc(2026, 11, 2, 0, 30))
            .run_market("AU")
            .await;

        assert!(!run.ok);
        assert_eq!(
            run.payload["price_refresh"],
            json!({"venue": "AU", "universe": 0, "available": false, "reason": "no-universe"})
        );
        assert_eq!(
            run.detail,
            "daily refreshed; priced 0/0 of the AU universe; available false"
        );
        assert!(products.calls().is_empty());
    }

    #[tokio::test]
    async fn a_pricing_failure_is_reported_inside_the_price_refresh() {
        let products = Arc::new(FakeProducts {
            prices_error: Some("provider exploded".to_string()),
            ..FakeProducts::default()
        });
        let prices = FakePrices::with(&["BHP"], None);
        let refresh = FakeRefresh::new(Ok(refreshed()));
        let run = collector(&products, &prices, &refresh, utc(2026, 11, 2, 0, 30))
            .run_market("AU")
            .await;

        assert!(!run.ok);
        assert_eq!(
            run.payload["price_refresh"],
            json!({"venue": "AU", "available": false, "error": "provider exploded"})
        );
    }

    #[tokio::test]
    async fn already_fresh_is_a_good_run() {
        let mut outcome = refreshed();
        outcome.status = RefreshStatus::AlreadyFresh;
        let products = Arc::new(FakeProducts::default());
        let prices = FakePrices::with(&[], None);
        let refresh = FakeRefresh::new(Ok(outcome));
        let run = collector(&products, &prices, &refresh, utc(2026, 11, 2, 15, 0))
            .run_market("US")
            .await;

        assert!(run.ok);
        assert_eq!(run.status, "already-fresh");
    }

    #[tokio::test]
    async fn a_refresh_failure_becomes_an_error_run_rather_than_escaping() {
        let products = Arc::new(FakeProducts::default());
        let prices = FakePrices::with(&[], None);
        let refresh = FakeRefresh::new(Err(RefreshFailure::Invalid(
            "vendor rejected the bulk pull".to_string(),
        )));
        let run = collector(&products, &prices, &refresh, utc(2026, 11, 2, 15, 0))
            .run_market("US")
            .await;

        assert!(!run.ok);
        assert_eq!(run.status, "error");
        assert_eq!(run.payload["market"], "US");
        assert_eq!(run.payload["detail"], "vendor rejected the bulk pull");
        assert!(products.calls().is_empty());
    }

    #[tokio::test]
    async fn a_busy_refresh_is_not_ok_and_still_reports_its_daily_answer() {
        let mut outcome = refreshed();
        outcome.status = RefreshStatus::Busy;
        let products = Arc::new(FakeProducts::default());
        let prices = FakePrices::with(&[], None);
        let refresh = FakeRefresh::new(Ok(outcome));
        let run = collector(&products, &prices, &refresh, utc(2026, 11, 2, 15, 0))
            .run_market("US")
            .await;

        assert!(!run.ok);
        assert_eq!(run.status, "busy");
        assert_eq!(run.payload["daily"]["status"], "busy");
    }

    #[test]
    fn the_market_list_is_sorted_and_single_quoted_like_python() {
        assert_eq!(market_list(), "['AU', 'US']");
    }

    #[test]
    fn the_universe_min_breadth_is_the_store_default() {
        // The seed query's default is what makes the universe stable; pin the copy.
        assert_eq!(UNIVERSE_MIN_BREADTH, 50);
    }
}
