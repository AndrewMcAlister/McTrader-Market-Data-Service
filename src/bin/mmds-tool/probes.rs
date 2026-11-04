//! The three probes from the source `tools/` directory.
//!
//! They answer operational questions, not business ones: does an index quote carry the right
//! previous close, does a 150-symbol batch take the bulk route, and does the service stay
//! responsive while a heavier call runs. Each one prints its evidence before it judges, so a
//! failed probe still shows an operator what the service actually said.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use mmds::config::Settings;
use mmds::store::DailyBarRepository;

use crate::http::{show, status_text, Api};
use crate::{open_store, settings, ToolResult};

/// The widest tier the bulk route prices for one 100-unit call.
const BULK_PROBE_SYMBOLS: i64 = 150;

/// The bulk call gets longer than an interactive request: it is a whole-market answer.
const BULK_TIMEOUT: Duration = Duration::from_secs(180);
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);

/// Prove a 150-symbol batch takes the bulk route: one call, 100 units, not 150 calls.
///
/// The vendor counter is read from EODHD itself, before and after, and the service's own meter
/// from `/stats`. Both deltas are printed. The command spends 100 units by design; that is the
/// cost it exists to measure.
pub(crate) async fn probe_bulk_route(base: &str) -> ToolResult<()> {
    let settings = settings();
    let store = open_store(&settings).await?;
    let Some(session) = store.latest_session().await? else {
        return Err("DailySnapshot is empty; seed or pull a session first".into());
    };
    let symbols = store
        .top_dollar_volume_symbols(session, BULK_PROBE_SYMBOLS)
        .await?;
    let requested = symbols.len();
    println!("latest session {session}: {requested} symbol(s) from the dollar-volume tier");
    println!(
        "first five: {:?}",
        symbols.iter().take(5).collect::<Vec<_>>()
    );
    if requested < BULK_PROBE_SYMBOLS as usize {
        println!(
            "  note: fewer than {BULK_PROBE_SYMBOLS} symbols; the bulk threshold may not be met"
        );
    }

    let api = Api::new(base)?;
    let before_units = service_units(&api).await;
    let before_quota = vendor_quota(&settings).await;
    println!(
        "before: service units {}, vendor counter {}",
        render_count(before_units),
        render_count(before_quota)
    );

    let started = Instant::now();
    let reply = api
        .post(
            "/prices",
            &json!({"symbols": symbols, "country_code": "US", "exchange": "US"}),
            BULK_TIMEOUT,
        )
        .await?;
    let elapsed = started.elapsed().as_secs_f64();
    let payload = reply
        .json()
        .ok_or_else(|| format!("POST /prices returned non-JSON: {}", reply.excerpt()))?;
    let route = show(&payload, "route");
    println!(
        "answer: route={route} requested={} count={} missing={} in {elapsed:.2}s",
        show(&payload, "requested"),
        show(&payload, "count"),
        payload
            .get("missing")
            .and_then(Value::as_array)
            .map_or_else(|| "0".to_string(), |missing| missing.len().to_string())
    );

    let after_units = service_units(&api).await;
    let after_quota = vendor_quota(&settings).await;
    let service_cost = before_units
        .zip(after_units)
        .map(|(before, after)| after - before);
    let vendor_cost = before_quota
        .zip(after_quota)
        .map(|(before, after)| before - after);
    println!(
        "after : service units {} (+{}), vendor counter {} (+{})",
        render_count(after_units),
        service_cost.map_or_else(|| "?".to_string(), |cost| cost.to_string()),
        render_count(after_quota),
        vendor_cost.map_or_else(|| "?".to_string(), |cost| cost.to_string())
    );
    if let Some(cost) = service_cost.filter(|_| requested > 0) {
        println!(
            "cost per symbol: {:.4} units",
            cost as f64 / requested as f64
        );
    }

    if route != "bulk" {
        return Err(format!("expected the bulk route, saw {route:?}").into());
    }
    println!("PASS: {BULK_PROBE_SYMBOLS} symbols priced on the bulk route");
    Ok(())
}

/// Time `/healthz` alone, then again eight times while a `/bars` call is in flight.
pub(crate) async fn probe_responsiveness(base: &str) -> ToolResult<()> {
    let api = Api::new(base)?;
    println!("service: {}", api.base());

    println!("=== /healthz alone x5 ===");
    let mut solo = Vec::new();
    for _ in 0..5 {
        let timed = timed_get(&api, "/healthz").await;
        println!("  {} in {:.0} ms", timed.status, timed.millis);
        solo.push(timed);
    }

    println!();
    println!("=== 8 concurrent /healthz while POST /bars runs ===");
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let api = api.clone();
        set.spawn(async move { timed_get(&api, "/healthz").await });
    }
    let bars = timed_post(
        &api,
        "/bars",
        &json!({"symbols": ["AAPL"], "sessions": 15, "country_code": "US"}),
    )
    .await;
    let mut concurrent = Vec::new();
    while let Some(result) = set.join_next().await {
        concurrent.push(result?);
    }
    for timed in &concurrent {
        println!("  {} in {:.0} ms", timed.status, timed.millis);
    }
    println!("  POST /bars: {} in {:.0} ms", bars.status, bars.millis);

    let solo_worst = worst(&solo);
    let concurrent_worst = worst(&concurrent);
    let concurrent_median = median(&concurrent);
    println!(
        "\nhealthz alone:      worst {solo_worst:.0} ms  median {:.0} ms",
        median(&solo)
    );
    println!(
        "healthz concurrent: worst {concurrent_worst:.0} ms  median {concurrent_median:.0} ms"
    );

    if concurrent.iter().any(|timed| timed.status != "200") {
        return Err("a /healthz call failed while /bars was running".into());
    }
    if concurrent_worst >= 5_000.0 {
        return Err(format!("concurrent /healthz worst case was {concurrent_worst:.0} ms").into());
    }
    println!("PASS: health checks stayed responsive under load");
    Ok(())
}

struct Timed {
    status: String,
    millis: f64,
}

async fn timed_get(api: &Api, path: &str) -> Timed {
    let started = Instant::now();
    match api.get(path, HEALTH_TIMEOUT).await {
        Ok(reply) => Timed {
            status: status_text(&reply),
            millis: elapsed_millis(started),
        },
        Err(_) => Timed {
            status: "transport-error".to_string(),
            millis: elapsed_millis(started),
        },
    }
}

async fn timed_post(api: &Api, path: &str, body: &Value) -> Timed {
    let started = Instant::now();
    match api.post(path, body, PROBE_TIMEOUT).await {
        Ok(reply) => Timed {
            status: status_text(&reply),
            millis: elapsed_millis(started),
        },
        Err(_) => Timed {
            status: "transport-error".to_string(),
            millis: elapsed_millis(started),
        },
    }
}

fn elapsed_millis(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn worst(timings: &[Timed]) -> f64 {
    timings
        .iter()
        .map(|timed| timed.millis)
        .fold(0.0_f64, f64::max)
}

fn median(timings: &[Timed]) -> f64 {
    if timings.is_empty() {
        return 0.0;
    }
    let mut values: Vec<f64> = timings.iter().map(|timed| timed.millis).collect();
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

/// The service's own vendor meter, or `None` when no product layer is attached.
async fn service_units(api: &Api) -> Option<i64> {
    let reply = api.get("/stats", HEALTH_TIMEOUT).await.ok()?;
    let payload = reply.json()?;
    payload
        .get("vendor")
        .and_then(|vendor| vendor.get("units"))
        .and_then(Value::as_i64)
}

/// EODHD's own counter for today. The key goes in the query string, never in the output.
async fn vendor_quota(settings: &Settings) -> Option<i64> {
    if settings.eodhd_api_key.is_empty() {
        return None;
    }
    let url = format!("{}/user", settings.eodhd_base_url.trim_end_matches('/'));
    let response = reqwest::Client::new()
        .get(url)
        .query(&[
            ("api_token", settings.eodhd_api_key.as_str()),
            ("fmt", "json"),
        ])
        .timeout(HEALTH_TIMEOUT)
        .send()
        .await
        .ok()?;
    let payload: Value = response.json().await.ok()?;
    payload.get("apiRequests").and_then(Value::as_i64)
}

fn render_count(value: Option<i64>) -> String {
    value.map_or_else(|| "unknown".to_string(), |value| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timed(millis: f64) -> Timed {
        Timed {
            status: "200".to_string(),
            millis,
        }
    }

    #[test]
    fn the_worst_case_picks_the_slowest_answer() {
        assert_eq!(worst(&[timed(10.0), timed(400.0), timed(30.0)]), 400.0);
    }

    #[test]
    fn the_median_handles_both_parities() {
        assert_eq!(median(&[timed(30.0), timed(10.0), timed(20.0)]), 20.0);
        assert_eq!(
            median(&[timed(40.0), timed(10.0), timed(30.0), timed(20.0)]),
            25.0
        );
        assert_eq!(median(&[]), 0.0);
    }

    #[test]
    fn a_missing_counter_renders_as_unknown() {
        assert_eq!(render_count(None), "unknown");
        assert_eq!(render_count(Some(100)), "100");
    }
}
