//! The MCP server: five tools over the REST API, on stdio or streamable HTTP.
//!
//! # A proxy, not a second implementation
//!
//! Every tool forwards to `mmds-service`. If a tool reimplemented a cache policy or an ATR
//! definition, an MCP client and a REST client could be answered differently; a proxy cannot
//! drift. That is why this binary holds no store handle and no vendor client.
//!
//! # Transports
//!
//! `MDS_MCP_TRANSPORT=stdio` (the default) serves a local MCP client; `streamable-http` serves
//! a remote one at `http://<host>:<port>/mcp`. The source also accepted `sse`; rmcp 3.8 has no
//! SSE server transport, so that value is accepted, reported, and served as streamable HTTP —
//! the protocol SSE was superseded by. The difference is recorded in `TODO/Loose-Ends.md`.
//!
//! # What is logged
//!
//! Only the REST base URL and the transport. Stdio carries the MCP protocol on stdout, so all
//! logging goes to stderr; a stray `println!` would corrupt a session.

use std::process::ExitCode;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ServerCapabilities, ServerConfig};
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use serde::Deserialize;
use serde_json::{json, Value};

use mmds::logging;
use mmds::mcp::{McpProxy, INSTRUCTIONS, SERVER_NAME};

/// Any failure that stops the server from starting is fatal.
type Fatal = Box<dyn std::error::Error + Send + Sync>;

const GET_PRICE_DESCRIPTION: &str = "\
Latest price for ONE symbol, cached for five minutes, with the previous close and the \
percentage move between them. `country_code` (US or AU) is required and names the market: \
`CBA` is both an ASX and a US listing, so a ticker alone does not say which tape to read. \
`exchange` (US, AU/ASX, L, TO, HK, F, PA) is an optional second spelling of the same fact and \
must agree with it; for crypto the country is accepted and ignored, because the market is \
CRYPTO. `asset_class` picks the provider: \
shares (default) and indices/commodities/rates/forex go to EODHD then Yahoo; crypto goes \
to Binance first. The answer names the provider that served it, the market it belongs \
to and both ages -- `age_seconds` (our cache) and `as_of_age_seconds` (the vendor's \
observation, which can be older). bid/ask are always null: this vendor has no quote side.";

const GET_PRICES_DESCRIPTION: &str = "\
Latest prices for a LIST of symbols in one call, cached for five minutes. `country_code` \
(US or AU) is required and names the market, because a ticker is not unique across them \
(`CBA` is an ASX listing and a US one); one request is one market, and the answer echoes \
`exchange`. `exchange` is an optional second spelling that must agree with the country. The \
service picks the cheaper route itself from the list length, because the tariff breaks \
even at 100 symbols (1 unit per symbol against 100 flat for the whole market) -- so a \
6-symbol request and a 2,800-symbol request are both priced correctly without the caller \
knowing about it. `route` in the answer says which way it went. Symbols the vendor \
cannot price come back under `missing`, not as zeros.";

const GET_ATR_DESCRIPTION: &str = "\
Average True Range for one or more symbols, from the precomputed IndicatorCache, so it \
costs no vendor call when it has been calculated for the session. `country_code` (US or \
AU) is required and names the market -- the cache is keyed by it, so an ASX `CBA` and a US \
`CBA` have separate ATRs and neither is served for the other; `exchange` (US, AU/ASX, L, TO, \
HK, F, PA) is an optional second spelling that must agree. TRUE RANGE IS \
GAP-INCLUSIVE: max(High-Low, |High-PrevClose|, |Low-PrevClose|), averaged over the last N \
bars -- the standard method, and what every existing implementation in the estate uses. \
`method` may be 'tr' (default) or 'high_low', and `smoothing` 'simple' (default, the N-bar \
mean) or 'wilder'. Both are echoed back, because a stop level without a stated definition \
cannot be reproduced. Supply `price` and `multiplier` together to also get `stop_level`; \
the multiplier is never defaulted, since the books use 1.0, 2.0 and 3.0.";

const GET_BARS_DESCRIPTION: &str = "\
Completed daily bars for one or more symbols, from DailySnapshot, with the vendor called \
only when the cache is short. `country_code` (US or AU) is required and names the market, \
and it chooses the series: `CBA` at the ASX and `CBA` in New York are \
different instruments, so an exchange with a short series answers `short_history` rather than \
borrowing the other one's bars; `exchange` (US, AU/ASX, L, TO, HK, F, PA) is an optional \
second spelling that must agree. The session still in progress is always excluded: \
a partial session has a partial range and using it shrinks a stop exactly when the morning \
is quiet. Each answer carries `as_of_session`, `exchange`, `adjusted` (whether the series is \
split-adjusted) and `provider`. `allow_fetch=false` answers only from the cache and never \
calls a vendor.";

const GET_SERVICE_STATUS_DESCRIPTION: &str = "\
Is the market data service healthy, and what has it spent? Returns the database it is \
pointed at, whether that database is reachable, the cache hit/fetch counters, routing \
outcomes per provider, and the vendor units spent since it started. Use it before \
concluding that a missing price is a market fact.";

#[derive(Clone)]
struct MarketDataService {
    proxy: McpProxy,
    tool_router: ToolRouter<Self>,
}

impl MarketDataService {
    fn new(proxy: McpProxy) -> Self {
        Self {
            proxy,
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetPriceArgs {
    /// The ticker, optionally with a market suffix such as `CBA.AU`.
    symbol: String,
    /// Required: `US` or `AU`. A ticker alone does not name a market.
    country_code: String,
    #[serde(default)]
    exchange: Option<String>,
    #[serde(default)]
    asset_class: Option<String>,
    /// A cache age in seconds; a caller may only ask for something newer, never staler.
    #[serde(default)]
    max_age_seconds: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetPricesArgs {
    symbols: Vec<String>,
    /// Required: `US` or `AU`. A ticker alone does not name a market.
    country_code: String,
    #[serde(default)]
    exchange: Option<String>,
    #[serde(default)]
    asset_class: Option<String>,
    #[serde(default)]
    max_age_seconds: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetAtrArgs {
    symbols: Vec<String>,
    /// Required: `US` or `AU`. A ticker alone does not name a market.
    country_code: String,
    #[serde(default = "default_period")]
    period: i64,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    smoothing: Option<String>,
    #[serde(default)]
    exchange: Option<String>,
    #[serde(default)]
    asset_class: Option<String>,
    /// The price the stop is measured from. Supplying it without `multiplier` is a refusal.
    #[serde(default)]
    price: Option<f64>,
    /// The ATR multiple; never defaulted because the books use 1.0, 2.0 and 3.0.
    #[serde(default)]
    multiplier: Option<f64>,
    #[serde(default = "default_direction")]
    direction: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetBarsArgs {
    symbols: Vec<String>,
    /// Required: `US` or `AU`. A ticker alone does not name a market.
    country_code: String,
    #[serde(default = "default_sessions")]
    sessions: i64,
    #[serde(default)]
    exchange: Option<String>,
    #[serde(default)]
    asset_class: Option<String>,
    #[serde(default = "default_true")]
    allow_fetch: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetServiceStatusArgs {}

fn default_period() -> i64 {
    14
}

fn default_sessions() -> i64 {
    15
}

fn default_direction() -> String {
    "long".to_string()
}

fn default_true() -> bool {
    true
}

#[tool_router]
impl MarketDataService {
    #[tool(name = "get_price", description = GET_PRICE_DESCRIPTION)]
    async fn get_price(&self, Parameters(args): Parameters<GetPriceArgs>) -> CallToolResult {
        let mut query = Vec::new();
        push_string(&mut query, "country_code", Some(args.country_code));
        push_string(&mut query, "exchange", args.exchange);
        push_string(&mut query, "asset_class", args.asset_class);
        push_number(&mut query, "max_age_seconds", args.max_age_seconds);
        let path = format!("/price/{}", args.symbol);
        CallToolResult::structured(self.proxy.get(&path, &query).await)
    }

    #[tool(name = "get_prices", description = GET_PRICES_DESCRIPTION)]
    async fn get_prices(&self, Parameters(args): Parameters<GetPricesArgs>) -> CallToolResult {
        let mut body = json!({"symbols": args.symbols, "country_code": args.country_code});
        set_string(&mut body, "exchange", args.exchange);
        set_string(&mut body, "asset_class", args.asset_class);
        set_number(&mut body, "max_age_seconds", args.max_age_seconds);
        CallToolResult::structured(self.proxy.post("/prices", &body).await)
    }

    #[tool(name = "get_atr", description = GET_ATR_DESCRIPTION)]
    async fn get_atr(&self, Parameters(args): Parameters<GetAtrArgs>) -> CallToolResult {
        let mut body = json!({
            "symbols": args.symbols,
            "country_code": args.country_code,
            "period": args.period,
            "direction": args.direction,
        });
        set_string(&mut body, "exchange", args.exchange);
        set_string(&mut body, "method", args.method);
        set_string(&mut body, "smoothing", args.smoothing);
        set_string(&mut body, "asset_class", args.asset_class);
        set_number(&mut body, "price", args.price);
        set_number(&mut body, "multiplier", args.multiplier);
        CallToolResult::structured(self.proxy.post("/atr", &body).await)
    }

    #[tool(name = "get_bars", description = GET_BARS_DESCRIPTION)]
    async fn get_bars(&self, Parameters(args): Parameters<GetBarsArgs>) -> CallToolResult {
        let mut body = json!({
            "symbols": args.symbols,
            "country_code": args.country_code,
            "sessions": args.sessions,
            "allow_fetch": args.allow_fetch,
        });
        set_string(&mut body, "exchange", args.exchange);
        set_string(&mut body, "asset_class", args.asset_class);
        CallToolResult::structured(self.proxy.post("/bars", &body).await)
    }

    #[tool(name = "get_service_status", description = GET_SERVICE_STATUS_DESCRIPTION)]
    async fn get_service_status(
        &self,
        Parameters(_args): Parameters<GetServiceStatusArgs>,
    ) -> CallToolResult {
        CallToolResult::structured(self.proxy.get("/stats", &[]).await)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MarketDataService {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(INSTRUCTIONS)
    }
}

/// Add a query parameter only when the caller stated a non-empty value, as the source did.
fn push_string(query: &mut Vec<(&'static str, String)>, key: &'static str, value: Option<String>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        query.push((key, value));
    }
}

/// A number is present even when it is zero; only an absent value is omitted.
fn push_number(query: &mut Vec<(&'static str, String)>, key: &'static str, value: Option<f64>) {
    if let Some(value) = value {
        query.push((key, value.to_string()));
    }
}

fn set_string(body: &mut Value, key: &str, value: Option<String>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        body[key] = Value::String(value);
    }
}

fn set_number(body: &mut Value, key: &str, value: Option<f64>) {
    if let Some(value) = value.and_then(serde_json::Number::from_f64) {
        body[key] = Value::Number(value);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    Stdio,
    StreamableHttp,
}

impl Transport {
    fn from_env() -> Self {
        let requested = std::env::var("MDS_MCP_TRANSPORT")
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        match requested.as_str() {
            "streamable-http" => Self::StreamableHttp,
            "sse" => {
                tracing::warn!(
                    "MDS_MCP_TRANSPORT=sse has no transport in rmcp 3.8; serving \
                     streamable-http at /mcp instead"
                );
                Self::StreamableHttp
            }
            _ => Self::Stdio,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::StreamableHttp => "streamable-http",
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    logging::init_stderr();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("MCP server failed: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Fatal> {
    let transport = Transport::from_env();
    let proxy = McpProxy::from_env()?;
    tracing::info!(
        "{SERVER_NAME} MCP on {} (transport={})",
        proxy.base_url(),
        transport.as_str()
    );
    let handler = MarketDataService::new(proxy);
    match transport {
        Transport::Stdio => serve_stdio(handler).await,
        Transport::StreamableHttp => serve_http(handler).await,
    }
}

async fn serve_stdio(handler: MarketDataService) -> Result<(), Fatal> {
    let service = handler.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

async fn serve_http(handler: MarketDataService) -> Result<(), Fatal> {
    let address = http_address();
    let session_manager = Arc::new(LocalSessionManager::default());
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        session_manager,
        StreamableHttpServerConfig::default(),
    );
    let app = axum::Router::new().route_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(&address).await?;
    tracing::info!("streamable-http listening on http://{address}/mcp");
    axum::serve(listener, app).await?;
    Ok(())
}

/// `MDS_MCP_HOST`/`MDS_MCP_PORT`, then the source FastMCP names, then the FastMCP defaults.
fn http_address() -> String {
    resolve_address(
        environment("MDS_MCP_HOST").or_else(|| environment("FASTMCP_HOST")),
        environment("MDS_MCP_PORT").or_else(|| environment("FASTMCP_PORT")),
    )
}

fn environment(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn resolve_address(host: Option<String>, port: Option<String>) -> String {
    let host = host
        .map(|host| host.trim().to_string())
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let port = port
        .and_then(|port| port.trim().parse::<u16>().ok())
        .unwrap_or(8000);
    format!("{host}:{port}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_query_values_are_omitted_and_empty_strings_count_as_absent() {
        let mut query = Vec::new();
        push_string(&mut query, "exchange", Some("US".into()));
        push_string(&mut query, "country_code", Some(String::new()));
        push_string(&mut query, "asset_class", None);
        assert_eq!(query, vec![("exchange", "US".to_string())]);
    }

    #[test]
    fn a_zero_number_is_still_a_stated_value() {
        let mut query = Vec::new();
        push_number(&mut query, "max_age_seconds", Some(0.0));
        push_number(&mut query, "price", None);
        assert_eq!(query, vec![("max_age_seconds", "0".to_string())]);
    }

    #[test]
    fn a_body_gains_only_the_fields_the_caller_stated() {
        let mut body = json!({"symbols": ["AAPL"]});
        set_string(&mut body, "exchange", Some("US".into()));
        set_string(&mut body, "method", None);
        set_number(&mut body, "price", Some(231.4));
        assert_eq!(
            body,
            json!({"symbols": ["AAPL"], "exchange": "US", "price": 231.4})
        );
    }

    #[test]
    fn the_http_address_defaults_to_the_fastmcp_defaults() {
        assert_eq!(resolve_address(None, None), "127.0.0.1:8000");
    }

    #[test]
    fn stated_host_and_port_win_a_port_that_cannot_parse_does_not() {
        assert_eq!(
            resolve_address(Some("0.0.0.0".into()), Some("9000".into())),
            "0.0.0.0:9000"
        );
        assert_eq!(
            resolve_address(Some("localhost".into()), Some("nope".into())),
            "localhost:8000"
        );
    }

    #[test]
    fn the_defaults_match_the_source_tool_signatures() {
        assert_eq!(default_period(), 14);
        assert_eq!(default_sessions(), 15);
        assert_eq!(default_direction(), "long");
        assert!(default_true());
    }

    #[test]
    fn every_symbol_tool_refuses_a_missing_country_code() {
        assert!(serde_json::from_value::<GetPriceArgs>(json!({"symbol": "AAPL"})).is_err());
        assert!(serde_json::from_value::<GetPricesArgs>(json!({"symbols": ["AAPL"]})).is_err());
        assert!(serde_json::from_value::<GetAtrArgs>(json!({"symbols": ["AAPL"]})).is_err());
        assert!(serde_json::from_value::<GetBarsArgs>(json!({"symbols": ["AAPL"]})).is_err());
    }

    #[test]
    fn the_router_lists_exactly_the_five_source_tools() {
        let proxy = McpProxy::new("http://127.0.0.1:1", std::time::Duration::from_secs(1)).unwrap();
        let service = MarketDataService::new(proxy);
        let mut names: Vec<String> = service
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "get_atr",
                "get_bars",
                "get_price",
                "get_prices",
                "get_service_status"
            ]
        );
    }
}
