//! The EODHD client: five vendor calls, and no blind retries.
//!
//! Everything the service needs from the vendor is one of five requests:
//!
//! | Purpose                    | Request                                          | Billed      |
//! |----------------------------|--------------------------------------------------|-------------|
//! | one symbol's latest price  | `/real-time/{SYM}.{EX}`                          | 1/symbol    |
//! | the whole US market        | `/real-time/{placeholder}.US?ex=US`              | 100 flat    |
//! | one symbol's daily history | `/eod/{SYM}.{EX}?from=...&period=d`              | 1 flat      |
//! | one symbol's intraday bars | `/intraday/{SYM}.{EX}?interval=5m&from=...`      | 5 flat      |
//! | one session, whole market  | `/eod-bulk-last-day/{MARKET}?date=YYYY-MM-DD`    | 100 flat    |
//!
//! Two facts about the tariff drive the code:
//!
//! 1. **A rejected request is still billed** (measured: a 404 from `/eod/` cost 1 unit, a 404
//!    from `/intraday/` cost 5). So a 4xx is never retried — retrying a malformed request pays
//!    again for the same mistake. The one exception is 429: a rate-limit refusal is not a
//!    request the vendor processed, so it is neither billed nor a mistake. It is waited out and
//!    retried, honouring `Retry-After` when the vendor sends one.
//! 2. **`/eod/` is flat regardless of window**, so the history call asks for the whole window
//!    it needs in one request rather than paging.
//!
//! There is deliberately no caching in this module. The cache lives in `products`, which is
//! the layer that knows about TTLs; a cache here would be a second lifetime for the same data.
//!
//! The API key travels in the query string, so no transport error is ever surfaced with its
//! URL attached: `without_url` strips it before the message reaches a log or a caller.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use chrono::{Days, Local, NaiveDate, Utc};
use reqwest::header::RETRY_AFTER;
use serde::Serialize;
use serde_json::Value;

use crate::config::EODHD_BASE_URL;
use crate::domain::venues::normalise_venue_code;
use crate::error::{py_repr, ProviderFailure};
use crate::providers::base::truncate_chars;
use crate::usage::{request_type_for, UsageRecorder};

/// The only venue the whole-market real-time feed was measured for. A request for BHP.AU
/// returns nothing from it, so other venues are refused rather than priced off a US tape.
pub const BULK_REAL_TIME_VENUE: &str = "US";

/// The path ticker for the `ex=` form. EODHD ignores it once `ex` is set; it is present
/// because the path is not optional. Fixed, so no caller's symbol can end up in it.
pub const BULK_PLACEHOLDER: &str = "AAPL";

/// A 429 means the vendor refused the request before doing any work. It is not billed, so
/// retrying it costs nothing but time. The cap keeps a sustained rate-limit from hanging a
/// caller forever; the backfill also throttles itself so this is a safety net, not the plan.
pub const MAX_RATE_LIMIT_RETRIES: u32 = 6;

/// What one client has spent since the process started.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct EodhdStats {
    pub calls_by_purpose: BTreeMap<String, u64>,
    pub units: i64,
}

struct Counters {
    stats: EodhdStats,
}

/// The five vendor calls, with the meter attached.
///
/// Cloning is not offered: the counters are a single process-local meter, and two clones
/// would report two different spend totals for one service.
pub struct EodhdClient {
    api_key: String,
    base_url: String,
    timeout: Duration,
    bulk_timeout: Duration,
    http: reqwest::Client,
    counters: Mutex<Counters>,
    /// Interior-mutable because the ledger is attached after construction: the client is
    /// built first, the tariff is read from the database, and only then can the ledger price
    /// a call. It is read on every billable call and written once at startup (or per request
    /// by the API).
    recorder: RwLock<Option<Arc<dyn UsageRecorder>>>,
}

impl EodhdClient {
    /// Build a client, refusing to exist without a key.
    ///
    /// A missing key is a startup error, not a per-call one: discovering it on the first
    /// request would turn a configuration mistake into a stream of 502s.
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        timeout_seconds: f64,
        bulk_timeout_seconds: f64,
    ) -> Result<Self, ProviderFailure> {
        let api_key = api_key.into();
        if api_key.is_empty() {
            return Err(ProviderFailure::new(
                "no-api-key",
                "no EODHD key: set EODHD_API_KEY or store it in the keyring at \
                 market_data:eodhd:global / eodhd:api_key",
                None,
            ));
        }
        let base_url = {
            let raw = base_url.into();
            let trimmed = raw.trim_end_matches('/');
            if trimmed.is_empty() {
                EODHD_BASE_URL.to_string()
            } else {
                trimmed.to_string()
            }
        };
        let http = reqwest::Client::builder().build().map_err(|error| {
            ProviderFailure::new("transport", error.without_url().to_string(), None)
        })?;
        Ok(Self {
            api_key,
            base_url,
            timeout: duration_or_default(timeout_seconds, 30.0),
            bulk_timeout: duration_or_default(bulk_timeout_seconds, 60.0),
            http,
            counters: Mutex::new(Counters {
                stats: EodhdStats::default(),
            }),
            recorder: RwLock::new(None),
        })
    }

    /// Attach the tariff-backed ledger. Without one the literal unit counts are the meter.
    pub fn with_recorder(self, recorder: Arc<dyn UsageRecorder>) -> Self {
        self.set_recorder(recorder);
        self
    }

    /// Replace the ledger after construction.
    ///
    /// The API rebinds this per request to the caller named in `X-MarketData-Caller`; the
    /// default attached at startup attributes spend made outside a request (the scheduler,
    /// the daily job) to the task-local caller or `unknown`.
    pub fn set_recorder(&self, recorder: Arc<dyn UsageRecorder>) {
        let mut slot = self
            .recorder
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = Some(recorder);
    }

    pub fn stats(&self) -> EodhdStats {
        self.lock_counters().stats.clone()
    }

    fn lock_counters(&self) -> std::sync::MutexGuard<'_, Counters> {
        self.counters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Count one billable call, priced from the tariff when a recorder is attached.
    ///
    /// The tariff wins over the literal `units` because `dbo.RequestTypes` carries the
    /// measurement and its evidence: a re-measured price should be a row update, not a code
    /// change. `request_type` overrides the purpose's default for a call whose tariff row
    /// depends on what was asked: an ASX bulk pull is `/eod-bulk-last-day/AU`, and filing it
    /// under the US row is how the per-market freshness check reads another market's spend.
    async fn spent(&self, purpose: &str, units: i64, symbols: i64, request_type: Option<&str>) {
        let request_type = request_type
            .map(str::to_string)
            .or_else(|| request_type_for(purpose).map(str::to_string));
        let mut cost = units;
        let recorder = self
            .recorder
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let (Some(recorder), Some(request_type)) = (recorder, request_type.as_deref()) {
            if let Some(priced) = recorder.record(request_type, symbols).await {
                cost = priced;
            }
        }
        let mut counters = self.lock_counters();
        *counters
            .stats
            .calls_by_purpose
            .entry(purpose.to_string())
            .or_insert(0) += 1;
        counters.stats.units += cost;
    }

    /// One GET, at most one transport retry, and never a retry on a billed 4xx.
    ///
    /// A 4xx is billed, so retrying it pays twice for the same rejected question. Only a
    /// transport-level failure is retried, and only once, because the second attempt is free
    /// in the case that matters (the request never landed).
    async fn get_json(
        &self,
        path: &str,
        params: &[(String, String)],
        billing: Billing<'_>,
        timeout: Duration,
    ) -> Result<Value, ProviderFailure> {
        let mut query: Vec<(String, String)> = params.to_vec();
        query.push(("api_token".to_string(), self.api_key.clone()));
        if !query.iter().any(|(key, _)| key == "fmt") {
            query.push(("fmt".to_string(), "json".to_string()));
        }
        let url = format!("{}/{path}", self.base_url);

        let mut transport_attempt = 0;
        let mut rate_limit_attempt = 0;
        loop {
            let response = match self
                .http
                .get(&url)
                .query(&query)
                .timeout(timeout)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    transport_attempt += 1;
                    if transport_attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        continue;
                    }
                    return Err(ProviderFailure::new(
                        "transport",
                        error.without_url().to_string(),
                        None,
                    ));
                }
            };

            let status = response.status();
            if status.is_success() {
                let body = match response.text().await {
                    Ok(body) => body,
                    Err(error) => {
                        return Err(ProviderFailure::new(
                            "transport",
                            error.without_url().to_string(),
                            None,
                        ))
                    }
                };
                let payload = serde_json::from_str::<Value>(&body).map_err(|error| {
                    ProviderFailure::new(
                        "unparseable",
                        format!("200 with a non-JSON body ({error})"),
                        Some(200),
                    )
                })?;
                self.spent(
                    billing.purpose,
                    billing.units,
                    billing.symbols,
                    billing.request_type,
                )
                .await;
                return Ok(payload);
            }

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                // Refused before processing, so not billed. Retry after the window the
                // vendor states; a blind immediate retry would just be refused again.
                rate_limit_attempt += 1;
                if rate_limit_attempt > MAX_RATE_LIMIT_RETRIES {
                    return Err(ProviderFailure::new(
                        "rate-limited",
                        format!("HTTP 429 after {MAX_RATE_LIMIT_RETRIES} retries for /{path}"),
                        Some(429),
                    ));
                }
                let retry_after = response
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|value| value.to_str().ok());
                let delay = retry_after_seconds(retry_after, rate_limit_attempt);
                tokio::time::sleep(duration_or_default(delay, 1.0)).await;
                continue;
            }

            // 4xx/5xx: billed, so no retry -- and billed, so it is recorded before raising.
            // Measured: a 404 from /eod/ cost 1 and a 404 from /intraday/ cost 5. A transport
            // failure above is the one other case that costs nothing, because the request
            // never landed; a 429 above costs nothing because the vendor refused to process it.
            let text = response.text().await.unwrap_or_default();
            self.spent(
                billing.purpose,
                billing.units,
                billing.symbols,
                billing.request_type,
            )
            .await;
            return Err(ProviderFailure::new(
                "http-error",
                format!(
                    "HTTP {} for /{path}: {}",
                    status.as_u16(),
                    truncate_chars(&text, 200)
                ),
                Some(status.as_u16()),
            ));
        }
    }

    // -- the five calls ----------------------------------------------------

    /// One symbol's latest price. Billed 1.
    pub async fn real_time(
        &self,
        symbol: &str,
        venue: Option<&str>,
    ) -> Result<Value, ProviderFailure> {
        let ticker = render_symbol(symbol, venue)?;
        let payload = self
            .get_json(
                &format!("real-time/{ticker}"),
                &[],
                Billing::new("real_time", 1, 1),
                self.timeout,
            )
            .await?;
        let payload = match payload {
            Value::Array(mut rows) => {
                if rows.is_empty() {
                    Value::Object(Default::default())
                } else {
                    rows.remove(0)
                }
            }
            other => other,
        };
        if !payload.is_object() {
            return Err(ProviderFailure::new(
                "unparseable",
                format!("expected an object, got {}", json_type(&payload)),
                None,
            ));
        }
        Ok(payload)
    }

    /// Every US listing's latest price in one request. Billed 100 flat.
    ///
    /// Returns the raw rows unfiltered: the feed carries inactive and delisted symbols
    /// holding the last price they ever traded at, and deciding what is too old is a policy
    /// call that belongs to the caller, where the freshness rule is known.
    pub async fn real_time_whole_market(&self, venue: &str) -> Result<Vec<Value>, ProviderFailure> {
        let market = normalise_venue(Some(venue))?;
        if market != BULK_REAL_TIME_VENUE {
            return Err(ProviderFailure::new(
                "non-us-venue",
                format!(
                    "the whole-market real-time feed was measured for {BULK_REAL_TIME_VENUE} \
                     only; {} is refused rather than priced off a US tape",
                    py_repr(venue)
                ),
                None,
            ));
        }
        let payload = self
            .get_json(
                &format!("real-time/{BULK_PLACEHOLDER}.{BULK_REAL_TIME_VENUE}"),
                &[("ex".to_string(), BULK_REAL_TIME_VENUE.to_string())],
                Billing::new("real_time_bulk", 100, 0),
                self.bulk_timeout,
            )
            .await?;
        expect_array(payload)
    }

    /// One symbol's daily bars, oldest first. Billed 1 whatever the window.
    ///
    /// Asks for a window generous enough to supply `period + 1` sessions after weekends and
    /// holidays, because a wider window costs exactly the same as a narrow one.
    pub async fn eod(
        &self,
        symbol: &str,
        venue: Option<&str>,
        days: i64,
        to_date: Option<NaiveDate>,
    ) -> Result<Vec<Value>, ProviderFailure> {
        let ticker = render_symbol(symbol, venue)?;
        let end = to_date.unwrap_or_else(|| Local::now().date_naive());
        let start = end
            .checked_sub_days(Days::new(days.max(1) as u64))
            .unwrap_or(end);
        let payload = self
            .get_json(
                &format!("eod/{ticker}"),
                &[
                    ("from".to_string(), start.format("%Y-%m-%d").to_string()),
                    ("to".to_string(), end.format("%Y-%m-%d").to_string()),
                    ("period".to_string(), "d".to_string()),
                    ("order".to_string(), "a".to_string()),
                ],
                Billing::new("eod", 1, 1),
                self.timeout,
            )
            .await?;
        expect_array(payload)
    }

    /// One symbol's intraday bars, oldest first. Billed 5 flat per call.
    ///
    /// `start` and `end` are epoch seconds, not dates: the vendor rejects a date-string
    /// window with HTTP 422, which reads like a range problem and is not one. The 5m interval
    /// accepts at most 600 days in one request; a wider window is refused rather than silently
    /// truncated.
    pub async fn intraday(
        &self,
        symbol: &str,
        venue: Option<&str>,
        interval: &str,
        start: i64,
        end: i64,
    ) -> Result<Vec<Value>, ProviderFailure> {
        let ticker = render_symbol(symbol, venue)?;
        let now = Utc::now().timestamp();
        let end = if end == 0 { now } else { end };
        if start == 0 {
            return Err(ProviderFailure::new(
                "empty-window",
                "intraday needs an explicit epoch start; a default would hide a caller \
                 asking for the wrong range",
                None,
            ));
        }
        if end - start > 600 * 86_400 {
            return Err(ProviderFailure::new(
                "window-too-wide",
                format!(
                    "5m intraday accepts at most 600 days; the request spans {:.1} days",
                    (end - start) as f64 / 86_400.0
                ),
                None,
            ));
        }
        let payload = self
            .get_json(
                &format!("intraday/{ticker}"),
                &[
                    ("interval".to_string(), interval.to_string()),
                    ("from".to_string(), start.to_string()),
                    ("to".to_string(), end.to_string()),
                ],
                Billing::new("intraday", 5, 1),
                self.bulk_timeout,
            )
            .await?;
        expect_array(payload)
    }

    /// Every listing's daily bar for one session. Billed 100 flat.
    ///
    /// With `session_date` omitted this is the latest session. With it set, the response
    /// carries listings that have since stopped trading — ~50,270 codes against ~27,714 for
    /// the current session — so a dated pull must be intersected with the current session's
    /// codes rather than read alone.
    pub async fn bulk_last_day(
        &self,
        market: &str,
        session_date: Option<NaiveDate>,
    ) -> Result<Vec<Value>, ProviderFailure> {
        let market = normalise_venue(Some(market))?;
        let mut params = Vec::with_capacity(1);
        if let Some(date) = session_date {
            params.push(("date".to_string(), date.format("%Y-%m-%d").to_string()));
        }
        let request_type = format!("/eod-bulk-last-day/{market}");
        let payload = self
            .get_json(
                &format!("eod-bulk-last-day/{market}"),
                &params,
                Billing::new("eod_bulk", 100, 0).under(&request_type),
                self.bulk_timeout,
            )
            .await?;
        expect_array(payload)
    }
}

/// What one vendor call costs and which tariff row prices it.
///
/// Grouped rather than passed as four positional arguments, because units and symbols read
/// alike at a call site and swapping them would be a silent accounting error.
#[derive(Debug, Clone, Copy)]
struct Billing<'a> {
    purpose: &'a str,
    units: i64,
    symbols: i64,
    request_type: Option<&'a str>,
}

impl<'a> Billing<'a> {
    fn new(purpose: &'a str, units: i64, symbols: i64) -> Self {
        Self {
            purpose,
            units,
            symbols,
            request_type: None,
        }
    }

    /// File this call under a market-specific tariff row instead of its purpose's default.
    fn under(mut self, request_type: &'a str) -> Self {
        self.request_type = Some(request_type);
        self
    }
}

/// The five vendor calls behind a trait, so a provider test can script them.
///
/// Without this the provider adapter would need a mock HTTP server to exercise its parsing,
/// which would make a pure mapping test depend on the network layer's retry policy.
#[async_trait::async_trait]
pub trait EodhdApi: Send + Sync {
    async fn real_time(&self, symbol: &str, venue: Option<&str>) -> Result<Value, ProviderFailure>;
    async fn real_time_whole_market(&self, venue: &str) -> Result<Vec<Value>, ProviderFailure>;
    async fn eod(
        &self,
        symbol: &str,
        venue: Option<&str>,
        days: i64,
        to_date: Option<NaiveDate>,
    ) -> Result<Vec<Value>, ProviderFailure>;
    async fn intraday(
        &self,
        symbol: &str,
        venue: Option<&str>,
        interval: &str,
        start: i64,
        end: i64,
    ) -> Result<Vec<Value>, ProviderFailure>;
    async fn bulk_last_day(
        &self,
        market: &str,
        session_date: Option<NaiveDate>,
    ) -> Result<Vec<Value>, ProviderFailure>;
}

#[async_trait::async_trait]
impl EodhdApi for EodhdClient {
    async fn real_time(&self, symbol: &str, venue: Option<&str>) -> Result<Value, ProviderFailure> {
        EodhdClient::real_time(self, symbol, venue).await
    }

    async fn real_time_whole_market(&self, venue: &str) -> Result<Vec<Value>, ProviderFailure> {
        EodhdClient::real_time_whole_market(self, venue).await
    }

    async fn eod(
        &self,
        symbol: &str,
        venue: Option<&str>,
        days: i64,
        to_date: Option<NaiveDate>,
    ) -> Result<Vec<Value>, ProviderFailure> {
        EodhdClient::eod(self, symbol, venue, days, to_date).await
    }

    async fn intraday(
        &self,
        symbol: &str,
        venue: Option<&str>,
        interval: &str,
        start: i64,
        end: i64,
    ) -> Result<Vec<Value>, ProviderFailure> {
        EodhdClient::intraday(self, symbol, venue, interval, start, end).await
    }

    async fn bulk_last_day(
        &self,
        market: &str,
        session_date: Option<NaiveDate>,
    ) -> Result<Vec<Value>, ProviderFailure> {
        EodhdClient::bulk_last_day(self, market, session_date).await
    }
}

/// How long to wait after a 429: the vendor's `Retry-After` if sane, else backoff.
pub fn retry_after_seconds(retry_after: Option<&str>, attempt: u32) -> f64 {
    if let Some(raw) = retry_after {
        if let Ok(value) = raw.trim().parse::<f64>() {
            if value.is_finite() {
                return value.clamp(0.0, 60.0);
            }
        }
    }
    2_f64.powi(attempt.saturating_sub(1) as i32).min(30.0)
}

/// The EODHD venue code for a venue name, or an error.
///
/// Refusing an unknown venue rather than defaulting to US is deliberate: silently pricing an
/// Australian listing from the US feed produces a wrong number that looks right, which is
/// worse than an error. A blank value means the house default, `US`.
pub fn normalise_venue(text: Option<&str>) -> Result<String, ProviderFailure> {
    normalise_venue_code(text).ok_or_else(|| {
        ProviderFailure::new(
            "unknown-venue",
            format!(
                "{} is not a known venue; pass one of US, AU, L, TO, HK, F, PA, ...",
                match text {
                    Some(value) => py_repr(value),
                    None => "None".to_string(),
                }
            ),
            None,
        )
    })
}

/// `("AAPL", "US") -> "AAPL.US"`; an already-suffixed symbol is left alone.
///
/// Only a suffix that matches a known venue is treated as one, so a genuinely dotted ticker
/// (`BRK.B`) is not mistaken for a venue-qualified code.
pub fn render_symbol(symbol: &str, venue: Option<&str>) -> Result<String, ProviderFailure> {
    let text = symbol.trim().to_uppercase();
    if text.is_empty() {
        return Err(ProviderFailure::new(
            "empty-symbol",
            "a blank symbol is not a symbol",
            None,
        ));
    }
    if let Some((root, tail)) = text.rsplit_once('.') {
        if !root.is_empty() && !tail.is_empty() {
            if let Some(mapped) = normalise_venue_code(Some(tail)) {
                return Ok(format!("{root}.{mapped}"));
            }
        }
    }
    Ok(format!("{text}.{}", normalise_venue(venue)?))
}

/// `"AAPL.US" -> "AAPL"`. Used to key the bulk feed's rows.
pub fn bare_symbol(code: &str) -> String {
    let text = code.trim().to_uppercase();
    match text.split_once('.') {
        Some((root, _)) => root.to_string(),
        None => text,
    }
}

fn expect_array(payload: Value) -> Result<Vec<Value>, ProviderFailure> {
    match payload {
        Value::Array(rows) => Ok(rows),
        other => Err(ProviderFailure::new(
            "unparseable",
            format!("expected a list, got {}", json_type(&other)),
            None,
        )),
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

fn duration_or_default(seconds: f64, fallback: f64) -> Duration {
    Duration::try_from_secs_f64(seconds)
        .or_else(|_| Duration::try_from_secs_f64(fallback))
        .unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::NaiveDate;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct FakeRecorder {
        seen: Mutex<Vec<(String, i64)>>,
        priced: Option<i64>,
    }

    impl FakeRecorder {
        fn new(priced: Option<i64>) -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
                priced,
            }
        }

        fn seen(&self) -> Vec<(String, i64)> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl UsageRecorder for FakeRecorder {
        async fn record(&self, request_type: &str, symbols: i64) -> Option<i64> {
            self.seen
                .lock()
                .unwrap()
                .push((request_type.to_string(), symbols));
            self.priced
        }
    }

    fn client(server: &MockServer) -> EodhdClient {
        EodhdClient::new("test-key", format!("{}/api", server.uri()), 5.0, 5.0).unwrap()
    }

    async fn requests(server: &MockServer) -> Vec<wiremock::Request> {
        server.received_requests().await.unwrap()
    }

    fn query_value(request: &wiremock::Request, name: &str) -> Option<String> {
        request
            .url
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }

    #[test]
    fn venue_aliases_resolve_to_eodhd_codes() {
        assert_eq!(normalise_venue(Some("US")).unwrap(), "US");
        assert_eq!(normalise_venue(Some("NASDAQ")).unwrap(), "US");
        assert_eq!(normalise_venue(Some("ASX")).unwrap(), "AU");
        assert_eq!(normalise_venue(Some("AU")).unwrap(), "AU");
        assert_eq!(normalise_venue(Some("LSE")).unwrap(), "L");
        assert_eq!(normalise_venue(Some("UK")).unwrap(), "L");
        assert_eq!(normalise_venue(Some("TSX")).unwrap(), "TO");
        assert_eq!(normalise_venue(Some("XETRA")).unwrap(), "F");
    }

    #[test]
    fn a_blank_venue_defaults_to_us() {
        assert_eq!(normalise_venue(Some("")).unwrap(), "US");
        assert_eq!(normalise_venue(None).unwrap(), "US");
    }

    #[test]
    fn an_unknown_venue_is_refused_rather_than_defaulted() {
        let error = normalise_venue(Some("MARS")).unwrap_err();
        assert_eq!(error.reason, "unknown-venue");
    }

    #[test]
    fn a_bare_symbol_gets_the_requested_venue() {
        assert_eq!(render_symbol("AAPL", Some("US")).unwrap(), "AAPL.US");
        assert_eq!(render_symbol("cba", Some("AU")).unwrap(), "CBA.AU");
    }

    #[test]
    fn an_already_suffixed_symbol_is_not_double_suffixed() {
        assert_eq!(render_symbol("AAPL.US", Some("US")).unwrap(), "AAPL.US");
        assert_eq!(render_symbol("CBA.AU", Some("US")).unwrap(), "CBA.AU");
    }

    #[test]
    fn a_dotted_ticker_is_not_mistaken_for_a_venue() {
        assert_eq!(render_symbol("BRK.B", Some("US")).unwrap(), "BRK.B.US");
    }

    #[test]
    fn a_blank_symbol_is_refused() {
        assert_eq!(
            render_symbol("", Some("US")).unwrap_err().reason,
            "empty-symbol"
        );
    }

    #[test]
    fn bare_symbol_strips_one_known_suffix() {
        assert_eq!(bare_symbol("AAPL.US"), "AAPL");
        assert_eq!(bare_symbol("brk.b.us"), "BRK");
        assert_eq!(bare_symbol("AAPL"), "AAPL");
        assert_eq!(bare_symbol(""), "");
    }

    #[test]
    fn retry_after_is_clamped_and_falls_back_to_backoff() {
        assert_eq!(retry_after_seconds(Some("5"), 1), 5.0);
        assert_eq!(retry_after_seconds(Some("120"), 1), 60.0);
        assert_eq!(retry_after_seconds(Some("-3"), 1), 0.0);
        assert_eq!(retry_after_seconds(Some("later"), 1), 1.0);
        assert_eq!(retry_after_seconds(None, 1), 1.0);
        assert_eq!(retry_after_seconds(None, 2), 2.0);
        assert_eq!(retry_after_seconds(None, 6), 30.0);
        assert_eq!(retry_after_seconds(None, 7), 30.0);
    }

    #[tokio::test]
    async fn intraday_asks_with_epoch_seconds_and_the_venue_suffix() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/intraday/BHP.AU"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        let client = client(&server);
        let start = 1_700_000_000;
        client
            .intraday("BHP", Some("AU"), "5m", start, start + 400 * 86_400)
            .await
            .unwrap();

        let requests = requests(&server).await;
        assert_eq!(requests.len(), 1);
        assert_eq!(query_value(&requests[0], "interval"), Some("5m".into()));
        assert_eq!(query_value(&requests[0], "from"), Some(start.to_string()));
        assert_eq!(
            query_value(&requests[0], "to"),
            Some((start + 400 * 86_400).to_string())
        );
        assert_eq!(
            query_value(&requests[0], "api_token"),
            Some("test-key".into())
        );
        assert_eq!(client.stats().units, 5);
    }

    #[tokio::test]
    async fn intraday_refuses_a_window_wider_than_the_vendor_accepts() {
        let server = MockServer::start().await;
        let client = client(&server);
        let error = client
            .intraday("BHP", Some("US"), "5m", 1, 1 + 601 * 86_400)
            .await
            .unwrap_err();
        assert_eq!(error.reason, "window-too-wide");
    }

    #[tokio::test]
    async fn intraday_refuses_a_window_without_a_start() {
        let server = MockServer::start().await;
        let client = client(&server);
        let error = client
            .intraday("BHP", Some("US"), "5m", 0, 1_700_000_000)
            .await
            .unwrap_err();
        assert_eq!(error.reason, "empty-window");
    }

    #[tokio::test]
    async fn a_429_is_retried_and_not_billed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/eod/AAPL.US"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/eod/AAPL.US"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .with_priority(2)
            .mount(&server)
            .await;

        let client = client(&server);
        client.eod("AAPL", Some("US"), 60, None).await.unwrap();

        assert_eq!(requests(&server).await.len(), 2);
        assert_eq!(client.stats().units, 1);
    }

    #[tokio::test]
    async fn a_persistent_429_stops_at_the_cap_and_is_still_unbilled() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/eod/AAPL.US"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .mount(&server)
            .await;

        let client = client(&server);
        let error = client.eod("AAPL", Some("US"), 60, None).await.unwrap_err();

        assert_eq!(error.reason, "rate-limited");
        assert_eq!(
            requests(&server).await.len(),
            (MAX_RATE_LIMIT_RETRIES + 1) as usize
        );
        assert_eq!(client.stats().units, 0);
    }

    #[tokio::test]
    async fn a_billed_404_is_not_retried_and_is_recorded() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/eod/AAPL.US"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let client = client(&server);
        let error = client.eod("AAPL", Some("US"), 60, None).await.unwrap_err();

        assert_eq!(error.reason, "http-error");
        assert_eq!(error.status, Some(404));
        assert_eq!(requests(&server).await.len(), 1);
        assert_eq!(client.stats().units, 1);
    }

    #[tokio::test]
    async fn a_200_with_a_non_json_body_is_unbilled_and_distinct() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/eod/AAPL.US"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>nope</html>"))
            .mount(&server)
            .await;

        let client = client(&server);
        let error = client.eod("AAPL", Some("US"), 60, None).await.unwrap_err();

        assert_eq!(error.reason, "unparseable");
        assert_eq!(error.status, Some(200));
        assert_eq!(client.stats().units, 0);
    }

    #[tokio::test]
    async fn an_au_bulk_pull_is_recorded_under_the_au_market() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/eod-bulk-last-day/AU"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        let recorder = Arc::new(FakeRecorder::new(Some(100)));
        let client = client(&server).with_recorder(recorder.clone());
        client.bulk_last_day("AU", None).await.unwrap();

        assert_eq!(
            recorder.seen(),
            vec![("/eod-bulk-last-day/AU".to_string(), 0)]
        );
        assert_eq!(client.stats().units, 100);
        let requests = requests(&server).await;
        assert_eq!(requests.len(), 1);
        assert!(requests[0].url.path().ends_with("/eod-bulk-last-day/AU"));
    }

    #[tokio::test]
    async fn a_us_bulk_pull_keeps_its_own_ledger_row() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/eod-bulk-last-day/US"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        let recorder = Arc::new(FakeRecorder::new(Some(100)));
        let client = client(&server).with_recorder(recorder.clone());
        client.bulk_last_day("US", None).await.unwrap();

        assert_eq!(
            recorder.seen(),
            vec![("/eod-bulk-last-day/US".to_string(), 0)]
        );
    }

    #[tokio::test]
    async fn a_billed_bulk_failure_is_recorded_under_the_market_asked() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/eod-bulk-last-day/AU"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let recorder = Arc::new(FakeRecorder::new(Some(100)));
        let client = client(&server).with_recorder(recorder.clone());
        assert!(client.bulk_last_day("AU", None).await.is_err());
        assert_eq!(
            recorder.seen(),
            vec![("/eod-bulk-last-day/AU".to_string(), 0)]
        );
    }

    #[tokio::test]
    async fn eod_formats_a_calendar_window_and_returns_the_rows() {
        let server = MockServer::start().await;
        let rows = json!([{"date": "2026-10-30", "close": 10.5}]);
        Mock::given(method("GET"))
            .and(path("/api/eod/AAPL.US"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rows.clone()))
            .mount(&server)
            .await;

        let client = client(&server);
        let end = NaiveDate::from_ymd_opt(2026, 11, 3).unwrap();
        let payload = client.eod("AAPL", Some("US"), 60, Some(end)).await.unwrap();

        assert_eq!(Value::Array(payload), rows);
        let requests = requests(&server).await;
        assert_eq!(query_value(&requests[0], "from"), Some("2026-09-04".into()));
        assert_eq!(query_value(&requests[0], "to"), Some("2026-11-03".into()));
        assert_eq!(query_value(&requests[0], "period"), Some("d".into()));
        assert_eq!(query_value(&requests[0], "order"), Some("a".into()));
    }
}
