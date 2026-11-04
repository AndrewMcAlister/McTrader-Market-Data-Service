//! The two smoke tools: one exercises the data endpoints, one exercises the provider routing.
//!
//! Both talk to a **running** service and both fetch live data, so they spend vendor units.
//! That is their purpose, and why they are commands rather than tests. Every call prints its
//! HTTP status and timing before its payload, so a failure names the step that broke rather
//! than only the endpoint that answered last.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::http::{at, entries, show, Api, Reply};
use crate::ToolResult;

const GET_TIMEOUT: Duration = Duration::from_secs(90);
const POST_TIMEOUT: Duration = Duration::from_secs(120);

/// The batch `smoke-api` prices twice: the second call should be cheaper, not free.
const BATCH_SYMBOLS: [&str; 3] = ["MSFT", "NVDA", "SPY"];

/// `smoke-api`: health, one quote, a batch twice, bars, ATR, a stop, a refusal and stats.
pub(crate) async fn smoke_api(base: &str) -> ToolResult<()> {
    let mut smoke = Smoke::new(base)?;
    smoke.health().await;
    smoke.price("AAPL").await;
    smoke.prices(&BATCH_SYMBOLS).await;
    smoke.prices(&BATCH_SYMBOLS).await;
    smoke.bars(&["AAPL", "MSFT"]).await;
    smoke
        .atr(&json!({
            "symbols": ["AAPL", "MSFT", "NVDA"],
            "country_code": "US",
            "period": 14
        }))
        .await;
    smoke
        .atr(&json!({
            "symbols": ["AAPL"],
            "country_code": "US",
            "period": 14,
            "price": 231.40,
            "multiplier": 1.0,
            "direction": "long"
        }))
        .await;
    smoke
        .atr(&json!({
            "symbols": ["AAPL"],
            "country_code": "US",
            "period": 14,
            "price": 231.40
        }))
        .await;
    smoke.price("ZZZZ").await;
    smoke.stats().await;
    smoke.verdict()
}

/// `smoke-providers`: a quote from each provider's territory, then the router's counters.
pub(crate) async fn smoke_providers(base: &str) -> ToolResult<()> {
    let mut smoke = Smoke::new(base)?;
    smoke.price("AAPL").await;
    smoke.price("MSFT").await;
    smoke.price_class("BTC-USD", "crypto").await;
    smoke.price_class("ETH-USD", "crypto").await;
    smoke.price_class("IXIC", "indices").await;
    smoke.prices(&["NVDA", "SPY"]).await;
    smoke.bars_class(&["BTC-USD"], 15, "crypto").await;
    smoke
        .atr(&json!({
            "symbols": ["BTC-USD"],
            "country_code": "US",
            "period": 14,
            "asset_class": "crypto"
        }))
        .await;
    smoke.router().await;
    smoke.verdict()
}

/// One conversation with the service, remembering whether it ever went silent.
struct Smoke {
    api: Api,
    unreachable: bool,
}

impl Smoke {
    fn new(base: &str) -> ToolResult<Self> {
        Ok(Self {
            api: Api::new(base)?,
            unreachable: false,
        })
    }

    async fn health(&mut self) {
        let Some(payload) = self.get("/healthz", "GET /healthz").await else {
            return;
        };
        println!(
            "  status={} version={} vendor_configured={} key_source={}",
            show(&payload, "status"),
            show(&payload, "version"),
            show(&payload, "vendor_configured"),
            show(&payload, "key_source"),
        );
        println!(
            "  database={} ok={} error={}",
            show(&payload, "database"),
            show(&payload, "database_ok"),
            show(&payload, "database_error"),
        );
    }

    async fn price(&mut self, symbol: &str) {
        let path = format!("/price/{symbol}?country_code=US");
        if let Some(payload) = self.get(&path, &format!("GET {path}")).await {
            print_quote(symbol, &payload);
        }
    }

    async fn price_class(&mut self, symbol: &str, asset_class: &str) {
        let path = format!("/price/{symbol}?country_code=US&asset_class={asset_class}");
        if let Some(payload) = self.get(&path, &format!("GET {path}")).await {
            print_quote(symbol, &payload);
        }
    }

    async fn prices(&mut self, symbols: &[&str]) {
        let body = json!({"symbols": symbols, "country_code": "US", "exchange": "US"});
        if let Some(payload) = self.post("/prices", &body, "POST /prices").await {
            print_prices(&payload);
        }
    }

    async fn bars(&mut self, symbols: &[&str]) {
        let body = json!({"symbols": symbols, "sessions": 15, "country_code": "US"});
        if let Some(payload) = self.post("/bars", &body, "POST /bars").await {
            print_bars(&payload);
        }
    }

    async fn bars_class(&mut self, symbols: &[&str], sessions: i64, asset_class: &str) {
        let body = json!({
            "symbols": symbols,
            "sessions": sessions,
            "country_code": "US",
            "asset_class": asset_class
        });
        if let Some(payload) = self.post("/bars", &body, "POST /bars (crypto)").await {
            print_bars(&payload);
        }
    }

    async fn atr(&mut self, body: &Value) {
        let label = if body.get("price").is_some() {
            "POST /atr (with stop)"
        } else {
            "POST /atr"
        };
        if let Some(payload) = self.post("/atr", body, label).await {
            print_atr(&payload);
        }
    }

    async fn stats(&mut self) {
        if let Some(payload) = self.get("/stats", "GET /stats").await {
            print_stats(&payload);
        }
    }

    async fn router(&mut self) {
        if let Some(payload) = self.get("/stats", "GET /stats (router)").await {
            print_router(&payload);
        }
    }

    async fn get(&mut self, path: &str, label: &str) -> Option<Value> {
        println!("\n### {label}");
        let started = Instant::now();
        let result = self.api.get(path, GET_TIMEOUT).await;
        self.finish(result, started)
    }

    async fn post(&mut self, path: &str, body: &Value, label: &str) -> Option<Value> {
        println!("\n### {label}");
        let started = Instant::now();
        let result = self.api.post(path, body, POST_TIMEOUT).await;
        self.finish(result, started)
    }

    fn finish(&mut self, result: Result<Reply, String>, started: Instant) -> Option<Value> {
        match result {
            Ok(reply) => {
                let millis = started.elapsed().as_secs_f64() * 1000.0;
                println!("HTTP {} in {millis:.0} ms", reply.status);
                match reply.json() {
                    Some(payload) => {
                        if !reply.is_success() {
                            println!("  refusal: {}", compact(&payload));
                        }
                        Some(payload)
                    }
                    None => {
                        println!("  non-JSON: {}", reply.excerpt());
                        None
                    }
                }
            }
            Err(error) => {
                println!("TRANSPORT FAILURE: {error}");
                self.unreachable = true;
                None
            }
        }
    }

    fn verdict(self) -> ToolResult<()> {
        if self.unreachable {
            return Err("at least one call could not reach the service".into());
        }
        println!("\nPASS: every endpoint answered");
        Ok(())
    }
}

fn print_quote(symbol: &str, quote: &Value) {
    println!(
        "  {symbol:<8} provider={} price={} prev={} chg={} age={}s as_of_age={}s",
        show(quote, "provider"),
        show(quote, "price"),
        show(quote, "previous_close"),
        show(quote, "change_pct"),
        show(quote, "age_seconds"),
        show(quote, "as_of_age_seconds"),
    );
}

fn print_prices(payload: &Value) {
    println!(
        "  exchange={} route={} requested={} count={} complete={} missing={}",
        show(payload, "exchange"),
        show(payload, "route"),
        show(payload, "requested"),
        show(payload, "count"),
        show(payload, "complete"),
        show(payload, "missing"),
    );
    for (symbol, quote) in entries(payload, &["quotes"]) {
        print_quote(symbol, quote);
    }
}

fn print_bars(payload: &Value) {
    println!(
        "  requested_sessions={} complete={} fetched={} short_history={}",
        show(payload, "requested_sessions"),
        show(payload, "complete"),
        show(payload, "fetched"),
        show(payload, "short_history"),
    );
    for (symbol, entry) in entries(payload, &["bars"]) {
        println!(
            "  {symbol:<8} sessions={} as_of={} source={} provider={} adjusted={}",
            show(entry, "sessions"),
            show(entry, "as_of_session"),
            show(entry, "source"),
            show(entry, "provider"),
            show(entry, "adjusted"),
        );
        if let Some(row) = entry
            .get("rows")
            .and_then(Value::as_array)
            .and_then(|rows| rows.last())
        {
            println!(
                "    latest d={} o={} h={} l={} c={} cr={} v={}",
                show(row, "d"),
                show(row, "o"),
                show(row, "h"),
                show(row, "l"),
                show(row, "c"),
                show(row, "cr"),
                show(row, "v"),
            );
        }
    }
}

fn print_atr(payload: &Value) {
    println!(
        "  exchange={} period={} method={} smoothing={} complete={}",
        show(payload, "exchange"),
        show(payload, "period"),
        show(payload, "method"),
        show(payload, "smoothing"),
        show(payload, "complete"),
    );
    for (symbol, reading) in entries(payload, &["atr"]) {
        println!(
            "  {symbol:<8} value={} percent={} sessions={} as_of={} source={} stop={} reason={}",
            show(reading, "value"),
            show(reading, "percent"),
            show(reading, "sessions_used"),
            show(reading, "as_of_session"),
            show(reading, "source"),
            show(reading, "stop_level"),
            show(reading, "reason"),
        );
    }
}

fn print_stats(payload: &Value) {
    let database = at(payload, &["database"]).unwrap_or(&Value::Null);
    println!(
        "  database: {} price row(s), {} daily row(s), latest session {}",
        show(database, "price_rows"),
        show(database, "daily_rows"),
        show(database, "latest_session"),
    );
    for (name, value) in entries(payload, &["counters"]) {
        println!("  counter {name}: {value}");
    }
    let vendor = payload.get("vendor").cloned().unwrap_or(Value::Null);
    println!("  vendor units: {}", show(&vendor, "units"));
    for (purpose, calls) in entries(&vendor, &["calls_by_purpose"]) {
        println!("    {purpose}: {calls}");
    }
    print_router(payload);
}

fn print_router(payload: &Value) {
    let router = payload.get("router").cloned().unwrap_or(Value::Null);
    println!(
        "  router: fallbacks={} providers={}",
        show(&router, "fallbacks"),
        show(&router, "providers"),
    );
    for (outcome, count) in entries(&router, &["outcomes"]) {
        println!("    {outcome}: {count}");
    }
}

fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quote_summary_reads_the_contract_fields() {
        let quote = json!({
            "provider": "eodhd",
            "price": 231.4,
            "previous_close": 230.0,
            "change_pct": 0.61,
            "age_seconds": 12.0,
            "as_of_age_seconds": 3600.0
        });
        assert_eq!(show(&quote, "provider"), "eodhd");
        assert_eq!(show(&quote, "price"), "231.4");
    }

    #[test]
    fn a_batch_summary_counts_the_quotes_it_received() {
        let payload = json!({
            "exchange": "US",
            "route": "bulk",
            "requested": 2,
            "count": 2,
            "complete": true,
            "missing": [],
            "quotes": {"AAPL": {"price": 1.0}, "MSFT": {"price": 2.0}}
        });
        assert_eq!(entries(&payload, &["quotes"]).count(), 2);
        assert_eq!(show(&payload, "route"), "bulk");
    }

    #[test]
    fn a_refusal_is_rendered_compactly() {
        assert_eq!(
            compact(&json!({"error": "incomplete-stop"})),
            "{\"error\":\"incomplete-stop\"}"
        );
    }
}
