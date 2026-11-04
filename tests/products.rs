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
    store.backdate_venue_prices("US", aged);

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

    // The venue has completed another session and the vendor has since published it.
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
    // {"symbols": ["AAPL"]} is the whole request: period 14, tr, simple, venue US.
    let (products, _, _, _) = setup().await;
    let wanted = symbols(&["AAPL"]);

    let result = products.atr_batch(AtrRequest::new(&wanted)).await.unwrap();

    assert_eq!(result.period, 14);
    assert_eq!(result.method, "tr");
    assert_eq!(result.smoothing, "simple");
    assert_eq!(result.venue, "US");
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
// the market: venue, country_code, or the symbol's own suffix
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_country_code_picks_the_market_the_price_is_cached_under() {
    let (products, _, store, _) = setup().await;

    let result = products
        .prices(&symbols(&["CBA"]), None, None, None, Some("AU"))
        .await
        .unwrap();

    assert_eq!(result.venue, "AU");
    assert!(store.has_price("CBA", "AU"));
    assert!(!store.has_price("CBA", "US"));
    assert_eq!(result.quotes["CBA"].venue.as_deref(), Some("AU"));
}

#[tokio::test]
async fn a_suffixed_symbol_states_its_own_market_when_no_venue_does() {
    // `CBA.AU` used to be stripped and priced at venue US: the suffix was decoration.
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

    assert_eq!(error.reason, "venue-conflict");
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

    assert_eq!(payload.bars["CBA"].venue, "AU");
    assert_eq!(store.daily_exchanges("CBA"), vec!["AU".to_string()]);
}

#[tokio::test]
async fn atr_at_a_country_code_is_cached_under_that_market() {
    let (products, _, store, _) = setup().await;
    let wanted = symbols(&["CBA"]);
    let mut request = AtrRequest::new(&wanted);
    request.country_code = Some("AU");

    let result = products.atr_batch(request).await.unwrap();

    assert_eq!(result.venue, "AU");
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
