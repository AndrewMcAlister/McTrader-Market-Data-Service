//! The MCP proxy's one HTTP client, shared by the five tools.
//!
//! The MCP surface has no business logic of its own. Every tool forwards to the REST API, so an
//! MCP client and a REST client are answered by the same cache policy, the same ATR arithmetic
//! and the same provider routing. A proxy cannot drift from the service it proxies.
//!
//! This module is the part worth testing without a protocol: the calls and, more importantly,
//! the shape of a failure. A vendor refusal, an unreachable service and an unparseable body are
//! three different answers, and an MCP client must be able to tell them apart.

use std::time::Duration;

use reqwest::Method;
use serde_json::{json, Value};

/// Where the REST service listens when the environment does not say otherwise.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8003";

/// The source MCP server's request timeout.
pub const DEFAULT_TIMEOUT_SECONDS: f64 = 90.0;

/// The name an MCP client shows for this server.
pub const SERVER_NAME: &str = "McTrader_Market_Data_Service";

/// The server-level guidance the source advertised, verbatim.
pub const INSTRUCTIONS: &str = concat!(
    "Market data for the trading books: prices, daily bars and ATR, from one cache and one ",
    "ATR definition. Prefer get_prices for a few symbols and get_prices_whole_market for a ",
    "scan -- above about a hundred names the whole-market call is up to a hundred times ",
    "cheaper. Every answer states which provider served it; do not mix providers within one ",
    "decision without noting it."
);

/// A client for the running REST service.
#[derive(Clone)]
pub struct McpProxy {
    base_url: String,
    timeout: Duration,
    client: reqwest::Client,
}

impl McpProxy {
    pub fn new(base_url: impl Into<String>, timeout: Duration) -> Result<Self, reqwest::Error> {
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            timeout,
            client: reqwest::Client::builder().build()?,
        })
    }

    /// The proxy the environment describes: `MDS_MCP_BASE_URL`, defaulting to port 8003, and
    /// `MDS_MCP_TIMEOUT`, defaulting to 90 seconds.
    pub fn from_env() -> Result<Self, reqwest::Error> {
        Self::new(base_url_from_env(), timeout_from_env())
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn get(&self, path: &str, query: &[(&'static str, String)]) -> Value {
        self.request(Method::GET, path, query, None).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Value {
        self.request(Method::POST, path, &[], Some(body)).await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&'static str, String)],
        body: Option<&Value>,
    ) -> Value {
        let url = format!("{}{path}", self.base_url);
        let mut request = self.client.request(method, &url).timeout(self.timeout);
        if !query.is_empty() {
            request = request.query(query);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => return unreachable(&url, &error),
        };
        let status = response.status().as_u16();
        let text = match response.text().await {
            Ok(text) => text,
            Err(error) => return unreachable(&url, &error),
        };
        interpret(status, &text)
    }
}

/// `MDS_MCP_BASE_URL`, or the default, without a trailing slash.
pub fn base_url_from_env() -> String {
    std::env::var("MDS_MCP_BASE_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// `MDS_MCP_TIMEOUT`, or the default. A value that is not a positive number is ignored, which
/// is what the source's `except ValueError` achieved.
pub fn timeout_from_env() -> Duration {
    parse_timeout(std::env::var("MDS_MCP_TIMEOUT").ok().as_deref())
}

fn parse_timeout(text: Option<&str>) -> Duration {
    text.and_then(|text| text.trim().parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map(Duration::from_secs_f64)
        .unwrap_or_else(|| Duration::from_secs_f64(DEFAULT_TIMEOUT_SECONDS))
}

/// Turn one reply into the tool's answer, preserving the service's own refusal shape.
fn interpret(status: u16, text: &str) -> Value {
    match serde_json::from_str::<Value>(text) {
        Ok(mut payload) => {
            if status >= 400 {
                match payload.as_object_mut() {
                    Some(object) => {
                        object
                            .entry("status")
                            .or_insert_with(|| Value::from(status));
                    }
                    // A non-object error body cannot carry the status as a field, so it is
                    // reported whole rather than reshaped into something invented.
                    None => return http_error(status, text),
                }
            }
            payload
        }
        // The source reported unparseable bodies whether or not the status was an error:
        // an HTML proxy page at 200 is just as useless as one at 502.
        Err(_) => json!({
            "error": "unparseable",
            "status": status,
            "detail": clip(text, 400),
        }),
    }
}

fn unreachable(url: &str, error: &reqwest::Error) -> Value {
    json!({
        "error": "service-unreachable",
        "detail": format!("{error}"),
        "url": url,
    })
}

fn http_error(status: u16, text: &str) -> Value {
    json!({
        "error": "http-error",
        "status": status,
        "detail": clip(text, 400),
    })
}

/// At most `limit` characters, measured the way a person reads them.
fn clip(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn a_successful_get_returns_the_service_payload() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/price/AAPL"))
            .and(query_param("max_age_seconds", "60"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"price": 231.4})))
            .mount(&server)
            .await;

        let proxy = McpProxy::new(server.uri(), Duration::from_secs(5)).unwrap();
        let value = proxy
            .get("/price/AAPL", &[("max_age_seconds", "60".into())])
            .await;
        assert_eq!(value["price"], 231.4);
        assert!(value.get("error").is_none());
    }

    #[tokio::test]
    async fn a_service_refusal_keeps_its_reason_and_gains_the_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/atr"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(json!({"error": "incomplete-stop", "detail": "multiplier"})),
            )
            .mount(&server)
            .await;

        let proxy = McpProxy::new(server.uri(), Duration::from_secs(5)).unwrap();
        let value = proxy.post("/atr", &json!({"symbols": ["AAPL"]})).await;
        assert_eq!(value["error"], "incomplete-stop");
        assert_eq!(value["status"], 400);
    }

    #[tokio::test]
    async fn an_existing_status_field_is_not_overwritten() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/price/X"))
            .respond_with(
                ResponseTemplate::new(503)
                    .set_body_json(json!({"error": "database-busy", "status": 503})),
            )
            .mount(&server)
            .await;

        let proxy = McpProxy::new(server.uri(), Duration::from_secs(5)).unwrap();
        let value = proxy.get("/price/X", &[]).await;
        assert_eq!(value["status"], 503);
    }

    #[tokio::test]
    async fn a_non_json_body_is_reported_as_unparseable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/healthz"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>proxy</html>"))
            .mount(&server)
            .await;

        let proxy = McpProxy::new(server.uri(), Duration::from_secs(5)).unwrap();
        let value = proxy.get("/healthz", &[]).await;
        assert_eq!(value["error"], "unparseable");
        assert_eq!(value["status"], 200);
    }

    #[tokio::test]
    async fn a_closed_port_is_reported_as_unreachable_and_names_the_url() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let proxy = McpProxy::new(format!("http://{address}"), Duration::from_secs(2)).unwrap();
        let value = proxy.get("/healthz", &[]).await;
        assert_eq!(value["error"], "service-unreachable");
        assert!(value["url"].as_str().unwrap().ends_with("/healthz"));
    }

    #[test]
    fn a_missing_or_invalid_timeout_falls_back_to_the_source_default() {
        assert_eq!(
            parse_timeout(None),
            Duration::from_secs_f64(DEFAULT_TIMEOUT_SECONDS)
        );
        assert_eq!(
            parse_timeout(Some("nineteeen")),
            Duration::from_secs_f64(DEFAULT_TIMEOUT_SECONDS)
        );
        assert_eq!(parse_timeout(Some("5")), Duration::from_secs(5));
    }

    #[test]
    fn a_non_positive_timeout_is_ignored() {
        for text in ["0", "-1", "NaN"] {
            assert_eq!(
                parse_timeout(Some(text)),
                Duration::from_secs_f64(DEFAULT_TIMEOUT_SECONDS),
                "{text}"
            );
        }
    }

    #[test]
    fn a_long_error_body_is_clipped_at_four_hundred_characters() {
        let value = interpret(502, &"x".repeat(1_000));
        assert_eq!(value["error"], "unparseable");
        assert_eq!(value["detail"].as_str().unwrap().chars().count(), 400);
    }

    #[test]
    fn a_non_object_error_body_becomes_an_http_error() {
        let value = interpret(500, "[1, 2, 3]");
        assert_eq!(value["error"], "http-error");
        assert_eq!(value["status"], 500);
    }
}
