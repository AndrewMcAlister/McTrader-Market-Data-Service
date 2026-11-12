//! Route-by-route contract tests.
//!
//! Each test drives the real router through `tower::ServiceExt::oneshot`, so it exercises the
//! same extractors, state and error rendering a socket request would. The product layer is a
//! fake, as the source suite's `FakeProducts` was: no test may reach a database, a vendor or
//! the real keyring.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use chrono::{DateTime, TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::clock::{Clock, MarketClock};
use crate::collector::CollectionProducts;
use crate::config::{NoKeyring, Settings};
use crate::error::MarketDataError;
use crate::products::{
    AtrBatchResponse, AtrRequest, BarsResponse, BulkDateKey, BulkResponse, BulkSnapshotStats,
    IntradayRequest, IntradayResponse, PricesResponse, ProductError, ProfileBatch, ProfilesResponse,
    QuotePayload, ScreenerResponse, StatsResponse,
};
use crate::schedule::{MarketRun, MarketRunner, RefreshScheduler, SessionSchedule, MARKETS};
use crate::secrets::{CredentialBackend, EodhdKeyStore, KeyringFailure};

use super::layer::ProductLayer;
use super::router::build_router;
use super::state::{AppState, Shared};

// ---------------------------------------------------------------------------
// the harness
// ---------------------------------------------------------------------------

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

async fn call(state: &AppState, request: Request<Body>) -> Reply {
    let response = build_router(state.clone())
        .oneshot(request)
        .await
        .expect("the router is infallible");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("the body is readable")
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).to_string()))
    };
    Reply {
        status,
        headers,
        body,
    }
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .body(Body::empty())
        .expect("a valid request")
}

fn delete(path: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(path)
        .body(Body::empty())
        .expect("a valid request")
}

fn post_json(path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("a valid request")
}

fn post_empty(path: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .body(Body::empty())
        .expect("a valid request")
}

fn header(mut request: Request<Body>, name: &'static str, value: &str) -> Request<Body> {
    request.headers_mut().insert(
        name,
        HeaderValue::from_str(value).expect("a valid header value"),
    );
    request
}

struct Fixed(DateTime<Utc>);

impl Clock for Fixed {
    fn now_utc(&self) -> DateTime<Utc> {
        self.0
    }
}

fn settings() -> Settings {
    let lookup = |_: &str| None::<String>;
    Settings::from_lookup(&lookup, None, &NoKeyring)
}

fn market() -> Arc<MarketClock> {
    Arc::new(MarketClock::from_file(Path::new("data/clock.json")).expect("the fixture parses"))
}

fn state_with(products: Option<Arc<dyn ProductLayer>>, admin_token: Option<&str>) -> AppState {
    let clock: Arc<dyn Clock> = Arc::new(Fixed(
        Utc.with_ymd_and_hms(2026, 10, 28, 14, 3, 0)
            .single()
            .expect("a valid instant"),
    ));
    let mut shared = Shared::new(settings(), market(), clock)
        .with_admin_token(admin_token.map(str::to_string))
        .with_key_store(Arc::new(EodhdKeyStore::with_backend(Arc::new(
            MemoryVault::default(),
        ))));
    if let Some(products) = products {
        shared = shared.with_products(products);
    }
    Arc::new(shared)
}

fn state(products: Option<Arc<dyn ProductLayer>>) -> AppState {
    state_with(products, None)
}

// ---------------------------------------------------------------------------
// the fake product layer
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeProducts {
    calls: Mutex<Vec<String>>,
    failure: Mutex<Option<ProductError>>,
}

impl FakeProducts {
    fn ok() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn failing(error: ProductError) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            failure: Mutex::new(Some(error)),
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }

    fn take_failure(&self) -> Option<ProductError> {
        self.failure.lock().unwrap().take()
    }
}

fn quote(symbol: &str, exchange: &str) -> QuotePayload {
    QuotePayload {
        symbol: Some(symbol.to_string()),
        exchange: Some(exchange.to_string()),
        price: Some(123.45),
        previous_close: None,
        change_pct: None,
        bid: None,
        ask: None,
        provider: "fake".to_string(),
        fetched_at: None,
        age_seconds: None,
        as_of: None,
        as_of_age_seconds: None,
    }
}

fn stats() -> StatsResponse {
    StatsResponse {
        counters: Default::default(),
        vendor: None,
        router: Default::default(),
        bulk_snapshot: BulkSnapshotStats {
            rows_seen: 0,
            rows_usable: 0,
            rows_dropped: 0,
            fetched_at: None,
        },
        settings: crate::products::SettingsStats {
            price_ttl_seconds: 300.0,
            bulk_breakeven: 100,
            min_sessions: 15,
            history_sessions: 560,
            atr_periods: vec![14],
            atr_method: "tr".to_string(),
            atr_smoothing: "simple".to_string(),
            database: "Market_Data".to_string(),
        },
    }
}

#[async_trait]
impl ProductLayer for FakeProducts {
    async fn price(
        &self,
        symbol: &str,
        exchange: Option<&str>,
        max_age_seconds: Option<f64>,
        asset_class: Option<&str>,
        country_code: Option<&str>,
    ) -> Result<QuotePayload, ProductError> {
        self.record(format!(
            "price:{symbol}:{exchange:?}:{max_age_seconds:?}:{asset_class:?}:{country_code:?}"
        ));
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        Ok(quote(symbol, exchange.unwrap_or("US")))
    }

    async fn bars(
        &self,
        symbols: &[String],
        sessions: i64,
        exchange: Option<&str>,
        allow_fetch: bool,
        asset_class: Option<&str>,
        country_code: Option<&str>,
    ) -> Result<BarsResponse, ProductError> {
        self.record(format!(
            "bars:{symbols:?}:{sessions}:{exchange:?}:{allow_fetch}:{asset_class:?}:{country_code:?}"
        ));
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        Ok(BarsResponse {
            bars: BTreeMap::new(),
            fetched: Vec::new(),
            short_history: Vec::new(),
            complete: true,
            requested_sessions: sessions,
        })
    }

    async fn atr_batch(&self, request: AtrRequest<'_>) -> Result<AtrBatchResponse, ProductError> {
        self.record(format!(
            "atr:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}:{}:{:?}:{:?}",
            request.symbols,
            request.period,
            request.method,
            request.smoothing,
            request.exchange,
            request.price,
            request.multiplier,
            request.direction,
            request.allow_fetch,
            request.asset_class,
            request.country_code,
        ));
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        Ok(AtrBatchResponse {
            atr: BTreeMap::new(),
            exchange: "US".to_string(),
            period: request.period.unwrap_or(14),
            method: request.method.unwrap_or("gap").to_string(),
            smoothing: request.smoothing.unwrap_or("simple").to_string(),
            complete: true,
        })
    }

    async fn stats(&self) -> StatsResponse {
        stats()
    }

    async fn screener(
        &self,
        country_code: &str,
        exchange: Option<&str>,
        filters: &[serde_json::Value],
        sort: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<ScreenerResponse, ProductError> {
        self.record(format!(
            "screener:{country_code}:{exchange:?}:{}:{sort:?}:{limit}:{offset}",
            serde_json::to_string(filters).unwrap_or_default()
        ));
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        Ok(ScreenerResponse {
            provider: "eodhd".to_string(),
            country_code: country_code.to_string(),
            exchange: exchange.unwrap_or("US").to_string(),
            filters: filters.to_vec(),
            sort: sort.map(str::to_string),
            limit,
            offset,
            count: 0,
            rows: Vec::new(),
        })
    }

    async fn profiles(&self, request: ProfileBatch<'_>) -> Result<ProfilesResponse, ProductError> {
        self.record(format!(
            "profiles:{:?}:{:?}:{:?}",
            request.symbols, request.exchange, request.country_code
        ));
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        Ok(ProfilesResponse {
            provider: "eodhd".to_string(),
            country_code: request.country_code.unwrap_or_default().to_string(),
            exchange: request.exchange.unwrap_or("US").to_string(),
            requested: request.symbols.len(),
            complete: true,
            missing: Vec::new(),
            profiles: BTreeMap::new(),
        })
    }

    async fn intraday(&self, request: IntradayRequest<'_>) -> Result<IntradayResponse, ProductError> {
        self.record(format!(
            "intraday:{:?}:{}:{}:{}:{:?}:{:?}",
            request.symbols,
            request.timeframe,
            request.limit,
            request.allow_fetch,
            request.exchange,
            request.country_code
        ));
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        Ok(IntradayResponse {
            exchange: request.exchange.unwrap_or("US").to_string(),
            provider: "cache".to_string(),
            fetched: Vec::new(),
            short_history: Vec::new(),
            bars: BTreeMap::new(),
        })
    }

    async fn bulk_closes(
        &self,
        market: &str,
        dates: &[BulkDateKey],
        allow_fetch: bool,
    ) -> Result<BulkResponse, ProductError> {
        self.record(format!(
            "bulk:{market}:{}:{allow_fetch}",
            dates
                .iter()
                .map(|key| key.as_requested())
                .collect::<Vec<_>>()
                .join("|")
        ));
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        Ok(BulkResponse {
            market: market.to_string(),
            provider: "cache".to_string(),
            partial: Vec::new(),
            dates: BTreeMap::new(),
        })
    }
}

#[async_trait]
impl CollectionProducts for FakeProducts {
    async fn refresh_snapshot(&self, _exchange: &str) -> Result<usize, ProductError> {
        Ok(0)
    }

    async fn prices(
        &self,
        symbols: &[String],
        exchange: Option<&str>,
        max_age_seconds: Option<f64>,
        asset_class: Option<&str>,
        country_code: Option<&str>,
    ) -> Result<PricesResponse, ProductError> {
        self.record(format!(
            "prices:{symbols:?}:{exchange:?}:{max_age_seconds:?}:{asset_class:?}:{country_code:?}"
        ));
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        let exchange = exchange.unwrap_or("US").to_string();
        let quotes = symbols
            .iter()
            .map(|symbol| (symbol.clone(), quote(symbol, &exchange)))
            .collect::<BTreeMap<_, _>>();
        Ok(PricesResponse {
            count: quotes.len(),
            quotes,
            exchange,
            fetched: symbols.to_vec(),
            missing: Vec::new(),
            deferred: Vec::new(),
            complete: true,
            route: "per-symbol",
            requested: symbols.len(),
        })
    }

    fn vendor(&self) -> Option<Arc<dyn crate::refresh::RefreshVendor>> {
        None
    }
}

// ---------------------------------------------------------------------------
// the fake keyring
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MemoryVault {
    value: Mutex<Option<String>>,
}

impl CredentialBackend for MemoryVault {
    fn available(&self) -> bool {
        true
    }

    fn describe(&self) -> Option<String> {
        Some("memory".to_string())
    }

    fn read(&self, _service: &str, _user: &str) -> Option<String> {
        self.value.lock().unwrap().clone()
    }

    fn write(&self, _service: &str, _user: &str, value: &str) -> Result<(), KeyringFailure> {
        *self.value.lock().unwrap() = Some(value.to_string());
        Ok(())
    }

    fn delete(&self, _service: &str, _user: &str) -> bool {
        self.value.lock().unwrap().take().is_some()
    }
}

// ---------------------------------------------------------------------------
// routing and infrastructure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_path_is_a_fastapi_shaped_404() {
    let reply = call(&state(None), get("/no-such-route")).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.body, json!({"detail": "Not Found"}));
}

#[tokio::test]
async fn the_wrong_method_is_a_fastapi_shaped_405() {
    let reply = call(&state(None), delete("/healthz")).await;
    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(reply.body, json!({"detail": "Method Not Allowed"}));
}

#[tokio::test]
async fn healthz_is_degraded_without_a_store_but_still_answers() {
    let reply = call(&state(None), get("/healthz")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["status"], "degraded");
    assert_eq!(reply.body["database_ok"], false);
    assert_eq!(reply.body["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn stats_without_a_store_is_a_named_503() {
    let reply = call(&state(None), get("/stats")).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body, json!({"error": "database-unavailable"}));
}

#[tokio::test]
async fn docs_and_the_openapi_document_are_served() {
    let state = state(None);
    // SwaggerUi serves the page at `/docs/` and redirects `/docs` there. Python's FastAPI
    // served both spellings directly; a browser cannot tell the difference, but the redirect
    // is recorded in the test so a future change to it is deliberate.
    let docs = call(&state, get("/docs")).await;
    assert_eq!(docs.status, StatusCode::SEE_OTHER);
    assert_eq!(docs.headers["location"], "/docs/");

    let page = call(&state, get("/docs/")).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.headers["content-type"]
        .to_str()
        .unwrap()
        .contains("text/html"));

    let spec = call(&state, get("/openapi.json")).await;
    assert_eq!(spec.status, StatusCode::OK);
    assert_eq!(spec.body["openapi"], "3.1.0");
    assert_eq!(spec.body["info"]["title"], "McTrader_Market_Data_Service");
}

#[tokio::test]
async fn shutdown_is_accepted_with_the_source_note() {
    let reply = call(&state(None), post_empty("/admin/shutdown")).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert_eq!(reply.body["ok"], true);
    assert_eq!(reply.body["stopping"], true);
    assert_eq!(reply.body["requested_by"], "unknown");
    assert!(reply.body["note"].as_str().unwrap().contains("stop.bat"));
}

// ---------------------------------------------------------------------------
// data endpoints
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_product_layer_is_vendor_unavailable() {
    let reply = call(&state(None), get("/price/TSLA?country_code=US")).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body, json!({"error": "vendor-unavailable"}));
}

#[tokio::test]
async fn every_symbol_endpoint_refuses_a_missing_country_code() {
    let products = FakeProducts::ok();
    let state = state(Some(products.clone()));

    let requests = [
        (get("/price/TSLA"), json!(["query", "country_code"])),
        (
            post_json("/prices", json!({"symbols": ["TSLA"]})),
            json!(["body", "country_code"]),
        ),
        (
            post_json("/atr", json!({"symbols": ["TSLA"]})),
            json!(["body", "country_code"]),
        ),
        (
            post_json("/bars", json!({"symbols": ["TSLA"]})),
            json!(["body", "country_code"]),
        ),
    ];
    for (request, location) in requests {
        let reply = call(&state, request).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(reply.body["detail"][0]["loc"], location);
        assert_eq!(reply.body["detail"][0]["type"], "missing");
    }
    assert!(products.calls().is_empty(), "{:?}", products.calls());
}

#[tokio::test]
async fn an_unknown_country_code_is_refused_before_the_product_layer() {
    let products = FakeProducts::ok();
    let reply = call(
        &state(Some(products.clone())),
        post_json("/prices", json!({"symbols": ["CBA"], "country_code": "NZ"})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["detail"][0]["type"], "literal_error");
    assert_eq!(reply.body["detail"][0]["input"], "NZ");
    assert!(products.calls().is_empty());
}

#[tokio::test]
async fn an_empty_symbol_list_is_a_422_before_the_product_layer_is_touched() {
    let products = FakeProducts::ok();
    let state = state(Some(products.clone()));
    let reply = call(
        &state,
        post_json("/prices", json!({"symbols": [], "country_code": "US"})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["detail"][0]["loc"], json!(["body", "symbols"]));
    assert!(products.calls().is_empty());
}

#[tokio::test]
async fn an_unknown_exchange_is_rendered_as_the_source_service_rendered_it() {
    let products = FakeProducts::failing(ProductError::Refused(MarketDataError::bad_request(
        "unknown-exchange",
        "could not resolve an exchange for ['VOD']",
    )));
    let reply = call(
        &state(Some(products)),
        post_json("/prices", json!({"symbols": ["VOD"], "country_code": "US"})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.body["error"], "unknown-exchange");
    assert_eq!(
        reply.body["detail"],
        "could not resolve an exchange for ['VOD']"
    );
}

#[tokio::test]
async fn a_exchange_conflict_keeps_its_reason_and_status() {
    let products = FakeProducts::failing(ProductError::Refused(MarketDataError::bad_request(
        "exchange-conflict",
        "stated exchange US disagrees with suffix .AU on CBA.AU",
    )));
    let reply = call(
        &state(Some(products)),
        post_json(
            "/prices",
            json!({"symbols": ["CBA.AU"], "exchange": "US", "country_code": "US"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.body["error"], "exchange-conflict");
}

#[tokio::test]
async fn the_country_code_is_normalised_before_it_reaches_the_product_layer() {
    let products = FakeProducts::ok();
    let reply = call(
        &state(Some(products.clone())),
        post_json(
            "/prices",
            json!({"symbols": ["CBA.AU"], "country_code": " au "}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["exchange"], "US");
    assert_eq!(
        products.calls(),
        vec!["prices:[\"CBA.AU\"]:None:None:None:Some(\"AU\")"]
    );
}

#[tokio::test]
async fn bars_default_sessions_and_fetch_flag_match_the_contract() {
    let products = FakeProducts::ok();
    let reply = call(
        &state(Some(products.clone())),
        post_json("/bars", json!({"symbols": ["TSLA"], "country_code": "US"})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["requested_sessions"], 15);
    let calls = products.calls();
    assert!(
        calls[0].starts_with("bars:[\"TSLA\"]:15:None:true:None:Some(\"US\")"),
        "{calls:?}"
    );
}

#[tokio::test]
async fn bars_refuses_a_zero_session_count_as_a_422() {
    let reply = call(
        &state(Some(FakeProducts::ok())),
        post_json(
            "/bars",
            json!({"symbols": ["TSLA"], "sessions": 0, "country_code": "US"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["detail"][0]["loc"], json!(["body", "sessions"]));
}

#[tokio::test]
async fn a_price_without_a_multiplier_keeps_the_incomplete_stop_refusal() {
    let products = FakeProducts::failing(ProductError::Refused(MarketDataError::bad_request(
        "incomplete-stop",
        "price was supplied without multiplier",
    )));
    let reply = call(
        &state(Some(products)),
        post_json(
            "/atr",
            json!({"symbols": ["TSLA"], "price": 101.5, "country_code": "US"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.body["error"], "incomplete-stop");
}

#[tokio::test]
async fn the_get_price_query_reaches_the_product_layer() {
    let products = FakeProducts::ok();
    let reply = call(
        &state(Some(products.clone())),
        get("/price/tsla?exchange=US&max_age_seconds=60&country_code=us"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["symbol"], "tsla");
    assert_eq!(
        products.calls(),
        vec!["price:tsla:Some(\"US\"):Some(60.0):None:Some(\"US\")"]
    );
}

// ---------------------------------------------------------------------------
// asset_class on the four existing routes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_absent_asset_class_is_still_shares_and_changes_nothing() {
    let products = FakeProducts::ok();
    let state = state(Some(products.clone()));
    for request in [
        get("/price/TSLA?country_code=US"),
        post_json("/prices", json!({"symbols": ["TSLA"], "country_code": "US"})),
        post_json("/atr", json!({"symbols": ["TSLA"], "country_code": "US"})),
        post_json("/bars", json!({"symbols": ["TSLA"], "country_code": "US"})),
    ] {
        let reply = call(&state, request).await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    }
    for call in products.calls() {
        assert!(call.contains(":None:"), "an absent class must stay None: {call}");
    }
}

#[tokio::test]
async fn a_stated_asset_class_reaches_the_product_layer_canonically() {
    let products = FakeProducts::ok();
    let state = state(Some(products.clone()));
    let requests = [
        get("/price/IXIC?country_code=US&asset_class=indices"),
        post_json(
            "/prices",
            json!({"symbols": ["GLD"], "country_code": "US", "asset_class": "commodities"}),
        ),
        post_json(
            "/atr",
            json!({"symbols": ["US10Y"], "country_code": "US", "asset_class": "RATES"}),
        ),
        post_json(
            "/bars",
            json!({"symbols": ["GSPC"], "country_code": "US", "asset_class": "index"}),
        ),
    ];
    for request in requests {
        let reply = call(&state, request).await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    }
    let calls = products.calls();
    assert!(calls[0].contains("Some(\"indices\")"), "{calls:?}");
    assert!(calls[1].contains("Some(\"commodities\")"), "{calls:?}");
    // An alias is forwarded as the canonical spelling, so the product layer sees one vocabulary.
    assert!(calls[2].contains("Some(\"rates\")"), "{calls:?}");
    assert!(calls[3].contains("Some(\"indices\")"), "{calls:?}");
}

#[tokio::test]
async fn an_unknown_asset_class_is_a_422_naming_the_vocabulary_on_every_route() {
    let products = FakeProducts::ok();
    let state = state(Some(products.clone()));
    let requests = [
        (get("/price/TSLA?country_code=US&asset_class=warrants"), "query"),
        (
            post_json(
                "/prices",
                json!({"symbols": ["TSLA"], "country_code": "US", "asset_class": "warrants"}),
            ),
            "body",
        ),
        (
            post_json(
                "/atr",
                json!({"symbols": ["TSLA"], "country_code": "US", "asset_class": "warrants"}),
            ),
            "body",
        ),
        (
            post_json(
                "/bars",
                json!({"symbols": ["TSLA"], "country_code": "US", "asset_class": "warrants"}),
            ),
            "body",
        ),
    ];
    for (request, _) in requests {
        let reply = call(&state, request).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(reply.body["error"], "unknown-asset-class");
        let detail = reply.body["detail"].as_str().unwrap_or_default();
        assert!(detail.contains("indices"), "{detail}");
        assert!(detail.contains("warrants"), "{detail}");
    }
    assert!(products.calls().is_empty(), "{:?}", products.calls());
}

// ---------------------------------------------------------------------------
// the screener and the profile
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_screen_without_a_country_is_refused_before_the_product_layer() {
    let products = FakeProducts::ok();
    let reply = call(
        &state(Some(products.clone())),
        post_json("/screener", json!({"filters": [["adjusted_close", ">", 1]]})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        reply.body["detail"][0]["loc"],
        json!(["body", "country_code"])
    );
    assert_eq!(reply.body["detail"][0]["type"], "missing");
    assert!(products.calls().is_empty());
}

#[tokio::test]
async fn the_screener_defaults_and_the_query_reach_the_product_layer() {
    let products = FakeProducts::ok();
    let reply = call(
        &state(Some(products.clone())),
        post_json(
            "/screener",
            json!({
                "country_code": "us",
                "exchange": "NASDAQ",
                "filters": [["market_capitalization", ">", 300000000]],
                "sort": "refund_5d_p.desc"
            }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    let expected = format!(
        "screener:US:Some(\"NASDAQ\"):{}:Some(\"refund_5d_p.desc\"):100:0",
        json!([["market_capitalization", ">", 300000000]])
    );
    assert_eq!(products.calls(), vec![expected]);
}

#[tokio::test]
async fn the_screener_bounds_are_enforced_before_the_product_layer() {
    let products = FakeProducts::ok();
    let state = state(Some(products.clone()));
    let cases = [
        (json!({"country_code": "US", "limit": 501}), json!(["body", "limit"])),
        (json!({"country_code": "US", "limit": 0}), json!(["body", "limit"])),
        (json!({"country_code": "US", "offset": -1}), json!(["body", "offset"])),
    ];
    for (body, location) in cases {
        let reply = call(&state, post_json("/screener", body)).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(reply.body["detail"][0]["loc"], location);
    }
    assert!(products.calls().is_empty());
}

#[tokio::test]
async fn a_vendor_rejection_keeps_the_422_vendor_rejected_shape() {
    let products = FakeProducts::failing(ProductError::Refused(MarketDataError::new(
        "vendor-rejected",
        "Invalid filter field: market_cap",
        422,
    )));
    let reply = call(
        &state(Some(products)),
        post_json("/screener", json!({"country_code": "US"})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["error"], "vendor-rejected");
    assert!(reply.body["detail"].as_str().unwrap().contains("market_cap"));
}

#[tokio::test]
async fn a_missing_product_layer_is_a_503_for_both_vendor_routes() {
    let state = state(None);
    for (path, body) in [
        ("/screener", json!({"country_code": "US"})),
        ("/profile", json!({"symbols": ["WDS"], "country_code": "AU"})),
        (
            "/bars/intraday",
            json!({"symbols": ["TTD"], "timeframe": "5Min", "country_code": "US"}),
        ),
        ("/bars/bulk", json!({"dates": ["latest"]})),
    ] {
        let reply = call(&state, post_json(path, body)).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert_eq!(reply.body, json!({"error": "vendor-unavailable"}), "{path}");
    }
}

#[tokio::test]
async fn the_profile_cap_is_refused_before_the_product_layer() {
    let products = FakeProducts::ok();
    let symbols: Vec<String> = (0..26).map(|index| format!("SYM{index}")).collect();
    let reply = call(
        &state(Some(products.clone())),
        post_json(
            "/profile",
            json!({"symbols": symbols, "country_code": "US"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["detail"][0]["loc"], json!(["body", "symbols"]));
    assert_eq!(reply.body["detail"][0]["type"], "too_long");
    assert!(products.calls().is_empty());
}

#[tokio::test]
async fn a_profile_request_reaches_the_product_layer_with_the_country_code() {
    let products = FakeProducts::ok();
    let reply = call(
        &state(Some(products.clone())),
        post_json(
            "/profile",
            json!({"symbols": ["WDS", "BHP"], "country_code": "au", "exchange": "ASX"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    assert_eq!(
        products.calls(),
        vec!["profiles:[\"WDS\", \"BHP\"]:Some(\"ASX\"):Some(\"AU\")"]
    );
}

// ---------------------------------------------------------------------------
// intraday and bulk bars
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_intraday_request_needs_a_timeframe_and_a_country() {
    let products = FakeProducts::ok();
    let state = state(Some(products.clone()));

    let reply = call(
        &state,
        post_json("/bars/intraday", json!({"symbols": ["TTD"], "country_code": "US"})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        reply.body["detail"][0]["loc"],
        json!(["body", "timeframe"])
    );

    let reply = call(
        &state,
        post_json(
            "/bars/intraday",
            json!({"symbols": ["TTD"], "timeframe": "1Day", "country_code": "US"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["detail"][0]["type"], "literal_error");

    assert!(products.calls().is_empty());
}

#[tokio::test]
async fn an_intraday_request_defaults_to_one_session_and_reaches_the_product_layer() {
    let products = FakeProducts::ok();
    let reply = call(
        &state(Some(products.clone())),
        post_json(
            "/bars/intraday",
            json!({"symbols": ["TTD"], "timeframe": "4h", "country_code": "us"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    assert_eq!(reply.body["provider"], "cache");
    assert_eq!(
        products.calls(),
        vec!["intraday:[\"TTD\"]:4Hour:78:true:None:Some(\"US\")"]
    );
}

#[tokio::test]
async fn a_bulk_request_needs_a_well_formed_date_and_an_allow_fetch_refusal_is_a_404() {
    let products = FakeProducts::failing(ProductError::Refused(MarketDataError::new(
        "not-pulled",
        "no completed bulk pull for US 2026-11-07; this service will not return a partial \
         session as if it were whole",
        404,
    )));
    let state = state(Some(products));

    let reply = call(
        &state,
        post_json("/bars/bulk", json!({"dates": ["2026-13-40"]})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        reply.body["detail"][0]["loc"],
        json!(["body", "dates", "0"])
    );
    assert_eq!(reply.body["detail"][0]["type"], "invalid-date");

    let reply = call(
        &state,
        post_json(
            "/bars/bulk",
            json!({"market": "US", "dates": ["2026-11-07"], "allow_fetch": false}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.body["error"], "not-pulled");
    assert!(reply.body["detail"]
        .as_str()
        .unwrap()
        .contains("partial session"));
}

#[tokio::test]
async fn a_bulk_request_collapses_duplicates_and_defaults_the_market() {
    let products = FakeProducts::ok();
    let reply = call(
        &state(Some(products.clone())),
        post_json(
            "/bars/bulk",
            json!({"dates": ["latest", "latest", "2026-08-13"]}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    assert_eq!(reply.body["market"], "US");
    assert_eq!(products.calls(), vec!["bulk:US:latest|2026-08-13:true"]);
}

#[tokio::test]
async fn a_bulk_request_over_ten_distinct_dates_is_refused() {
    let products = FakeProducts::ok();
    let dates: Vec<String> = (1..=11).map(|day| format!("2026-08-{day:02}")).collect();
    let reply = call(
        &state(Some(products.clone())),
        post_json("/bars/bulk", json!({"dates": dates})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["detail"][0]["type"], "too_long");
    assert!(products.calls().is_empty());
}

// ---------------------------------------------------------------------------
// the clock
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_market_clock_resolves_a_known_broker() {
    let reply = call(&state(None), get("/market/clock?broker_id=1")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["success"], true);
    assert_eq!(reply.body["broker"]["id"], 1);
    assert_eq!(
        reply.body["reference_time_utc"],
        "2026-10-28T14:03:00+00:00"
    );
    assert!(reply.body["exchanges"].as_array().unwrap().len() >= 5);
}

#[tokio::test]
async fn an_unknown_broker_is_a_404_whose_message_is_python_repr() {
    let reply = call(&state(None), get("/market/clock?broker_id=99")).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(
        reply.body["error"],
        "'broker 99 is not in the clock reference'"
    );
}

#[tokio::test]
async fn a_malformed_clock_time_is_a_400_with_the_parsers_sentence() {
    let reply = call(&state(None), get("/market/clock?time=9am")).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.body["error"], "time must be HH:MM, got '9am'");
}

#[tokio::test]
async fn the_market_session_names_its_reference_source() {
    let reply = call(&state(None), get("/market/session")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["broker_id"], 1);
    assert_eq!(reply.body["asset_class"], "shares");
    assert_eq!(
        reply.body["reference_source"],
        "McTrader_Live (copied 2026-11-01)"
    );
    assert!(reply.body.get("is_open").is_some());
}

#[tokio::test]
async fn trading_sessions_reads_the_time_in_the_operator_zone_by_default() {
    let reply = call(&state(None), get("/trading_sessions?time=10:00")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.body["reference_time_interpreted_in"],
        "Australia/Sydney"
    );
}

#[tokio::test]
async fn trading_sessions_rejects_a_bad_time_in_its_own_shape() {
    let reply = call(&state(None), get("/trading_sessions?time=9am")).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.body["error_type"], "validation_error");
    assert!(reply.body["error"]
        .as_str()
        .unwrap()
        .starts_with("Invalid time format. Use HH:MM"));
    assert!(reply.body["timestamp"].is_string());
}

#[tokio::test]
async fn exchanges_lists_the_reference_it_will_answer_from() {
    let reply = call(&state(None), get("/market/exchanges")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["source"], "McTrader_Live (copied 2026-11-01)");
    assert_eq!(reply.body["brokers"].as_array().unwrap().len(), 2);
    assert!(!reply.body["exchanges"].as_array().unwrap().is_empty());
    assert!(reply.body["broker_exchanges"]["1"].is_array());
    assert!(!reply.body["rules"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn the_schedule_is_unavailable_without_a_product_layer() {
    let reply = call(&state(None), get("/schedule")).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        reply.body,
        json!({"error": "no schedule: the product layer is unavailable"})
    );
}

struct FakeRunner {
    result: MarketRun,
    calls: Mutex<Vec<String>>,
}

#[async_trait]
impl MarketRunner for FakeRunner {
    async fn run_market(&self, market: &str) -> MarketRun {
        self.calls.lock().unwrap().push(market.to_string());
        self.result.clone()
    }
}

fn scheduled_state(result: MarketRun) -> (AppState, Arc<FakeRunner>) {
    let runner = Arc::new(FakeRunner {
        result,
        calls: Mutex::new(Vec::new()),
    });
    let schedule = Arc::new(SessionSchedule::new(market(), 45, 15));
    let market_runner: Arc<dyn MarketRunner> = runner.clone();
    let scheduler = Arc::new(RefreshScheduler::new(
        schedule,
        market_runner,
        MARKETS.to_vec(),
    ));
    let state = state(None);
    state.set_scheduler(scheduler);
    (state, runner)
}

#[tokio::test]
async fn an_installed_schedule_reports_both_markets_and_the_operator_settings() {
    let (state, _) = scheduled_state(MarketRun::new(true, "refreshed", ""));
    let reply = call(&state, get("/schedule")).await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["enabled"], false);
    assert_eq!(reply.body["offset_minutes"], 45);
    assert_eq!(reply.body["retry_minutes"], 15);
    let markets = reply.body["markets"].as_array().expect("a markets array");
    assert_eq!(markets.len(), 2);
    let codes: Vec<&str> = markets
        .iter()
        .filter_map(|market| market["market"].as_str())
        .collect();
    assert_eq!(codes, vec!["US", "AU"]);
}

#[tokio::test]
async fn a_manual_run_answers_200_when_it_worked() {
    let (state, runner) = scheduled_state(MarketRun::new(true, "refreshed", "collected"));
    let reply = call(&state, post_empty("/schedule/run?market=us")).await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["ok"], true);
    assert_eq!(reply.body["status"], "refreshed");
    // The endpoint uppercases, as the source did; the collector never has to normalize.
    assert_eq!(runner.calls.lock().unwrap().as_slice(), ["US"]);
}

#[tokio::test]
async fn a_manual_run_answers_409_when_it_refused() {
    let (state, _) = scheduled_state(MarketRun::new(
        false,
        "closed",
        "the exchange is not open; nothing was collected",
    ));
    let reply = call(&state, post_empty("/schedule/run?market=AU")).await;

    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.body["ok"], false);
    assert_eq!(reply.body["status"], "closed");
}

// ---------------------------------------------------------------------------
// today
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_today_endpoints_require_a_country_code() {
    let state = state(None);
    for path in ["/todays-data-is-available", "/todays-big-movers"] {
        let reply = call(&state, get(path)).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{path}");
        assert_eq!(
            reply.body["detail"][0]["loc"],
            json!(["query", "country_code"]),
            "{path}"
        );
        assert_eq!(reply.body["detail"][0]["type"], "missing", "{path}");
    }
}

#[tokio::test]
async fn an_unknown_country_is_refused_before_the_store_is_read() {
    let reply = call(
        &state(None),
        get("/todays-data-is-available?country_code=zz"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["detail"][0]["type"], "literal_error");
    assert_eq!(reply.body["detail"][0]["input"], "zz");
}

#[tokio::test]
async fn availability_without_a_store_is_a_named_503() {
    let reply = call(
        &state(None),
        get("/todays-data-is-available?country_code=us"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body, json!({"error": "database-unavailable"}));
}

#[tokio::test]
async fn movers_validates_its_filters_before_reading_anything() {
    let reply = call(&state(None), get("/todays-big-movers?limit=201")).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["detail"][0]["loc"], json!(["query", "limit"]));

    let reply = call(&state(None), get("/todays-big-movers?min_price=-1")).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        reply.body["detail"][0]["loc"],
        json!(["query", "min_price"])
    );
}

// ---------------------------------------------------------------------------
// the admin guard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_mismatched_admin_token_is_refused_with_401() {
    let state = state_with(Some(FakeProducts::ok()), Some("s3cret"));
    let reply = call(
        &state,
        header(post_empty("/refresh"), "x-admin-token", "wrong"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.body["error"], "unauthorised");
}

#[tokio::test]
async fn refresh_without_a_store_is_a_named_503() {
    let state = state_with(Some(FakeProducts::ok()), Some("s3cret"));
    let reply = call(
        &state,
        header(post_empty("/refresh"), "x-admin-token", "s3cret"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body, json!({"error": "database-unavailable"}));
}

// ---------------------------------------------------------------------------
// the vendor key
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_key_status_reports_where_the_key_is_and_only_masks_it() {
    let state = state(None);
    let reply = call(&state, get("/secrets/eodhd")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["stored"], false);
    assert_eq!(reply.body["masked"], Value::Null);
    assert_eq!(reply.body["in_use"], false);
    assert_eq!(reply.body["in_use_source"], "none");
    assert_eq!(reply.body["keyring_available"], true);
    assert_eq!(reply.body["keyring_service"], "market_data:eodhd:global");
    assert_eq!(reply.body["keyring_username"], "eodhd:api_key");
}

#[tokio::test]
async fn storing_a_key_masks_it_and_makes_it_the_live_key() {
    let state = state_with(Some(FakeProducts::ok()), Some("s3cret"));
    let reply = call(
        &state,
        header(
            post_json("/secrets/eodhd", json!({"api_key": "  secret-key-1234  "})),
            "x-admin-token",
            "s3cret",
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["stored"], true);
    assert_eq!(reply.body["vendor_configured"], true);
    assert_eq!(reply.body["status"]["masked"], "***********1234");
    assert_eq!(reply.body["status"]["length"], 15);
    assert!(!reply.body.to_string().contains("secret-key-1234"));

    let status = call(&state, get("/secrets/eodhd")).await;
    assert_eq!(status.body["stored"], true);
    assert_eq!(status.body["in_use"], true);
    assert_eq!(status.body["in_use_source"], "keyring");
}

#[tokio::test]
async fn a_blank_key_is_refused_as_invalid_key() {
    let state = state_with(None, Some("s3cret"));
    let reply = call(
        &state,
        header(
            post_json("/secrets/eodhd", json!({"api_key": "   "})),
            "x-admin-token",
            "s3cret",
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.body["error"], "invalid-key");
}

#[tokio::test]
async fn posting_a_second_key_is_refused_when_one_is_already_configured() {
    let state = state_with(Some(FakeProducts::ok()), Some("s3cret"));
    let stored = call(
        &state,
        header(
            post_json("/secrets/eodhd", json!({"api_key": "first-key-1234"})),
            "x-admin-token",
            "s3cret",
        ),
    )
    .await;
    assert_eq!(stored.status, StatusCode::OK);

    let refused = call(
        &state,
        header(
            post_json("/secrets/eodhd", json!({"api_key": "second-key-5678"})),
            "x-admin-token",
            "s3cret",
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.body["error"], "key-already-set");

    // The first key is still the one in force, unchanged.
    let status = call(&state, get("/secrets/eodhd")).await;
    assert_eq!(status.body["in_use"], true);
    assert_eq!(status.body["in_use_source"], "keyring");
    assert_eq!(status.body["length"], 14);
}
