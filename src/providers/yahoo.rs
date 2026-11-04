//! Yahoo Finance: the free fallback for equities, indices, commodities and rates.
//!
//! It is not metered, so it earns its place on capability rather than on cost: it is the only
//! provider here that can serve an index (`^IXIC`), a commodity future (`GC=F`) or the dollar
//! index (`DX-Y.NYB`), and EODHD renders several of those through US-listed ETF proxies instead.
//!
//! # What is worth knowing about this API
//!
//! 1. **It answers a rate-limit/range problem with HTTP 422, not 404.** A range longer than a
//!    young listing's history is refused rather than truncated. McTrader records the
//!    consequence as Lesson 9: a fetch loop that caught exceptions and continued silently
//!    dropped every recent listing (GEV, RDDT, ALAB) and "looked like a clean run with a
//!    slightly smaller universe". Hence [`RANGE_LADDER`]: ask wide, then narrower on refusal,
//!    and never treat a refusal as "this symbol has no history".
//! 2. **A bare request is blocked.** Yahoo serves the chart endpoint only to a browser-looking
//!    User-Agent, so one is set.
//! 3. **Nulls are normal in the arrays.** A halted session appears as `null` in every OHLC
//!    field of that index position; those positions are skipped rather than read as zero, which
//!    would put a zero low into a true range.
//!
//! No API key, no quota, no accounting — which is exactly why it is a fallback and never the
//! first choice for a US share: it is another tape, and the estate has already been bitten by a
//! silent tape switch.

use std::sync::Mutex;

use chrono::Utc;
use serde_json::Value;

use crate::domain::asset_class::AssetClass;
use crate::domain::symbols::{symbol_for_provider, venue_currency_code, ProviderId, YAHOO};
use crate::error::ProviderFailure;
use crate::providers::base::{
    epoch_date, epoch_naive, number, text, truthy, volume, Provider, ProviderBar, ProviderQuote,
};

/// The default chart endpoint. One constant so a test cannot accidentally reach production
/// while a caller-supplied base URL is what the provider actually uses.
pub const DEFAULT_BASE_URL: &str = "https://query1.finance.yahoo.com/v8/finance/chart";

/// Browser-looking, because Yahoo refuses the default client agent.
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                              (KHTML, like Gecko) Chrome/120.0 Safari/537.36";

/// Widest first. A range longer than the listing's history is refused with 422 rather than
/// shortened, so each rejection steps down instead of being read as "no history".
pub const RANGE_LADDER: [&str; 5] = ["730d", "2y", "1y", "6mo", "3mo"];

/// What this provider has done since the process started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct YahooStats {
    pub calls: u64,
    pub failures: u64,
}

pub struct YahooProvider {
    timeout_seconds: f64,
    base_url: String,
    http: reqwest::Client,
    counters: Mutex<YahooStats>,
}

impl YahooProvider {
    pub fn new(timeout_seconds: f64) -> Result<Self, ProviderFailure> {
        Self::with_base_url(timeout_seconds, DEFAULT_BASE_URL)
    }

    /// Build against an explicit endpoint. Tests point this at `wiremock`; production uses
    /// [`DEFAULT_BASE_URL`].
    pub fn with_base_url(
        timeout_seconds: f64,
        base_url: impl Into<String>,
    ) -> Result<Self, ProviderFailure> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .map_err(|error| {
                ProviderFailure::new("transport", error.without_url().to_string(), None)
            })?;
        Ok(Self {
            timeout_seconds,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http,
            counters: Mutex::new(YahooStats::default()),
        })
    }

    pub fn stats(&self) -> YahooStats {
        *self
            .counters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn count_call(&self) {
        let mut counters = self
            .counters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        counters.calls += 1;
    }

    fn count_failure(&self) {
        let mut counters = self
            .counters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        counters.failures += 1;
    }

    /// One chart request. Fails on refusal or transport failure.
    async fn chart(
        &self,
        ticker: &str,
        interval: &str,
        range: &str,
    ) -> Result<Value, ProviderFailure> {
        let url = format!("{}/{}", self.base_url, quote_ticker(ticker));
        let params = [
            ("interval", interval),
            ("range", range),
            ("includePrePost", "false"),
        ];
        self.count_call();
        let response = self
            .http
            .get(&url)
            .query(&params)
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
        if status != 200 {
            self.count_failure();
            return Err(ProviderFailure::new(
                if status == 422 {
                    "range-refused"
                } else {
                    "http-error"
                },
                format!("HTTP {status} for {ticker} range={range}"),
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
        let payload: Value = match serde_json::from_str(&body) {
            Ok(payload) => payload,
            Err(error) => {
                self.count_failure();
                return Err(ProviderFailure::new(
                    "unparseable",
                    format!("non-JSON body: {error}"),
                    None,
                ));
            }
        };

        let chart = object_or_empty(payload.get("chart"));
        if chart.get("error").is_some_and(truthy) {
            self.count_failure();
            let detail = chart
                .get("error")
                .and_then(text)
                .unwrap_or_else(|| chart.get("error").map(Value::to_string).unwrap_or_default());
            return Err(ProviderFailure::new(
                "vendor-error",
                detail.chars().take(200).collect::<String>(),
                None,
            ));
        }
        match chart.get("result").and_then(array_items) {
            Some([first, ..]) => Ok(first.clone()),
            _ => Err(ProviderFailure::new(
                "no-result",
                format!("no chart result for {ticker}"),
                None,
            )),
        }
    }

    /// Ask wide and step down on refusal, until one range yields enough sessions.
    async fn chart_with_ladder(
        &self,
        ticker: &str,
        interval: &str,
        wanted_sessions: usize,
    ) -> Result<Value, ProviderFailure> {
        let mut last: Option<ProviderFailure> = None;
        let mut best: Option<Value> = None;
        for range in RANGE_LADDER {
            match self.chart(ticker, interval, range).await {
                Ok(result) => {
                    let enough = timestamps(&result)
                        .map(|stamps| stamps.len() >= wanted_sessions)
                        .unwrap_or(false);
                    best = Some(result.clone());
                    if enough {
                        return Ok(result);
                    }
                }
                Err(error) => {
                    let retry = matches!(error.reason.as_str(), "range-refused" | "no-result");
                    last = Some(error);
                    if !retry {
                        return Err(last.expect("just assigned"));
                    }
                }
            }
        }
        match best {
            Some(result) => Ok(result),
            None => Err(last.unwrap_or_else(|| {
                ProviderFailure::new("no-result", format!("no range answered for {ticker}"), None)
            })),
        }
    }
}

#[async_trait::async_trait]
impl Provider for YahooProvider {
    fn name(&self) -> &'static str {
        YAHOO
    }

    /// Yahoo can serve everything the service distinguishes except crypto pairs it lacks.
    ///
    /// Crypto is included deliberately: Yahoo serves `BTC-USD` where Binance may not have the
    /// exact pair, so it is a genuine second source rather than a duplicate.
    fn supports(&self, _asset_class: AssetClass) -> bool {
        true
    }

    /// The latest price and the previous close.
    ///
    /// # The previous close is read from the series, not from `chartPreviousClose`
    ///
    /// `meta.chartPreviousClose` is the close *before the returned range*, which is not the same
    /// thing as the prior session. Measured on `^IXIC` with `range=1d` it came back 14,258
    /// against a 26,550 price — an 86% "move" that is simply the wrong reference. Two sessions
    /// of daily closes give an unambiguous pair: the newest close, and the one before it.
    async fn quote(
        &self,
        symbol: &str,
        asset_class: AssetClass,
        venue: Option<&str>,
    ) -> Result<Option<ProviderQuote>, ProviderFailure> {
        let Some(ticker) = symbol_for_provider(symbol, asset_class, venue, ProviderId::Yahoo)
        else {
            return Ok(None);
        };

        let mut result = None;
        let mut last_error = None;
        for range in ["5d", "1mo"] {
            match self.chart(&ticker, "1d", range).await {
                Ok(payload) => {
                    result = Some(payload);
                    break;
                }
                Err(error) => last_error = Some(error),
            }
        }
        let Some(result) = result else {
            return Err(last_error.unwrap_or_else(|| {
                ProviderFailure::new("no-result", format!("no quote for {ticker}"), None)
            }));
        };

        let meta = object_or_empty(result.get("meta"));
        let mut price = number(meta.get("regularMarketPrice").unwrap_or(&Value::Null));
        if price.is_none() {
            // Fall back to the last non-null close in the series before giving up.
            price = last_close(&result);
        }
        let Some(price) = price.filter(|price| *price > 0.0) else {
            return Ok(None);
        };

        let mut previous = previous_close_from_series(&result);
        if previous.is_none() {
            previous = number(meta.get("previousClose").unwrap_or(&Value::Null))
                .or_else(|| number(meta.get("chartPreviousClose").unwrap_or(&Value::Null)));
        }
        // Last guard: a reference more than 100% away is not a previous close, it is a data
        // fault or a corporate action. Report no reference rather than a false move.
        if previous.is_some_and(|previous| previous <= 0.0 || (price / previous - 1.0).abs() > 1.0)
        {
            previous = None;
        }

        Ok(Some(ProviderQuote {
            symbol: symbol.trim().to_uppercase(),
            price,
            provider: YAHOO.to_string(),
            previous_close: previous,
            as_of: epoch_naive(meta.get("regularMarketTime").unwrap_or(&Value::Null)),
            currency: {
                let stated = meta
                    .get("currency")
                    .filter(|currency| truthy(currency))
                    .and_then(text);
                stated.or_else(|| venue_currency_code(symbol, venue).map(str::to_string))
            },
            raw_ticker: Some(ticker),
            // Volume from the same meta block as the price, so a mover list has the traded
            // size for indices and FX too, not just for the venues EODHD prices.
            volume: volume(meta.get("regularMarketVolume").unwrap_or(&Value::Null)),
        }))
    }

    /// Completed daily bars, oldest first. The session in progress is dropped.
    async fn bars(
        &self,
        symbol: &str,
        asset_class: AssetClass,
        sessions: usize,
        venue: Option<&str>,
    ) -> Result<Vec<ProviderBar>, ProviderFailure> {
        let Some(ticker) = symbol_for_provider(symbol, asset_class, venue, ProviderId::Yahoo)
        else {
            return Ok(Vec::new());
        };

        let result = self.chart_with_ladder(&ticker, "1d", sessions).await?;
        let stamps = timestamps(&result).unwrap_or(&[]);
        let series = quote_series(&result);
        let closes = series.get("close");
        let opens = series.get("open");
        let highs = series.get("high");
        let lows = series.get("low");
        let volumes = series.get("volume");

        let today = Utc::now().date_naive();
        let mut bars = Vec::with_capacity(stamps.len());
        for (index, stamp) in stamps.iter().enumerate() {
            if stamp.is_null() {
                continue;
            }
            let Some(close) = at(closes, index) else {
                continue;
            };
            let Some(open) = at(opens, index) else {
                continue;
            };
            let Some(high) = at(highs, index) else {
                continue;
            };
            let Some(low) = at(lows, index) else {
                continue;
            };
            // A halted session is null in every field; reading it as zero would put a zero low
            // into a true range and inflate the ATR.
            if close <= 0.0 {
                continue;
            }
            let Some(session) = epoch_date(stamp) else {
                continue;
            };
            if session >= today {
                continue; // the session in progress is partial and would shrink a stop
            }
            bars.push(ProviderBar {
                session,
                open,
                high,
                low,
                close,
                volume: volume(
                    volumes
                        .and_then(|values| values.get(index))
                        .unwrap_or(&Value::Null),
                ),
                // The chart request asks for neither events nor adjusted closes, so every
                // field it returns is the raw one: close is already the traded close.
                adjusted: false,
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

fn duration_seconds(seconds: f64) -> std::time::Duration {
    std::time::Duration::try_from_secs_f64(seconds)
        .unwrap_or_else(|_| std::time::Duration::from_secs(20))
}

/// `requests.utils.quote(ticker, safe="=^.-")`: the characters in the safe set stay literal.
fn quote_ticker(ticker: &str) -> String {
    let mut encoded = String::with_capacity(ticker.len());
    for byte in ticker.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-' | b'~' | b'=' | b'^') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn object_or_empty(value: Option<&Value>) -> &serde_json::Map<String, Value> {
    static EMPTY: std::sync::OnceLock<serde_json::Map<String, Value>> = std::sync::OnceLock::new();
    match value {
        Some(Value::Object(map)) => map,
        _ => EMPTY.get_or_init(serde_json::Map::new),
    }
}

fn array_items(value: &Value) -> Option<&[Value]> {
    match value {
        Value::Array(items) => Some(items),
        _ => None,
    }
}

fn timestamps(result: &Value) -> Option<&[Value]> {
    array_items(result.get("timestamp")?)
}

fn quote_series(result: &Value) -> &serde_json::Map<String, Value> {
    let quote = result
        .get("indicators")
        .and_then(|indicators| indicators.get("quote"))
        .and_then(|quotes| quotes.get(0));
    object_or_empty(quote)
}

fn at(values: Option<&Value>, index: usize) -> Option<f64> {
    number(values?.get(index)?)
}

/// The second-newest non-null close, which is the prior session's close.
fn previous_close_from_series(result: &Value) -> Option<f64> {
    let series = quote_series(result);
    let closes: Vec<f64> = match series.get("close") {
        Some(Value::Array(items)) => items.iter().filter_map(number).collect(),
        _ => Vec::new(),
    };
    if closes.len() < 2 {
        return None;
    }
    closes.get(closes.len() - 2).copied()
}

/// The last non-null close in the series.
fn last_close(result: &Value) -> Option<f64> {
    let series = quote_series(result);
    match series.get("close") {
        Some(Value::Array(items)) => items
            .iter()
            .rev()
            .filter_map(number)
            .find(|value| *value > 0.0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Days;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider(server: &MockServer) -> YahooProvider {
        YahooProvider::with_base_url(5.0, format!("{}/v8/finance/chart", server.uri())).unwrap()
    }

    fn chart_payload(timestamps: Vec<i64>) -> Value {
        let count = timestamps.len();
        json!({
            "chart": {
                "result": [{
                    "meta": {
                        "regularMarketPrice": 343.83,
                        "previousClose": 347.10,
                        "currency": "USD",
                        "regularMarketTime": 1_761_000_000,
                        "regularMarketVolume": 5000
                    },
                    "timestamp": timestamps,
                    "indicators": {"quote": [{
                        "open": vec![100.0; count],
                        "high": vec![102.0; count],
                        "low": vec![99.0; count],
                        "close": vec![101.0; count],
                        "volume": vec![1000; count],
                    }]}
                }],
                "error": null
            }
        })
    }

    fn stamp(days_ago: u64) -> i64 {
        (Utc::now() - Days::new(days_ago)).timestamp()
    }

    #[tokio::test]
    async fn a_quote_reads_price_and_previous_close() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v8/finance/chart/AAPL"))
            .respond_with(ResponseTemplate::new(200).set_body_json(chart_payload(vec![stamp(1)])))
            .mount(&server)
            .await;

        let provider = provider(&server);
        let quote = provider
            .quote("AAPL", AssetClass::Shares, None)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(quote.price, 343.83);
        assert_eq!(quote.previous_close, Some(347.10));
        assert_eq!(quote.provider, "yahoo");
        assert_eq!(quote.currency.as_deref(), Some("USD"));
        assert_eq!(quote.volume, 5000);
        let expected = (343.83 / 347.10 - 1.0) * 100.0;
        assert!((quote.change_pct().unwrap() - expected).abs() < 1e-12);
        assert_eq!(provider.stats().calls, 1);
    }

    #[tokio::test]
    async fn a_quote_prefers_the_second_newest_series_close_over_chart_previous() {
        let server = MockServer::start().await;
        let mut payload = chart_payload(vec![stamp(2), stamp(1)]);
        payload["chart"]["result"][0]["indicators"]["quote"][0]["close"] = json!([340.0, 343.83]);
        payload["chart"]["result"][0]["meta"]["chartPreviousClose"] = json!(14_258.0);
        Mock::given(method("GET"))
            .and(path("/v8/finance/chart/AAPL"))
            .respond_with(ResponseTemplate::new(200).set_body_json(payload))
            .mount(&server)
            .await;

        let quote = provider(&server)
            .quote("AAPL", AssetClass::Shares, None)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(quote.previous_close, Some(340.0));
    }

    #[tokio::test]
    async fn a_reference_more_than_one_hundred_percent_away_is_dropped() {
        let server = MockServer::start().await;
        let mut payload = chart_payload(vec![stamp(1)]);
        payload["chart"]["result"][0]["meta"]["previousClose"] = json!(1.0);
        Mock::given(method("GET"))
            .and(path("/v8/finance/chart/AAPL"))
            .respond_with(ResponseTemplate::new(200).set_body_json(payload))
            .mount(&server)
            .await;

        let quote = provider(&server)
            .quote("AAPL", AssetClass::Shares, None)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(quote.previous_close, None);
    }

    #[tokio::test]
    async fn bars_drop_the_session_in_progress() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v8/finance/chart/AAPL"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(chart_payload(vec![stamp(3), stamp(0)])),
            )
            .mount(&server)
            .await;

        let bars = provider(&server)
            .bars("AAPL", AssetClass::Shares, 10, None)
            .await
            .unwrap();

        assert_eq!(bars.len(), 1);
        assert!(bars[0].session < Utc::now().date_naive());
        assert_eq!(bars[0].raw_close, Some(bars[0].close));
        assert!(!bars[0].adjusted);
    }

    #[tokio::test]
    async fn a_halted_session_is_absent_rather_than_a_zero() {
        let server = MockServer::start().await;
        let mut payload = chart_payload(vec![stamp(4), stamp(3), stamp(2)]);
        payload["chart"]["result"][0]["indicators"]["quote"][0]["open"][1] = Value::Null;
        payload["chart"]["result"][0]["indicators"]["quote"][0]["close"][1] = Value::Null;
        Mock::given(method("GET"))
            .and(path("/v8/finance/chart/AAPL"))
            .respond_with(ResponseTemplate::new(200).set_body_json(payload))
            .mount(&server)
            .await;

        let bars = provider(&server)
            .bars("AAPL", AssetClass::Shares, 10, None)
            .await
            .unwrap();

        assert_eq!(bars.len(), 2);
    }

    #[tokio::test]
    async fn a_422_steps_down_the_range_ladder() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v8/finance/chart/GEV"))
            .respond_with(ResponseTemplate::new(422))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v8/finance/chart/GEV"))
            .respond_with(ResponseTemplate::new(200).set_body_json(chart_payload(vec![
                stamp(3),
                stamp(2),
                stamp(1),
            ])))
            .with_priority(2)
            .mount(&server)
            .await;

        let bars = provider(&server)
            .bars("GEV", AssetClass::Shares, 2, None)
            .await
            .unwrap();

        assert_eq!(bars.len(), 2);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let ranges: Vec<String> = requests
            .iter()
            .map(|request| {
                request
                    .url
                    .query_pairs()
                    .find(|(key, _)| key == "range")
                    .map(|(_, value)| value.into_owned())
                    .unwrap()
            })
            .collect();
        assert_eq!(ranges, vec!["730d".to_string(), "2y".to_string()]);
    }

    #[tokio::test]
    async fn an_index_is_rendered_as_a_caret_ticker() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v8/finance/chart/^IXIC"))
            .respond_with(ResponseTemplate::new(200).set_body_json(chart_payload(vec![stamp(1)])))
            .mount(&server)
            .await;

        provider(&server)
            .quote("IXIC", AssetClass::Indices, None)
            .await
            .unwrap()
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].url.path().ends_with("/^IXIC"));
    }

    #[tokio::test]
    async fn a_422_on_every_range_is_reported_as_a_refusal_not_as_no_history() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v8/finance/chart/GEV"))
            .respond_with(ResponseTemplate::new(422))
            .mount(&server)
            .await;

        let error = provider(&server)
            .bars("GEV", AssetClass::Shares, 10, None)
            .await
            .unwrap_err();

        assert_eq!(error.reason, "range-refused");
        assert_eq!(server.received_requests().await.unwrap().len(), 5);
    }

    #[test]
    fn tickers_keep_the_characters_yahoo_needs() {
        assert_eq!(quote_ticker("^IXIC"), "^IXIC");
        assert_eq!(quote_ticker("GC=F"), "GC=F");
        assert_eq!(quote_ticker("DX-Y.NYB"), "DX-Y.NYB");
        assert_eq!(quote_ticker("BRK/B"), "BRK%2FB");
    }
}
