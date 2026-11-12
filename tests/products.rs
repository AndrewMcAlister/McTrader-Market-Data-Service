//! The products layer: routing, caching and provenance, with no vendor and no database.
//!
//! Ported from the source suite's `test_products.py`. The two decisions most worth pinning
//! are the same ones: `prices()` picks bulk or per-symbol from the list length, because the
//! tariff break-even is 100 symbols, and a cached answer costs no vendor call while a fresh
//! one is written back so the next caller is free.
//!
//! The source suite pinned the completed session by monkeypatching a module global. Here the
//! layer takes a [`Clock`], so each test states the instant instead. The pinned instant is
//! 2026-10-30 00:00 UTC, when New York has settled 2026-10-29.

mod common;

use std::sync::Arc;

use chrono::Duration;

use common::{
    bulk_quotes, date, pinned_clock, products as build_products, FakeProvider, FakeStore, TestClock,
};
use mmds::clock::Clock;
use mmds::domain::sessions::latest_completed_session;
use mmds::error::MarketDataError;
use mmds::presence::Presence;
use mmds::products::{AtrRequest, ProductError, Products};

fn symbols(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| name.to_string()).collect()
}

async fn setup() -> (Products, Arc<FakeProvider>, Arc<FakeStore>, Arc<TestClock>) {
    let store = Arc::new(FakeStore::new());
    let provider = Arc::new(FakeProvider::new("eodhd"));
    let clock = pinned_clock();
    let products = build_products(store.clone(), &provider, clock.clone()).await;
    (products, provider, store, clock)
}

fn refusal(error: ProductError) -> MarketDataError {
    match error {
        ProductError::Refused(error) => error,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// price: the cache is the point
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_price_miss_costs_exactly_one_vendor_call() {
    let (products, provider, store, _) = setup().await;

    let quote = products
        .price("AAPL", None, None, None, None)
        .await
        .expect("AAPL is priced");

    assert!((quote.price.unwrap() - 100.0).abs() < 1e-12);
    assert!((quote.previous_close.unwrap() - 98.0).abs() < 1e-12);
    assert_eq!(provider.quote_calls(), 1);
    assert_eq!(store.price_writes(), 1);
}

#[tokio::test]
async fn a_second_price_read_inside_the_window_costs_nothing() {
    let (products, provider, _, _) = setup().await;
    products
        .price("AAPL", None, None, None, None)
        .await
        .unwrap();

    let quote = products
        .price("AAPL", None, None, None, None)
        .await
        .unwrap();

    assert_eq!(provider.quote_calls(), 1);
    assert_eq!(quote.symbol.as_deref(), Some("AAPL"));
}

#[tokio::test]
async fn change_pct_is_computed_from_the_stored_pair() {
    // Not read from a vendor field: the ranking variable must be one arithmetic.
    let (products, _, _, _) = setup().await;
    let quote = products
        .price("AAPL", None, None, None, None)
        .await
        .unwrap();
    let expected = (100.0 / 98.0 - 1.0) * 100.0;
    assert!((quote.change_pct.unwrap() - expected).abs() < 1e-12);
}

#[tokio::test]
async fn max_age_seconds_can_lower_the_window_but_not_raise_it() {
    let (products, provider, store, clock) = setup().await;
    products
        .price("AAPL", None, None, None, None)
        .await
        .unwrap();

    // 100 seconds old: inside the 300-second policy, outside a caller's 60-second request.
    let aged = clock.now_utc().naive_utc() - Duration::seconds(100);
    store.backdate_price("AAPL", "US", aged);

    products
        .price("AAPL", None, Some(60.0), None, None)
        .await
        .unwrap();

    assert_eq!(provider.quote_calls(), 2);
}

#[tokio::test]
async fn an_unknown_symbol_is_reported_not_priced_at_zero() {
    let (products, provider, _, _) = setup().await;
    provider.set_quote_failure(true);

    let error = refusal(
        products
            .price("NOPE", None, None, None, None)
            .await
            .unwrap_err(),
    );

    assert_eq!(error.reason, "unpriced");
    assert_eq!(error.status, 404);
}

// ---------------------------------------------------------------------------
// prices: the routing decision
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_small_batch_goes_per_symbol() {
    let (products, provider, _, _) = setup().await;

    let result = products
        .prices(
            &symbols(&["S0", "S1", "S2", "S3", "S4", "S5"]),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(result.route, "per_symbol");
    assert_eq!(provider.quote_calls(), 6);
    assert_eq!(provider.whole_market_calls(), 0);
    assert_eq!(result.count, 6);
}

#[tokio::test]
async fn a_large_batch_goes_bulk_because_100_is_the_break_even() {
    // 150 symbols: 150 units per-symbol against 100 flat in bulk.
    let (products, provider, _, _) = setup().await;
    provider.set_whole_market(bulk_quotes(300));

    let wanted: Vec<String> = (0..150).map(|index| format!("SYM{index:04}")).collect();
    let result = products
        .prices(&wanted, None, None, None, None)
        .await
        .unwrap();

    assert_eq!(result.route, "bulk");
    assert_eq!(provider.quote_calls(), 0);
    assert_eq!(provider.whole_market_calls(), 1);
    assert_eq!(result.count, 150);
}

#[tokio::test]
async fn the_bulk_snapshot_warms_the_cache_for_every_row_not_just_the_asked_ones() {
    // One scan should leave single-symbol reads free for everyone afterwards.
    let (products, provider, store, _) = setup().await;
    provider.set_whole_market(bulk_quotes(300));

    let wanted: Vec<String> = (0..150).map(|index| format!("SYM{index:04}")).collect();
    products
        .prices(&wanted, None, None, None, None)
        .await
        .unwrap();

    // 300 rows in the feed, all written, not the 150 requested.
    assert_eq!(store.price_writes(), 300);

    products
        .price("SYM0299", None, None, None, None)
        .await
        .expect("the warmed row prices");
    assert_eq!(provider.quote_calls(), 0);
}

#[tokio::test]
async fn the_bulk_snapshot_is_reused_inside_its_own_lifetime() {
    // The 120-second snapshot is the whole reason a scan does not refetch 100 units per
    // request. Age the cached prices past the price TTL while the snapshot stays fresh.
    let (products, provider, store, clock) = setup().await;
    provider.set_whole_market(bulk_quotes(300));
    let wanted: Vec<String> = (0..150).map(|index| format!("SYM{index:04}")).collect();
    products
        .prices(&wanted, None, None, None, None)
        .await
        .unwrap();

    let aged = clock.now_utc().naive_utc() - Duration::seconds(400);
    store.backdate_exchange_prices("US", aged);

    let result = products
        .prices(&wanted, None, None, None, None)
        .await
        .unwrap();

    assert_eq!(provider.whole_market_calls(), 1);
    assert_eq!(result.count, 150);
    assert_eq!(store.price_writes(), 300);
}

#[tokio::test]
async fn no_symbols_is_a_refusal_not_an_empty_answer() {
    let (products, _, _, _) = setup().await;
    let error = refusal(
        products
            .prices(&[], None, None, None, None)
            .await
            .unwrap_err(),
    );
    assert_eq!(error.reason, "no-symbols");
    assert_eq!(error.status, 400);
}

// ---------------------------------------------------------------------------
// bars
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bars_come_from_the_vendor_once_then_from_the_cache() {
    let (products, provider, store, _) = setup().await;
    provider.set_newest(date(2026, 10, 29));

    products
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap();
    let first_writes = store.daily_writes();

    products
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap();

    assert_eq!(provider.bars_calls(), 1);
    assert_eq!(store.daily_writes(), first_writes);
}

#[tokio::test]
async fn bars_declare_their_session_and_adjustment() {
    let (products, provider, _, _) = setup().await;
    provider.set_newest(date(2026, 10, 29));

    let result = products
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap();
    let entry = &result.bars["AAPL"];

    assert_eq!(entry.sessions, 15);
    assert!(entry.adjusted);
    assert_eq!(entry.as_of_session.as_deref(), Some("2026-10-29"));
}

#[tokio::test]
async fn bars_carry_both_closes() {
    // `c` is the adjusted series; `cr` is the price that printed. A candle body that mixes
    // the two reads a dividend as a gap.
    let (products, provider, _, _) = setup().await;
    provider.set_newest(date(2026, 10, 29));

    let entry = products
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap()
        .bars
        .remove("AAPL")
        .unwrap();

    let first = &entry.rows[0];
    let close = first.c.expect("the adjusted close is present");
    let raw = first.cr.expect("the raw close is present");
    assert!((raw - (close + 0.1)).abs() < 1e-12);
}

// ---------------------------------------------------------------------------
// bars: a vendor refresh fills two years, not three weeks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_first_refresh_asks_for_the_whole_history_window() {
    let (products, provider, _, _) = setup().await;

    products
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap();

    assert_eq!(provider.bars_calls(), 1);
    // One unit either way, so the window is widened to the configured history depth.
    assert_eq!(
        provider.bars_widths(),
        vec![usize::try_from(products.settings().history_sessions).unwrap()]
    );
}

#[tokio::test]
async fn a_series_that_already_reaches_the_horizon_is_refreshed_shallow() {
    // The depth check keeps a fill from becoming a nightly rewrite of 560 rows.
    let (products, provider, store, _) = setup().await;
    store.seed_history("US", "AAPL", 560, date(2026, 10, 28));
    provider.set_newest(date(2026, 10, 29));

    products
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap();

    assert_eq!(provider.bars_widths(), vec![15]);
}

#[tokio::test]
async fn a_listing_younger_than_the_horizon_is_asked_deep_only_once() {
    // A young listing can never reach the target depth; without the `achieved` guard it
    // would be re-asked at full width every session for the rest of its life.
    let (products, provider, _, clock) = setup().await;

    products
        .bars(&symbols(&["IPO"]), 15, None, true, None, None)
        .await
        .unwrap();
    assert_eq!(provider.bars_widths(), vec![560]);

    clock.advance_days(1);
    provider.set_newest(date(2026, 10, 30));
    products
        .bars(&symbols(&["IPO"]), 15, None, true, None, None)
        .await
        .unwrap();

    assert_eq!(provider.bars_widths(), vec![560, 15]);
}

#[tokio::test]
async fn a_current_cache_without_raw_closes_is_refreshed_once() {
    // Pre-existing rows have no RawClose. One vendor call fills the served window; the next
    // request is a cache hit, so the backfill cannot become a call per scan.
    let (products, provider, store, clock) = setup().await;
    provider.set_newest(date(2026, 10, 29));
    products
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap();

    store.strip_raw_closes("US", "AAPL");

    // A fresh Products models a restart: the refresh memory belongs to the process.
    let restarted = build_products(store.clone(), &provider, clock).await;
    restarted
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap();

    assert_eq!(provider.bars_calls(), 2);
    assert!(store
        .daily_rows("US", "AAPL")
        .iter()
        .all(|row| row.raw_close.is_some()));

    restarted
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap();
    assert_eq!(provider.bars_calls(), 2);
}

#[tokio::test]
async fn allow_fetch_false_never_touches_the_vendor() {
    let (products, provider, _, _) = setup().await;

    let result = products
        .bars(&symbols(&["AAPL"]), 15, None, false, None, None)
        .await
        .unwrap();

    assert_eq!(provider.bars_calls(), 0);
    assert!(result.bars.is_empty());
    assert_eq!(result.short_history, vec!["AAPL".to_string()]);
}

// ---------------------------------------------------------------------------
// bars: a cache hit must be current, not merely long enough
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cached_series_that_stops_before_the_completed_session_is_refreshed() {
    let (products, provider, _, clock) = setup().await;
    products
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap();
    assert_eq!(provider.bars_calls(), 1);

    // The exchange has completed another session and the vendor has since published it.
    clock.advance_days(1);
    provider.set_newest(date(2026, 10, 30));

    let result = products
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap();

    assert_eq!(provider.bars_calls(), 2);
    assert_eq!(
        result.bars["AAPL"].as_of_session.as_deref(),
        Some("2026-10-30")
    );
}

#[tokio::test]
async fn a_refresh_that_cannot_reach_the_session_is_asked_for_once() {
    // A holiday, or a vendor that publishes late, must not cost a call per request.
    let (products, provider, _, _) = setup().await;

    products
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap();
    products
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap();

    assert_eq!(provider.bars_calls(), 1);
}

#[tokio::test]
async fn a_failed_refresh_is_retried_after_the_window() {
    // The vendor publishes after the close, so one early miss must not last the session.
    let (products, provider, _, clock) = setup().await;

    products
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap();
    assert_eq!(provider.bars_calls(), 1);

    clock.advance_seconds(1_800);
    products
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap();
    assert_eq!(provider.bars_calls(), 1);

    clock.advance_seconds(3_600);
    products
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap();
    assert_eq!(provider.bars_calls(), 2);
}

#[tokio::test]
async fn a_newer_session_asks_the_vendor_again() {
    // The memory is per session, so the next close is not suppressed by the last one.
    let (products, provider, _, clock) = setup().await;

    products
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap();
    clock.advance_days(1);
    products
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap();

    assert_eq!(provider.bars_calls(), 2);
}

#[tokio::test]
async fn a_current_cache_still_costs_nothing() {
    // The inverse: the freshness rule must not turn every read into a vendor call.
    let (products, provider, _, _) = setup().await;
    provider.set_newest(date(2026, 10, 29));

    products
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap();
    products
        .bars(&symbols(&["AAPL"]), 15, None, true, None, None)
        .await
        .unwrap();

    assert_eq!(provider.bars_calls(), 1);
}

// ---------------------------------------------------------------------------
// ATR
// ---------------------------------------------------------------------------

#[tokio::test]
async fn atr_computes_from_bars_and_caches_the_result() {
    let (products, provider, store, _) = setup().await;
    let wanted = symbols(&["AAPL"]);

    let result = products.atr_batch(AtrRequest::new(&wanted)).await.unwrap();
    let reading = &result.atr["AAPL"];

    assert!(reading.value.unwrap() > 0.0);
    assert!(reading.sessions_used.unwrap() >= 15);
    assert_eq!(store.indicator_writes().len(), 1);

    let bar_calls = provider.bars_calls();
    products.atr_batch(AtrRequest::new(&wanted)).await.unwrap();
    assert_eq!(provider.bars_calls(), bar_calls);
}

#[tokio::test]
async fn insufficient_history_yields_unavailable_and_a_reason() {
    let (_, provider, _, _) = setup().await;
    provider.set_bars_per_symbol(5);
    let store = Arc::new(FakeStore::new());
    let clock = pinned_clock();
    let products = build_products(store, &provider, clock).await;
    let wanted = symbols(&["AAPL"]);

    let result = products.atr_batch(AtrRequest::new(&wanted)).await.unwrap();
    let reading = &result.atr["AAPL"];

    assert_eq!(reading.value, None);
    match &reading.reason {
        Presence::Value(reason) => assert!(reason.contains("fewer than"), "got {reason}"),
        other => panic!("expected a reason, got {other:?}"),
    }
    assert!(!result.complete);
}

#[tokio::test]
async fn an_unknown_method_is_refused_by_name() {
    let (products, _, _, _) = setup().await;
    let wanted = symbols(&["AAPL"]);
    let mut request = AtrRequest::new(&wanted);
    request.method = Some("average_of_extremes");

    let error = refusal(products.atr_batch(request).await.unwrap_err());

    assert_eq!(error.reason, "bad-method");
    assert_eq!(error.status, 400);
    assert!(error.detail.contains("tr"), "got {}", error.detail);
}

#[tokio::test]
async fn stop_level_needs_both_price_and_multiplier() {
    let (products, _, _, _) = setup().await;
    let wanted = symbols(&["AAPL"]);
    let mut request = AtrRequest::new(&wanted);
    request.price = Some(100.0);

    let error = refusal(products.atr_batch(request).await.unwrap_err());

    assert_eq!(error.reason, "incomplete-stop");
    assert_eq!(error.status, 400);
}

#[tokio::test]
async fn stop_level_is_attached_when_both_are_supplied() {
    let (products, _, _, _) = setup().await;
    let wanted = symbols(&["AAPL"]);
    let mut request = AtrRequest::new(&wanted);
    request.price = Some(100.0);
    request.multiplier = Some(1.0);

    let reading = products
        .atr_batch(request)
        .await
        .unwrap()
        .atr
        .remove("AAPL")
        .unwrap();
    let value = reading.value.expect("the ATR is defined");
    let stop = match reading.stop_level {
        Presence::Value(stop) => stop,
        other => panic!("expected a stop level, got {other:?}"),
    };

    assert!((stop - (100.0 - value)).abs() < 1e-6);
}

#[tokio::test]
async fn a_zero_multiplier_is_a_reason_not_a_500() {
    // A body that fills every field of the schema sends multiplier=0. It must come back as a
    // named 400, not the arithmetic's own failure.
    let (products, _, _, _) = setup().await;
    let wanted = symbols(&["AAPL"]);
    let mut request = AtrRequest::new(&wanted);
    request.price = Some(100.0);
    request.multiplier = Some(0.0);

    let error = refusal(products.atr_batch(request).await.unwrap_err());

    assert_eq!(error.reason, "bad-multiplier");
    assert_eq!(error.status, 400);
}

#[tokio::test]
async fn a_bad_direction_is_a_reason_not_a_500() {
    let (products, _, _, _) = setup().await;
    let wanted = symbols(&["AAPL"]);
    let mut request = AtrRequest::new(&wanted);
    request.price = Some(100.0);
    request.multiplier = Some(1.0);
    request.direction = "sideways";

    let error = refusal(products.atr_batch(request).await.unwrap_err());

    assert_eq!(error.reason, "bad-direction");
    assert_eq!(error.status, 400);
}

#[tokio::test]
async fn the_minimal_request_is_atr_14_with_the_default_definition() {
    // {"symbols": ["AAPL"]} is the whole request: period 14, tr, simple, exchange US.
    let (products, _, _, _) = setup().await;
    let wanted = symbols(&["AAPL"]);

    let result = products.atr_batch(AtrRequest::new(&wanted)).await.unwrap();

    assert_eq!(result.period, 14);
    assert_eq!(result.method, "tr");
    assert_eq!(result.smoothing, "simple");
    assert_eq!(result.exchange, "US");
    assert_eq!(result.atr["AAPL"].period, 14);
}

#[tokio::test]
async fn a_different_smoothing_is_computed_and_not_written_to_the_cache() {
    // IndicatorCache has one Smoothing column, so a non-default definition must not land.
    let (products, _, store, _) = setup().await;
    let wanted = symbols(&["AAPL"]);

    products.atr_batch(AtrRequest::new(&wanted)).await.unwrap();
    let writes_after_default = store.indicator_writes().len();

    let mut request = AtrRequest::new(&wanted);
    request.smoothing = Some("wilder");
    products.atr_batch(request).await.unwrap();

    assert_eq!(store.indicator_writes().len(), writes_after_default);
}

#[tokio::test]
async fn the_response_always_states_the_definition_it_used() {
    let (products, _, _, _) = setup().await;
    let wanted = symbols(&["AAPL"]);
    let mut request = AtrRequest::new(&wanted);
    request.period = Some(22);
    request.method = Some("high_low");

    let result = products.atr_batch(request).await.unwrap();

    assert_eq!(result.period, 22);
    assert_eq!(result.method, "high_low");
    assert_eq!(result.smoothing, "simple");
}

// ---------------------------------------------------------------------------
// the market: exchange, country_code, or the symbol's own suffix
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_country_code_picks_the_market_the_price_is_cached_under() {
    let (products, _, store, _) = setup().await;

    let result = products
        .prices(&symbols(&["CBA"]), None, None, None, Some("AU"))
        .await
        .unwrap();

    assert_eq!(result.exchange, "AU");
    assert!(store.has_price("CBA", "AU"));
    assert!(!store.has_price("CBA", "US"));
    assert_eq!(result.quotes["CBA"].exchange.as_deref(), Some("AU"));
}

#[tokio::test]
async fn a_suffixed_symbol_states_its_own_market_when_no_exchange_does() {
    // `CBA.AU` used to be stripped and priced at exchange US: the suffix was decoration.
    let (products, _, store, _) = setup().await;

    products
        .prices(&symbols(&["CBA.AU"]), None, None, None, None)
        .await
        .unwrap();

    assert!(store.has_price("CBA", "AU"));
    assert!(!store.has_price("CBA", "US"));
}

#[tokio::test]
async fn a_stated_market_and_a_contradicting_suffix_are_refused_before_any_spend() {
    let (products, provider, _, _) = setup().await;

    let error = refusal(
        products
            .prices(&symbols(&["CBA.AU"]), Some("US"), None, None, None)
            .await
            .unwrap_err(),
    );

    assert_eq!(error.reason, "exchange-conflict");
    assert_eq!(error.status, 400);
    assert_eq!(provider.quote_calls(), 0);
}

#[tokio::test]
async fn bars_honour_a_country_code_and_file_the_rows_under_it() {
    let (products, _, store, _) = setup().await;

    let payload = products
        .bars(&symbols(&["CBA"]), 15, None, true, None, Some("AU"))
        .await
        .unwrap();

    assert_eq!(payload.bars["CBA"].exchange, "AU");
    assert_eq!(store.daily_exchanges("CBA"), vec!["AU".to_string()]);
}

#[tokio::test]
async fn atr_at_a_country_code_is_cached_under_that_market() {
    let (products, _, store, _) = setup().await;
    let wanted = symbols(&["CBA"]);
    let mut request = AtrRequest::new(&wanted);
    request.country_code = Some("AU");

    let result = products.atr_batch(request).await.unwrap();

    assert_eq!(result.exchange, "AU");
    assert!(result.atr["CBA"].value.is_some());
    // The write carries the market: without it the next AU request reads a US ATR.
    assert_eq!(store.indicator_exchange("CBA").as_deref(), Some("AU"));
}

#[test]
fn the_pinned_instant_completes_the_previous_us_session() {
    // Stated once, so a change to the session rule fails here rather than in a dozen cache
    // tests that would all look like a refresh-policy failure.
    assert_eq!(
        latest_completed_session(Some("US"), pinned_clock().now_utc()),
        date(2026, 10, 29)
    );
}

// ---------------------------------------------------------------------------
// the four EODHD egress routes
// ---------------------------------------------------------------------------
//
// These go through a real `EodhdClient` pointed at a mock vendor, because `Products` holds the
// concrete client: the request path the endpoint really uses — the encoded `filters` parameter,
// the tariff, the retry rule — is then exercised rather than bypassed. The store stays the
// in-memory fake, so nothing reaches a database, and no test spends quota.

mod egress {
    use std::sync::Arc;

    use chrono::NaiveDate;
    use common::{date, FakeProvider, FakeStore, TestClock};
    use mmds::clock::Clock;
    use mmds::domain::intraday::Timeframe;
    use mmds::error::MarketDataError;
    use mmds::products::{
        BulkDateKey, IntradayRequest, ProductError, Products, ProfileBatch,
    };
    use mmds::providers::eodhd_client::EodhdClient;
    use serde_json::{json, Value};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::common;
    use crate::symbols;

    /// The instant these tests are pinned at: one minute after the 2026-11-12 US session's last
    /// 5-minute bar, so the tape written by a fetch is inside its TTL.
    fn egress_clock() -> Arc<TestClock> {
        Arc::new(TestClock::new(
            NaiveDate::from_ymd_opt(2026, 11, 12)
                .unwrap()
                .and_hms_opt(21, 0, 0)
                .unwrap()
                .and_utc(),
        ))
    }

    /// The products layer over the real client, a mock vendor and the in-memory store.
    async fn setup(server: &MockServer) -> (Products, Arc<FakeStore>, Arc<TestClock>) {
        let store = Arc::new(FakeStore::new());
        let clock = egress_clock();
        store.set_now(clock.now_utc().naive_utc());
        let provider = Arc::new(FakeProvider::new("eodhd"));
        let client = Arc::new(
            EodhdClient::new("test-key", format!("{}/api", server.uri()), 5.0, 5.0)
                .expect("a client with a key"),
        );
        let products = Products::new(
            common::test_settings(),
            store.clone(),
            Some(client),
            Some(common::router_with(&provider)),
            clock.clone(),
        )
        .await
        .expect("the layered product builds");
        (products, store, clock)
    }

    fn refusal(error: ProductError) -> MarketDataError {
        match error {
            ProductError::Refused(error) => error,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    async fn requests(server: &MockServer) -> usize {
        server.received_requests().await.unwrap().len()
    }

    fn query(request: &wiremock::Request, name: &str) -> Option<String> {
        request
            .url
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }

    /// One screener answer, matching a request whose `filters` parameter is exactly `filters`.
    async fn mount_screen(server: &MockServer, filters: Value, body: Value, status: u16) {
        Mock::given(method("GET"))
            .and(path("/api/screener"))
            .and(query_param("filters", filters.to_string()))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(server)
            .await;
    }

    /// A profile answer: one row for `code` at `venue`, or an empty `data`.
    async fn mount_profile(server: &MockServer, code: &str, venue: &str, row: Option<Value>) {
        let filters = json!([["code", "=", code], ["exchange", "=", venue]]);
        let body = match row {
            Some(row) => json!({"data": [row]}),
            None => json!({"data": []}),
        };
        mount_screen(server, filters, body, 200).await;
    }

    // -- POST /screener ----------------------------------------------------

    #[tokio::test]
    async fn a_screen_appends_the_requested_venue_once_and_returns_the_rows_untouched() {
        let server = MockServer::start().await;
        let rows = json!([
            {"code": "AAPL", "exchange": "US", "sector": "Technology", "market_capitalization": 3.5e12},
            {"code": "MSFT", "exchange": "US", "sector": null, "currency_symbol": "USD"}
        ]);
        let wire = json!([
            ["market_capitalization", ">", 300000000],
            ["exchange", "=", "NASDAQ"]
        ]);
        mount_screen(&server, wire.clone(), json!({"data": rows.clone()}), 200).await;
        let (products, store, _) = setup(&server).await;

        let answer = products
            .screener(
                "US",
                Some("NASDAQ"),
                &[
                    json!(["market_capitalization", ">", 300000000]),
                    json!(["exchange", "=", "NASDAQ"]),
                ],
                Some("refund_5d_p.desc"),
                50,
                0,
            )
            .await
            .expect("the screen answers");

        assert_eq!(answer.provider, "eodhd");
        assert_eq!(answer.count, 2);
        // The rows are the vendor's objects: nulls and unknown columns included.
        assert_eq!(Value::Array(answer.rows.clone()), rows);
        // The lifted filter becomes exactly one triple, last, with the caller's spelling.
        assert_eq!(Value::Array(answer.filters), wire);
        assert_eq!(answer.sort.as_deref(), Some("refund_5d_p.desc"));
        // Five units flat, filed under /screener for the caller in scope.
        let usage = store.recorded_usage();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].request_type, "/screener");
        assert_eq!(usage[0].cost, 5);
    }

    #[tokio::test]
    async fn a_screen_is_never_answered_from_a_cache() {
        let server = MockServer::start().await;
        mount_screen(&server, json!([["exchange", "=", "US"]]), json!({"data": []}), 200).await;
        let (products, _, _) = setup(&server).await;

        for _ in 0..2 {
            products.screener("US", None, &[], None, 100, 0).await.unwrap();
        }

        assert_eq!(requests(&server).await, 2, "a screen is a question about now");
    }

    #[tokio::test]
    async fn an_empty_screen_is_a_200_with_no_rows() {
        let server = MockServer::start().await;
        mount_screen(&server, json!([["exchange", "=", "AU"]]), json!({"data": []}), 200).await;
        let (products, _, _) = setup(&server).await;

        let answer = products
            .screener("AU", None, &[], None, 100, 0)
            .await
            .unwrap();
        assert_eq!(answer.count, 0);
        assert!(answer.rows.is_empty());
    }

    #[tokio::test]
    async fn a_vendor_envelope_without_data_is_a_502_rather_than_an_empty_screen() {
        let server = MockServer::start().await;
        mount_screen(&server, json!([["exchange", "=", "US"]]), json!({"rows": []}), 200).await;
        let (products, _, _) = setup(&server).await;

        match products.screener("US", None, &[], None, 100, 0).await {
            Err(ProductError::Provider(failure)) => {
                assert_eq!(failure.reason, "unparseable");
                assert!(failure.detail.contains("data"), "{failure}");
            }
            other => panic!("expected an unparseable provider failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_vendor_422_is_a_caller_refusal_not_an_outage() {
        let server = MockServer::start().await;
        // A filter the local vocabulary accepts: the vendor can still refuse a query the table
        // cannot predict, and that refusal is the caller's to fix, not an outage.
        let filter = json!(["adjusted_close", ">", 1]);
        mount_screen(
            &server,
            // The wire form carries the venue triple the endpoint appends.
            json!([["adjusted_close", ">", 1], ["exchange", "=", "US"]]),
            json!({"error": "Invalid filter value: adjusted_close"}),
            422,
        )
        .await;
        let (products, _, _) = setup(&server).await;

        let error = refusal(
            products
                .screener("US", None, &[filter], None, 100, 0)
                .await
                .unwrap_err(),
        );
        assert_eq!(error.reason, "vendor-rejected");
        assert_eq!(error.status, 422);
        assert!(error.detail.contains("adjusted_close"), "{error}");
    }

    // -- POST /profile -----------------------------------------------------

    #[tokio::test]
    async fn a_profile_pins_the_code_and_the_venue_and_asks_for_five_rows() {
        let server = MockServer::start().await;
        mount_profile(
            &server,
            "WDS",
            "AU",
            Some(json!({
                "code": "WDS", "exchange": "AU", "name": "Woodside Energy Group Ltd",
                "sector": "Energy", "industry": "Oil & Gas E&P",
                "market_capitalization": 51_234_567_890.0, "currency_symbol": "AUD",
                "last_day_data_date": "2026-11-11"
            })),
        )
        .await;
        mount_profile(&server, "BHP", "AU", Some(json!({"code": "BHP", "name": "BHP Group Ltd"})))
            .await;
        let (products, store, _) = setup(&server).await;

        let answer = products
            .profiles(ProfileBatch {
                symbols: &symbols(&["WDS", "BHP"]),
                exchange: Some("ASX"),
                country_code: Some("AU"),
            })
            .await
            .unwrap();

        assert_eq!(answer.exchange, "AU");
        assert_eq!(answer.requested, 2);
        assert!(answer.complete);
        let wds = &answer.profiles["WDS"];
        assert!(wds.found);
        assert_eq!(wds.name.as_deref(), Some("Woodside Energy Group Ltd"));
        assert_eq!(wds.sector.as_deref(), Some("Energy"));
        assert_eq!(wds.market_cap, Some(51_234_567_890.0));
        assert_eq!(wds.currency.as_deref(), Some("AUD"));
        assert_eq!(wds.as_of.as_deref(), Some("2026-11-11"));
        // No entitled source: the keys are present and null, not absent.
        assert_eq!(wds.country, None);
        assert_eq!(wds.gic_sector, None);
        assert_eq!(wds.description, None);
        assert_eq!(wds.error, None);

        let sent = server.received_requests().await.unwrap();
        assert_eq!(sent.len(), 2, "one pinned call per symbol");
        for request in &sent {
            assert_eq!(query(request, "limit"), Some("5".into()));
            assert_eq!(query(request, "sort"), None);
            let filters: Value =
                serde_json::from_str(&query(request, "filters").unwrap()).unwrap();
            // `code` alone is not unique: the exchange filter is what pins the listing.
            assert_eq!(filters[0][0], "code");
            assert_eq!(filters[1], json!(["exchange", "=", "AU"]));
        }
        // Five units per uncached symbol.
        let usage = store.recorded_usage();
        assert_eq!(usage.iter().map(|row| row.cost).sum::<i64>(), 10);
    }

    #[tokio::test]
    async fn a_profile_memo_hit_costs_nothing_and_the_ttl_expires() {
        let server = MockServer::start().await;
        mount_profile(&server, "WDS", "AU", Some(json!({"code": "WDS", "name": "Woodside"}))).await;
        let (products, store, clock) = setup(&server).await;
        let wanted = symbols(&["WDS"]);

        let batch = || ProfileBatch {
            symbols: &wanted,
            exchange: None,
            country_code: Some("AU"),
        };
        products.profiles(batch()).await.unwrap();
        assert_eq!(requests(&server).await, 1);

        // A second read inside the hour spends nothing.
        products.profiles(batch()).await.unwrap();
        assert_eq!(requests(&server).await, 1);
        assert_eq!(store.recorded_usage().len(), 1);

        // Past the hour the profile is asked for again.
        clock.advance_seconds(3601);
        products.profiles(batch()).await.unwrap();
        assert_eq!(requests(&server).await, 2);
    }

    #[tokio::test]
    async fn a_symbol_with_no_listing_is_found_false_and_is_never_memoised() {
        let server = MockServer::start().await;
        mount_profile(&server, "NOPE", "US", None).await;
        let (products, _, _) = setup(&server).await;
        let wanted = symbols(&["NOPE"]);
        let batch = || ProfileBatch {
            symbols: &wanted,
            exchange: None,
            country_code: Some("US"),
        };

        let answer = products.profiles(batch()).await.unwrap();
        assert!(!answer.complete);
        assert_eq!(answer.missing, vec!["NOPE".to_string()]);
        assert!(!answer.profiles["NOPE"].found);
        assert_eq!(answer.profiles["NOPE"].error, None);

        // A miss is not an answer: the next pass asks again rather than caching "not there".
        products.profiles(batch()).await.unwrap();
        assert_eq!(requests(&server).await, 2);
    }

    #[tokio::test]
    async fn a_duplicate_profile_symbol_is_asked_once() {
        let server = MockServer::start().await;
        mount_profile(&server, "WDS", "AU", Some(json!({"code": "WDS", "name": "Woodside"}))).await;
        let (products, _, _) = setup(&server).await;

        products
            .profiles(ProfileBatch {
                symbols: &symbols(&["WDS", "WDS.AU", "wds"]),
                exchange: None,
                country_code: Some("AU"),
            })
            .await
            .unwrap();

        assert_eq!(requests(&server).await, 1);
    }

    #[tokio::test]
    async fn one_failed_profile_call_is_that_symbol_only() {
        let server = MockServer::start().await;
        mount_profile(&server, "WDS", "AU", Some(json!({"code": "WDS", "name": "Woodside"}))).await;
        // The vendor refuses one symbol's query; the batch must still answer the other.
        mount_screen(
            &server,
            json!([["code", "=", "BAD"], ["exchange", "=", "AU"]]),
            json!({"error": "boom"}),
            500,
        )
        .await;
        let (products, _, _) = setup(&server).await;

        let answer = products
            .profiles(ProfileBatch {
                symbols: &symbols(&["WDS", "BAD"]),
                exchange: None,
                country_code: Some("AU"),
            })
            .await
            .unwrap();

        assert!(answer.profiles["WDS"].found);
        assert!(!answer.profiles["BAD"].found);
        assert_eq!(answer.profiles["BAD"].error.as_deref(), Some("http-error"));
        assert!(!answer.complete);
        assert!(answer.missing.contains(&"BAD".to_string()));
    }

    // -- POST /bars/intraday -----------------------------------------------

    /// `count` native 5-minute rows from 14:30 UTC on 2026-11-12 (09:30 in New York).
    fn intraday_payload(count: usize) -> Value {
        let rows: Vec<Value> = (0..count)
            .map(|index| {
                let minute = 14 * 60 + 30 + index * 5;
                let stamp = format!(
                    "2026-11-12 {:02}:{:02}:00",
                    minute / 60,
                    minute % 60
                );
                json!({
                    "date": stamp,
                    "open": 100.0 + index as f64,
                    "high": 101.0 + index as f64,
                    "low": 99.0 + index as f64,
                    "close": 100.5 + index as f64,
                    "volume": 1000
                })
            })
            .collect();
        Value::Array(rows)
    }

    async fn mount_intraday(server: &MockServer, symbol: &str, count: usize) {
        Mock::given(method("GET"))
            .and(path(format!("/api/intraday/{symbol}.US")))
            .respond_with(ResponseTemplate::new(200).set_body_json(intraday_payload(count)))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn an_intraday_fetch_stores_the_native_rows_and_anchors_four_hour_buckets() {
        let server = MockServer::start().await;
        mount_intraday(&server, "TTD", 78).await;
        let (products, store, _) = setup(&server).await;

        let answer = products
            .intraday(IntradayRequest {
                symbols: &symbols(&["TTD"]),
                timeframe: Timeframe::FourHour,
                limit: 2,
                allow_fetch: true,
                exchange: None,
                country_code: Some("US"),
            })
            .await
            .unwrap();

        assert_eq!(answer.provider, "eodhd");
        assert_eq!(answer.fetched, vec!["TTD".to_string()]);
        assert!(answer.short_history.is_empty());
        let series = &answer.bars["TTD"];
        // One tape, one scale: the vendor's intraday series is unadjusted.
        assert!(!series.adjusted);
        assert_eq!(series.provider, "eodhd");
        // 78 bars from 09:30 ET: one full 48-bar bucket and a short 30-bar one.
        assert_eq!(series.rows.len(), 2);
        assert_eq!(series.rows[1].v, Some(30_000));
        // The first bucket opens at the session's first 5-minute bar, in exchange-local time.
        assert_eq!(series.rows[0].d, "2026-11-12T09:30:00");
        assert_eq!(series.rows[1].d, "2026-11-12T13:30:00");
        assert_eq!(series.as_of_session.as_deref(), Some("2026-11-12"));
        // Only the native rows are stored: a derived bucket would let the two disagree.
        assert_eq!(store.intraday_rows("US", "TTD"), 78);
        let usage = store.recorded_usage();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].request_type, "/intraday/");
        assert_eq!(usage[0].cost, 5);
    }

    #[tokio::test]
    async fn a_second_intraday_read_inside_the_ttl_spends_nothing() {
        let server = MockServer::start().await;
        mount_intraday(&server, "TTD", 78).await;
        let (products, _, clock) = setup(&server).await;
        let wanted = symbols(&["TTD"]);
        let request = || IntradayRequest {
            symbols: &wanted,
            timeframe: Timeframe::FiveMin,
            limit: 78,
            allow_fetch: true,
            exchange: None,
            country_code: Some("US"),
        };

        products.intraday(request()).await.unwrap();
        assert_eq!(requests(&server).await, 1);

        let answer = products.intraday(request()).await.unwrap();
        assert_eq!(requests(&server).await, 1, "inside the TTL is a cache hit");
        assert!(answer.fetched.is_empty());
        assert_eq!(answer.provider, "cache");
        assert_eq!(answer.bars["TTD"].source, "cache");

        // Past the TTL the tape is refreshed.
        clock.advance_seconds(301);
        let answer = products.intraday(request()).await.unwrap();
        assert_eq!(requests(&server).await, 2);
        assert_eq!(answer.fetched, vec!["TTD".to_string()]);
    }

    #[tokio::test]
    async fn an_intraday_cache_only_request_reports_a_symbol_with_no_tape_as_short() {
        let server = MockServer::start().await;
        let (products, _, _) = setup(&server).await;

        let answer = products
            .intraday(IntradayRequest {
                symbols: &symbols(&["TTD"]),
                timeframe: Timeframe::OneHour,
                limit: 8,
                allow_fetch: false,
                exchange: None,
                country_code: Some("US"),
            })
            .await
            .unwrap();

        assert!(answer.bars.is_empty());
        assert_eq!(answer.short_history, vec!["TTD".to_string()]);
        assert_eq!(requests(&server).await, 0, "cache-only never calls the vendor");
    }

    // -- POST /bars/bulk ---------------------------------------------------

    /// Two rows for the 2026-11-11 session, as `/eod-bulk-last-day` answers.
    fn bulk_payload() -> Value {
        json!([
            {"code": "AAPL.US", "date": "2026-11-11", "open": 230.0, "high": 233.0,
             "low": 229.0, "close": 231.7, "adjusted_close": 231.4, "volume": 51_000_000},
            {"code": "MSFT.US", "date": "2026-11-11", "open": 510.0, "high": 514.0,
             "low": 509.0, "close": 512.9, "adjusted_close": 512.7, "volume": 21_000_000}
        ])
    }

    async fn mount_bulk(server: &MockServer, date_param: &str, body: Value) {
        Mock::given(method("GET"))
            .and(path("/api/eod-bulk-last-day/US"))
            .and(query_param("date", date_param))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    fn bulk_at(session: NaiveDate) -> BulkDateKey {
        BulkDateKey::Session(session)
    }

    #[tokio::test]
    async fn a_bulk_pull_writes_the_marker_and_the_next_read_is_a_cache_answer() {
        let server = MockServer::start().await;
        mount_bulk(&server, "2026-11-11", bulk_payload()).await;
        let (products, store, _) = setup(&server).await;
        let dates = [bulk_at(date(2026, 11, 11))];

        let first = products
            .bulk_closes("US", &dates, true)
            .await
            .expect("the pull answers");
        assert_eq!(first.provider, "eodhd");
        assert!(first.partial.is_empty());
        assert_eq!(first.dates["2026-11-11"].session, "2026-11-11");
        // `c` is the adjusted close: return windows are computed from that series.
        assert_eq!(first.dates["2026-11-11"].rows["AAPL"].c, 231.4);
        assert_eq!(first.dates["2026-11-11"].rows["MSFT"].c, 512.7);
        // The completeness fact is recorded, and the rows landed in DailySnapshot.
        assert_eq!(store.bulk_pulls().len(), 1);
        assert_eq!(store.daily_rows("US", "AAPL").len(), 1);

        let second = products.bulk_closes("US", &dates, false).await.unwrap();
        assert_eq!(second.provider, "cache");
        assert_eq!(requests(&server).await, 1, "a marked session is free");
        assert_eq!(second.dates["2026-11-11"].rows["AAPL"].c, 231.4);
    }

    #[tokio::test]
    async fn a_cache_only_bulk_request_for_an_unpulled_session_is_a_404() {
        let server = MockServer::start().await;
        let (products, _, _) = setup(&server).await;

        let error = refusal(
            products
                .bulk_closes("US", &[bulk_at(date(2026, 11, 7))], false)
                .await
                .unwrap_err(),
        );
        assert_eq!(error.reason, "not-pulled");
        assert_eq!(error.status, 404);
        assert!(error.detail.contains("2026-11-07"), "{error}");
        assert!(error.detail.contains("partial session"), "{error}");
        assert_eq!(requests(&server).await, 0);
    }

    #[tokio::test]
    async fn two_uncached_bulk_dates_cost_two_pulls_and_the_second_is_cache() {
        let server = MockServer::start().await;
        mount_bulk(&server, "2026-11-11", bulk_payload()).await;
        mount_bulk(
            &server,
            "2026-08-13",
            json!([{"code": "AAPL.US", "date": "2026-08-13", "open": 218.0, "high": 220.0,
                    "low": 217.0, "close": 219.2, "adjusted_close": 218.9, "volume": 40_000_000}]),
        )
        .await;
        let (products, store, _) = setup(&server).await;

        let answer = products
            .bulk_closes(
                "US",
                &[bulk_at(date(2026, 11, 11)), bulk_at(date(2026, 8, 13))],
                true,
            )
            .await
            .unwrap();

        assert_eq!(requests(&server).await, 2);
        assert_eq!(answer.dates["2026-08-13"].rows["AAPL"].c, 218.9);
        let usage = store.recorded_usage();
        assert_eq!(usage.iter().map(|row| row.cost).sum::<i64>(), 200);
    }

    #[tokio::test]
    async fn latest_resolves_through_the_completed_session_rule() {
        let server = MockServer::start().await;
        mount_bulk(&server, "2026-11-11", bulk_payload()).await;
        let (products, _, _) = setup(&server).await;

        let answer = products
            .bulk_closes("US", &[BulkDateKey::Latest], true)
            .await
            .unwrap();

        // At 21:00 UTC on the 12th, New York is 16:00 EST: the 12th has closed but not settled
        // (close + 3h is 19:00 EST), so `latest` is the 11th — and the key stays "latest".
        assert_eq!(answer.dates["latest"].session, "2026-11-11");
        assert_eq!(requests(&server).await, 1);
    }

    #[tokio::test]
    async fn a_pull_far_below_the_previous_session_is_partial_and_writes_no_marker() {
        let server = MockServer::start().await;
        // 1,881 rows against a 27,714-row baseline is the live evidence for this rule; two
        // against ten is the same shape without the fixture.
        mount_bulk(&server, "2026-11-11", bulk_payload()).await;
        let store = Arc::new(FakeStore::new());
        store.seed_bulk_pull("US", date(2026, 11, 10), 20);
        let clock = egress_clock();
        store.set_now(clock.now_utc().naive_utc());
        let provider = Arc::new(FakeProvider::new("eodhd"));
        let client = Arc::new(
            EodhdClient::new("test-key", format!("{}/api", server.uri()), 5.0, 5.0).unwrap(),
        );
        let products = Products::new(
            common::test_settings(),
            store.clone(),
            Some(client),
            Some(common::router_with(&provider)),
            clock,
        )
        .await
        .unwrap();

        let answer = products
            .bulk_closes("US", &[bulk_at(date(2026, 11, 11))], true)
            .await
            .unwrap();

        assert_eq!(answer.partial, vec!["2026-11-11".to_string()]);
        // Upserted, but unmarked: a later cache read refuses rather than serving it as whole.
        assert_eq!(store.daily_rows("US", "AAPL").len(), 1);
        assert_eq!(store.bulk_pulls().len(), 1, "only the seeded baseline");
        let error = refusal(
            products
                .bulk_closes("US", &[bulk_at(date(2026, 11, 11))], false)
                .await
                .unwrap_err(),
        );
        assert_eq!(error.reason, "not-pulled");
    }
}
