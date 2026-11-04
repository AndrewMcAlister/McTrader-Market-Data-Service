//! Binance: crypto, from the free public spot API.
//!
//! Unmetered, so like Yahoo it earns its place on capability: EODHD serves crypto as `BTC.CC`
//! and renders it less reliably than an exchange that trades it natively.
//!
//! # Two things that will bite
//!
//! 1. **The pair suffix.** McTrader renders `BTC-USD` as `BTCUSD` (strip `-` and `/`), but
//!    Binance spot quotes most pairs against **USDT**, not USD, so `BTCUSD` is not universally
//!    listed. [`pair_candidates`] tries the rendered pair first and then appends `T`, which is
//!    the difference between "crypto works" and "crypto returns nothing" for the common case.
//!    The successful pair is reported in `raw_ticker` so a caller can see which one answered.
//! 2. **A rejected pair is a 400, not an empty body.** Binance answers
//!    `{"code":-1121,"msg":"Invalid symbol."}` with HTTP 400. That is a *routing* answer, not a
//!    transport failure, so it is reported as `unknown-pair` and the router falls through
//!    rather than treating crypto as an outage.
//!
//! Daily candles are UTC-day aligned and the current one is in progress, so the newest candle
//! is dropped for the same reason the daily bars drop today: a partial candle has a partial
//! range.

use std::sync::Mutex;

use chrono::Utc;
use serde_json::Value;

use crate::domain::asset_class::AssetClass;
use crate::domain::symbols::{symbol_for_provider, ProviderId, BINANCE};
use crate::error::{py_repr, ProviderFailure};
use crate::providers::base::{
    naive_from_seconds, number, truncate_chars, volume, Provider, ProviderBar, ProviderQuote,
};

pub const BASE_URL: &str = "https://api.binance.com";
pub const TICKER_24H: &str = "/api/v3/ticker/24hr";
pub const KLINES: &str = "/api/v3/klines";

/// Binance serves ~1,000 candles per call; this is far more than any ATR needs.
pub const MAX_CANDLES: usize = 500;

/// What this provider has done since the process started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BinanceStats {
    pub calls: u64,
    pub failures: u64,
}

pub struct BinanceProvider {
    base_url: String,
    timeout_seconds: f64,
    http: reqwest::Client,
    counters: Mutex<BinanceStats>,
}

impl BinanceProvider {
    pub fn new(timeout_seconds: f64) -> Result<Self, ProviderFailure> {
        Self::with_base_url(BASE_URL, timeout_seconds)
    }

    /// Build against an explicit endpoint. Tests point this at `wiremock`; production uses
    /// [`BASE_URL`].
    pub fn with_base_url(
        base_url: impl Into<String>,
        timeout_seconds: f64,
    ) -> Result<Self, ProviderFailure> {
        let http = reqwest::Client::builder().build().map_err(|error| {
            ProviderFailure::new("transport", error.without_url().to_string(), None)
        })?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            timeout_seconds,
            http,
            counters: Mutex::new(BinanceStats::default()),
        })
    }

    pub fn stats(&self) -> BinanceStats {
        *self
            .counters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn count_call(&self) {
        self.counters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .calls += 1;
    }

    fn count_failure(&self) {
        self.counters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .failures += 1;
    }

    async fn get(&self, path: &str, params: &[(&str, String)]) -> Result<Value, ProviderFailure> {
        self.count_call();
        let response = self
            .http
            .get(format!("{}{path}", self.base_url))
            .query(params)
            .timeout(duration_seconds(self.timeout_seconds))
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                self.count_failure();
                return Err(ProviderFailure::new(
                    "transport",
                    error.without_url().to_string(),
                    None,
                ));
            }
        };

        let status = response.status().as_u16();
        if status == 400 {
            // An unlisted pair. A routing answer, not an outage.
            self.count_failure();
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderFailure::new(
                "unknown-pair",
                truncate_chars(&body, 200),
                Some(400),
            ));
        }
        if status != 200 {
            self.count_failure();
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderFailure::new(
                "http-error",
                format!("HTTP {status}: {}", truncate_chars(&body, 200)),
                Some(status),
            ));
        }
        let body = match response.text().await {
            Ok(body) => body,
            Err(error) => {
                self.count_failure();
                return Err(ProviderFailure::new(
                    "transport",
                    error.without_url().to_string(),
                    None,
                ));
            }
        };
        serde_json::from_str(&body).map_err(|error| {
            self.count_failure();
            ProviderFailure::new("unparseable", format!("non-JSON body: {error}"), None)
        })
    }

    /// Try each candidate pair; return the one that answered, with its payload.
    async fn first_answering_pair(
        &self,
        symbol: &str,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<(String, Value), ProviderFailure> {
        let candidates = pair_candidates(symbol);
        if candidates.is_empty() {
            return Err(ProviderFailure::new(
                "unrenderable",
                format!("{} is not a crypto pair", py_repr(symbol)),
                None,
            ));
        }
        let mut last: Option<ProviderFailure> = None;
        for pair in candidates {
            let mut query: Vec<(&str, String)> = params.to_vec();
            query.push(("symbol", pair.clone()));
            match self.get(path, &query).await {
                Ok(payload) => return Ok((pair, payload)),
                Err(error) if error.reason == "unknown-pair" => last = Some(error),
                Err(error) => return Err(error),
            }
        }
        Err(last.unwrap_or_else(|| {
            ProviderFailure::new(
                "unknown-pair",
                format!("no pair for {}", py_repr(symbol)),
                None,
            )
        }))
    }
}

#[async_trait::async_trait]
impl Provider for BinanceProvider {
    fn name(&self) -> &'static str {
        BINANCE
    }

    /// Crypto only. Asking it for an index is a wasted request and a misleading failure.
    fn supports(&self, asset_class: AssetClass) -> bool {
        asset_class == AssetClass::Crypto
    }

    async fn quote(
        &self,
        symbol: &str,
        _asset_class: AssetClass,
        _exchange: Option<&str>,
    ) -> Result<Option<ProviderQuote>, ProviderFailure> {
        let (pair, payload) = self.first_answering_pair(symbol, TICKER_24H, &[]).await?;
        if !payload.is_object() {
            return Ok(None);
        }
        let Some(price) =
            number(payload.get("lastPrice").unwrap_or(&Value::Null)).filter(|price| *price > 0.0)
        else {
            return Ok(None);
        };
        let previous = number(payload.get("prevClosePrice").unwrap_or(&Value::Null));
        let as_of = number(payload.get("closeTime").unwrap_or(&Value::Null))
            .filter(|stamp| *stamp != 0.0)
            .and_then(|millis| naive_from_seconds(millis / 1000.0));

        Ok(Some(ProviderQuote {
            symbol: symbol.trim().to_uppercase(),
            price,
            provider: BINANCE.to_string(),
            previous_close: previous,
            as_of,
            currency: pair.ends_with("USDT").then(|| "USDT".to_string()),
            raw_ticker: Some(pair),
            // 24h quote volume: crypto trades around the clock, so this is the window the
            // price move is measured over and the only volume that means the same thing.
            volume: volume(payload.get("volume").unwrap_or(&Value::Null)),
        }))
    }

    async fn bars(
        &self,
        symbol: &str,
        _asset_class: AssetClass,
        sessions: usize,
        _exchange: Option<&str>,
    ) -> Result<Vec<ProviderBar>, ProviderFailure> {
        let limit = sessions.saturating_add(2).clamp(1, MAX_CANDLES);
        let params = [("interval", "1d".to_string()), ("limit", limit.to_string())];
        let (_pair, payload) = self.first_answering_pair(symbol, KLINES, &params).await?;
        let Value::Array(rows) = payload else {
            return Ok(Vec::new());
        };

        let today = Utc::now().date_naive();
        let mut bars = Vec::with_capacity(rows.len());
        for row in &rows {
            let Value::Array(fields) = row else {
                continue;
            };
            if fields.len() < 6 {
                continue;
            }
            let Some(open_ms) = number(&fields[0]) else {
                continue;
            };
            let Some(open) = number(&fields[1]) else {
                continue;
            };
            let Some(high) = number(&fields[2]) else {
                continue;
            };
            let Some(low) = number(&fields[3]) else {
                continue;
            };
            let Some(close) = number(&fields[4]) else {
                continue;
            };
            if close <= 0.0 {
                continue;
            }
            let Some(session) = naive_from_seconds(open_ms / 1000.0).map(|stamp| stamp.date())
            else {
                continue;
            };
            if session >= today {
                continue; // in-progress candle: a partial range would shrink a stop
            }
            bars.push(ProviderBar {
                session,
                open,
                high,
                low,
                close,
                volume: volume(&fields[5]),
                adjusted: false,
                // Klines are the traded price and crypto has no distributions, so the close
                // is the raw close. Saying it explicitly keeps RawClose from reading as
                // "not collected" downstream.
                raw_close: Some(close),
            });
        }

        bars.sort_by_key(|bar| bar.session);
        if sessions > 0 && bars.len() > sessions {
            let excess = bars.len() - sessions;
            bars.drain(..excess);
        }
        Ok(bars)
    }
}

/// The rendered pair, then the USDT variant that spot most often lists.
///
/// `BTCUSD` is a real pair on Binance for some assets but not most; `BTCUSDT` is the liquid
/// one. Trying the literal rendering first keeps a caller who names a real pair getting
/// exactly that pair.
pub fn pair_candidates(symbol: &str) -> Vec<String> {
    let Some(rendered) = symbol_for_provider(symbol, AssetClass::Crypto, None, ProviderId::Binance)
    else {
        return Vec::new();
    };
    let mut candidates = vec![rendered.clone()];
    if rendered.ends_with("USD") {
        candidates.push(format!("{rendered}T"));
    } else if rendered.ends_with("USDT") {
        candidates.push(rendered[..rendered.len() - 1].to_string());
    }
    candidates
}

fn duration_seconds(seconds: f64) -> std::time::Duration {
    std::time::Duration::try_from_secs_f64(seconds)
        .unwrap_or_else(|_| std::time::Duration::from_secs(20))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Days;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider(server: &MockServer) -> BinanceProvider {
        BinanceProvider::with_base_url(server.uri(), 5.0).unwrap()
    }

    fn klines() -> Value {
        let now = Utc::now();
        let rows: Vec<Value> = (0..=3)
            .map(|days_ago| {
                let stamp = (now - Days::new(3 - days_ago)).timestamp() * 1000;
                json!([
                    stamp, "100.0", "110.0", "95.0", "105.0", "1234.5", 0, "0", 0, "0", "0", "0"
                ])
            })
            .collect();
        Value::Array(rows)
    }

    async fn mount_pair(server: &MockServer, body: Value) {
        Mock::given(method("GET"))
            .and(path(TICKER_24H))
            .and(query_param("symbol", "BTCUSD"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string(r#"{"code":-1121,"msg":"Invalid symbol."}"#),
            )
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(TICKER_24H))
            .and(query_param("symbol", "BTCUSDT"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn a_quote_reads_last_and_previous_close() {
        let server = MockServer::start().await;
        mount_pair(
            &server,
            json!({"symbol": "BTCUSDT", "lastPrice": "105000.5",
                   "prevClosePrice": "104000.0", "closeTime": 1_761_000_000_000i64}),
        )
        .await;

        let quote = provider(&server)
            .quote("BTC-USD", AssetClass::Crypto, None)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(quote.price, 105_000.5);
        assert_eq!(quote.previous_close, Some(104_000.0));
        assert_eq!(quote.provider, "binance");
        assert_eq!(quote.raw_ticker.as_deref(), Some("BTCUSDT"));
        assert_eq!(quote.currency.as_deref(), Some("USDT"));
        assert!(quote.as_of.is_some());
    }

    #[tokio::test]
    async fn a_usdt_variant_is_tried_after_the_literal_pair_is_refused() {
        let server = MockServer::start().await;
        mount_pair(
            &server,
            json!({"symbol": "BTCUSDT", "lastPrice": "105000.5",
                   "prevClosePrice": "104000.0", "closeTime": 1_761_000_000_000i64}),
        )
        .await;

        let quote = provider(&server)
            .quote("BTC-USD", AssetClass::Crypto, None)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(quote.raw_ticker.as_deref(), Some("BTCUSDT"));
        let requests = server.received_requests().await.unwrap();
        let symbols: Vec<String> = requests
            .iter()
            .map(|request| {
                request
                    .url
                    .query_pairs()
                    .find(|(key, _)| key == "symbol")
                    .map(|(_, value)| value.into_owned())
                    .unwrap()
            })
            .collect();
        assert_eq!(symbols, vec!["BTCUSD".to_string(), "BTCUSDT".to_string()]);
    }

    #[tokio::test]
    async fn bars_drop_the_in_progress_candle() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(KLINES))
            .and(query_param("symbol", "BTCUSD"))
            .respond_with(ResponseTemplate::new(400).set_body_string("invalid"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(KLINES))
            .and(query_param("symbol", "BTCUSDT"))
            .respond_with(ResponseTemplate::new(200).set_body_json(klines()))
            .mount(&server)
            .await;

        let bars = provider(&server)
            .bars("BTC-USD", AssetClass::Crypto, 5, None)
            .await
            .unwrap();

        assert_eq!(bars.len(), 3);
        assert!(bars.iter().all(|bar| bar.session < Utc::now().date_naive()));
        assert!(bars.iter().all(|bar| bar.raw_close == Some(bar.close)));
        assert!(bars.iter().all(|bar| !bar.adjusted));
    }

    #[tokio::test]
    async fn an_index_is_declined_by_the_routing_hook() {
        let server = MockServer::start().await;
        let provider = provider(&server);
        assert!(!provider.supports(AssetClass::Indices));
        assert!(!provider.supports(AssetClass::Shares));
        assert!(provider.supports(AssetClass::Crypto));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_blank_symbol_is_unrenderable_rather_than_a_request() {
        let server = MockServer::start().await;
        let error = provider(&server)
            .quote("   ", AssetClass::Crypto, None)
            .await
            .unwrap_err();
        assert_eq!(error.reason, "unrenderable");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[test]
    fn the_pair_ladder_adds_and_strips_the_t_suffix() {
        assert_eq!(
            pair_candidates("BTC-USD"),
            vec!["BTCUSD".to_string(), "BTCUSDT".to_string()]
        );
        assert_eq!(
            pair_candidates("BTC-USDT"),
            vec!["BTCUSDT".to_string(), "BTCUSD".to_string()]
        );
        assert_eq!(pair_candidates("ETH/BTC"), vec!["ETHBTC".to_string()]);
    }
}
