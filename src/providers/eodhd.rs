//! EODHD as a provider: the only metered one, and the only one with a whole-market feed.
//!
//! This wraps [`EodhdApi`] rather than the HTTP client, so the measured vendor calls and their
//! tariff handling stay in one place and the mapping from vendor JSON to
//! [`ProviderQuote`]/[`ProviderBar`] stays testable without a network.
//!
//! Two capabilities exist here and nowhere else in the service:
//!
//! * **the whole-market snapshot** (`/real-time/{ph}?ex=US`, 100 units flat) — one call that
//!   prices ~12,700 US names, which is what makes a 2,757-name scan cost 100 units instead of
//!   2,757;
//! * **the whole-market daily bulk** (`/eod-bulk-last-day/US`, 100 units flat) — the daily job's
//!   single load.
//!
//! Neither has a Yahoo or Binance equivalent, so [`WholeMarketQuotes`] is a separate trait
//! instead of a "not supported" branch on every other provider.

use std::sync::Arc;

use chrono::{NaiveDate, Utc};
use serde_json::Value;

use crate::domain::asset_class::AssetClass;
use crate::domain::sessions::latest_completed_session;
use crate::domain::symbols::{symbol_for_provider, venue_currency_code, ProviderId, EODHD};
use crate::error::ProviderFailure;
use crate::providers::base::{
    epoch_naive, nonzero, number, text, truthy, volume, Provider, ProviderBar, ProviderQuote,
    WholeMarketQuotes,
};
use crate::providers::eodhd_client::{bare_symbol, normalise_venue, EodhdApi};

/// Quotes and daily bars from EODHD, plus its two whole-market calls.
pub struct EodhdProvider {
    client: Arc<dyn EodhdApi>,
}

impl EodhdProvider {
    pub fn new(client: Arc<dyn EodhdApi>) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl Provider for EodhdProvider {
    fn name(&self) -> &'static str {
        EODHD
    }

    /// Everything the service distinguishes. EODHD is the broadest source here.
    ///
    /// Crypto is included because EODHD serves `BTC.CC`; the router puts Binance first for
    /// crypto anyway, so this only matters as a fallback when Binance lacks the pair.
    fn supports(&self, _asset_class: AssetClass) -> bool {
        true
    }

    /// One instrument's latest price. Billed 1 unit.
    async fn quote(
        &self,
        symbol: &str,
        asset_class: AssetClass,
        venue: Option<&str>,
    ) -> Result<Option<ProviderQuote>, ProviderFailure> {
        let market = normalise_venue(venue)?;
        let Some(ticker) = symbol_for_provider(symbol, asset_class, venue, ProviderId::Eodhd)
        else {
            return Ok(None);
        };
        let payload = self.client.real_time(&ticker, Some(&market)).await?;

        // `or` rather than "first present": the vendor sends 0 for a field it does not have,
        // and a zero price is not an answer.
        let price = nonzero(payload.get("close").unwrap_or(&Value::Null))
            .or_else(|| number(payload.get("price").unwrap_or(&Value::Null)));
        let Some(price) = price.filter(|price| *price > 0.0) else {
            return Ok(None);
        };

        Ok(Some(ProviderQuote {
            symbol: symbol.trim().to_uppercase(),
            price,
            provider: EODHD.to_string(),
            previous_close: nonzero(payload.get("previousClose").unwrap_or(&Value::Null))
                .or_else(|| number(payload.get("previous_close").unwrap_or(&Value::Null))),
            as_of: epoch_naive(payload.get("timestamp").unwrap_or(&Value::Null)),
            currency: currency_of(&payload, symbol, venue),
            raw_ticker: Some(ticker),
            volume: volume(payload.get("volume").unwrap_or(&Value::Null)),
        }))
    }

    /// Completed daily bars, oldest first. Billed 1 unit whatever the window.
    ///
    /// Prefers `adjusted_close`: an unadjusted close across a split reads as a catastrophic
    /// loss, and this database already documents that trap (JDZG, 2026-03-23).
    async fn bars(
        &self,
        symbol: &str,
        asset_class: AssetClass,
        sessions: usize,
        venue: Option<&str>,
    ) -> Result<Vec<ProviderBar>, ProviderFailure> {
        let market = normalise_venue(venue)?;
        let Some(ticker) = symbol_for_provider(symbol, asset_class, venue, ProviderId::Eodhd)
        else {
            return Ok(Vec::new());
        };

        // `sessions` is a session count, but /eod/ takes a calendar window and bills the same
        // for any width, so ask wide enough to survive weekends and holidays.
        let days = (sessions.max(1) as i64).saturating_mul(2).max(60);
        let payload = self.client.eod(&ticker, Some(&market), days, None).await?;

        let latest = latest_completed_session(Some(&market), Utc::now());
        let mut bars = Vec::with_capacity(payload.len());
        for raw in &payload {
            let Some(bar) = bar_from_row(raw, latest) else {
                continue;
            };
            bars.push(bar);
        }

        bars.sort_by_key(|bar| bar.session);
        if sessions > 0 && bars.len() > sessions {
            let excess = bars.len() - sessions;
            bars.drain(..excess);
        }
        Ok(bars)
    }
}

#[async_trait::async_trait]
impl WholeMarketQuotes for EodhdProvider {
    /// Every US listing's latest price in one call. Billed 100 units flat.
    ///
    /// Rows carrying no usable price are skipped rather than returned as zeros; the caller
    /// applies the freshness rule, because "too old" depends on whether the question is a live
    /// entry or a daily pass.
    async fn whole_market_quotes(
        &self,
        venue: Option<&str>,
    ) -> Result<Vec<ProviderQuote>, ProviderFailure> {
        let market = normalise_venue(venue)?;
        let rows = self.client.real_time_whole_market(&market).await?;

        let mut quotes = Vec::with_capacity(rows.len());
        for row in &rows {
            let Some(quote) = whole_market_quote(row, &market) else {
                continue;
            };
            quotes.push(quote);
        }
        Ok(quotes)
    }
}

fn currency_of(payload: &Value, symbol: &str, venue: Option<&str>) -> Option<String> {
    let stated = payload
        .get("currency")
        .filter(|currency| truthy(currency))
        .and_then(text);
    stated.or_else(|| venue_currency_code(symbol, venue).map(str::to_string))
}

/// One `/eod/` row, or `None` when it is incomplete, malformed, or still trading.
fn bar_from_row(raw: &Value, latest: NaiveDate) -> Option<ProviderBar> {
    let date_text = text(raw.get("date").unwrap_or(&Value::Null))
        .unwrap_or_default()
        .trim()
        .to_string();
    if date_text.is_empty() {
        return None;
    }
    let session = NaiveDate::parse_from_str(&date_text, "%Y-%m-%d").ok()?;
    if session > latest {
        return None; // in-progress session: a partial range would shrink a stop
    }

    let adjusted = number(raw.get("adjusted_close").unwrap_or(&Value::Null));
    let raw_close = number(raw.get("close").unwrap_or(&Value::Null));
    let close = adjusted.or(raw_close)?;
    let open = number(raw.get("open").unwrap_or(&Value::Null))?;
    let high = number(raw.get("high").unwrap_or(&Value::Null))?;
    let low = number(raw.get("low").unwrap_or(&Value::Null))?;
    if close <= 0.0 {
        return None;
    }

    Some(ProviderBar {
        session,
        open,
        high,
        low,
        close,
        volume: volume(raw.get("volume").unwrap_or(&Value::Null)),
        // False only when the vendor gave no adjusted close, so a caller can tell a raw
        // series from an adjusted one instead of assuming.
        adjusted: adjusted.is_some(),
        // Both closes travel: Close keeps the series continuous across dividends and splits,
        // RawClose is what a candle body and an order price are read off.
        raw_close,
    })
}

fn whole_market_quote(row: &Value, market: &str) -> Option<ProviderQuote> {
    let code = bare_symbol(&text(row.get("code").unwrap_or(&Value::Null)).unwrap_or_default());
    if code.is_empty() {
        return None;
    }
    let price = number(row.get("close").unwrap_or(&Value::Null))?;
    if price <= 0.0 {
        return None;
    }
    Some(ProviderQuote {
        symbol: code.clone(),
        price,
        provider: EODHD.to_string(),
        previous_close: number(row.get("previousClose").unwrap_or(&Value::Null)),
        as_of: epoch_naive(row.get("timestamp").unwrap_or(&Value::Null)),
        // The whole-market row carries volume, and dropping it here is what made
        // todays-big-movers return nothing: the cache stored volume 0 for every US name, and
        // the query requires volume > 0.
        volume: volume(row.get("volume").unwrap_or(&Value::Null)),
        currency: (market == "US").then(|| "USD".to_string()),
        raw_ticker: Some(format!("{code}.{market}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::eodhd_client::EodhdApi;
    use async_trait::async_trait;
    use chrono::Days;
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Default)]
    struct ScriptedClient {
        rows: Vec<Value>,
        calls: Mutex<Vec<(String, Option<String>, i64)>>,
    }

    impl ScriptedClient {
        fn with_rows(rows: Vec<Value>) -> Arc<Self> {
            Arc::new(Self {
                rows,
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl EodhdApi for ScriptedClient {
        async fn real_time(
            &self,
            symbol: &str,
            venue: Option<&str>,
        ) -> Result<Value, ProviderFailure> {
            self.calls
                .lock()
                .unwrap()
                .push((symbol.to_string(), venue.map(str::to_string), 0));
            Ok(self.rows.first().cloned().unwrap_or(Value::Null))
        }

        async fn real_time_whole_market(
            &self,
            _venue: &str,
        ) -> Result<Vec<Value>, ProviderFailure> {
            Ok(self.rows.clone())
        }

        async fn eod(
            &self,
            symbol: &str,
            venue: Option<&str>,
            days: i64,
            _to_date: Option<NaiveDate>,
        ) -> Result<Vec<Value>, ProviderFailure> {
            self.calls
                .lock()
                .unwrap()
                .push((symbol.to_string(), venue.map(str::to_string), days));
            Ok(self.rows.clone())
        }

        async fn intraday(
            &self,
            _symbol: &str,
            _venue: Option<&str>,
            _interval: &str,
            _start: i64,
            _end: i64,
        ) -> Result<Vec<Value>, ProviderFailure> {
            Ok(Vec::new())
        }

        async fn bulk_last_day(
            &self,
            _market: &str,
            _session_date: Option<NaiveDate>,
        ) -> Result<Vec<Value>, ProviderFailure> {
            Ok(Vec::new())
        }
    }

    fn session(days_ago: u64) -> String {
        (Utc::now().date_naive() - Days::new(days_ago))
            .format("%Y-%m-%d")
            .to_string()
    }

    #[tokio::test]
    async fn eod_bars_carry_both_the_adjusted_and_the_raw_close() {
        let client = ScriptedClient::with_rows(vec![json!({
            "date": session(2), "open": 26.63, "high": 27.20, "low": 26.47,
            "close": 27.10, "adjusted_close": 26.709, "volume": 229441,
        })]);
        let provider = EodhdProvider::new(client.clone());

        let bars = provider
            .bars("LOV", AssetClass::Shares, 10, Some("AU"))
            .await
            .unwrap();

        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].close, 26.709);
        assert_eq!(bars[0].raw_close, Some(27.10));
        assert!(bars[0].adjusted);
        assert_eq!(bars[0].volume, 229_441);
    }

    #[tokio::test]
    async fn an_eod_bar_without_an_adjusted_close_reads_raw_on_both_fields() {
        let client = ScriptedClient::with_rows(vec![json!({
            "date": session(2), "open": 10.0, "high": 11.0, "low": 9.5,
            "close": 10.5, "volume": 100,
        })]);
        let provider = EodhdProvider::new(client);

        let bars = provider
            .bars("AAPL", AssetClass::Shares, 10, None)
            .await
            .unwrap();

        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].close, 10.5);
        assert_eq!(bars[0].raw_close, Some(10.5));
        assert!(!bars[0].adjusted);
    }

    #[tokio::test]
    async fn eod_bars_drop_the_session_in_progress_and_incomplete_rows() {
        let client = ScriptedClient::with_rows(vec![
            json!({"date": session(0), "open": 1.0, "high": 2.0, "low": 0.5,
                   "close": 1.5, "volume": 1}),
            json!({"date": session(1), "open": 1.0, "high": 2.0, "low": 0.5,
                   "close": 0, "volume": 1}),
            json!({"date": session(3), "open": 1.0, "high": 2.0, "low": 0.5,
                   "close": 1.5, "volume": 1}),
            json!({"date": "not-a-date", "open": 1.0, "high": 2.0, "low": 0.5,
                   "close": 1.5, "volume": 1}),
        ]);
        let provider = EodhdProvider::new(client);

        let bars = provider
            .bars("AAPL", AssetClass::Shares, 10, None)
            .await
            .unwrap();

        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].session.to_string(), session(3));
    }

    #[tokio::test]
    async fn bars_ask_for_a_generous_calendar_window_and_keep_the_newest() {
        let rows = (1..=4)
            .map(|days| {
                json!({"date": session(days), "open": 1.0, "high": 2.0, "low": 0.5,
                       "close": 1.5, "volume": 1})
            })
            .collect();
        let client = ScriptedClient::with_rows(rows);
        let provider = EodhdProvider::new(client.clone());

        let bars = provider
            .bars("AAPL", AssetClass::Shares, 2, None)
            .await
            .unwrap();

        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].session.to_string(), session(2));
        assert_eq!(bars[1].session.to_string(), session(1));
        assert_eq!(client.calls.lock().unwrap()[0].2, 60);
    }

    #[tokio::test]
    async fn a_quote_falls_through_a_zero_close_to_the_price_field() {
        let client = ScriptedClient::with_rows(vec![json!({
            "close": 0, "price": 12.5, "previousClose": 0, "previous_close": 11.0,
            "timestamp": 1_700_000_000, "currency": "", "volume": 1234.9,
        })]);
        let provider = EodhdProvider::new(client);

        let quote = provider
            .quote("CBA", AssetClass::Shares, Some("AU"))
            .await
            .unwrap()
            .expect("a quote");

        assert_eq!(quote.price, 12.5);
        assert_eq!(quote.previous_close, Some(11.0));
        assert_eq!(quote.currency.as_deref(), Some("AUD"));
        assert_eq!(quote.volume, 1234);
        assert_eq!(quote.raw_ticker.as_deref(), Some("CBA.AU"));
        assert!(quote.as_of.is_some());
    }

    #[tokio::test]
    async fn a_non_positive_price_declines_rather_than_quoting_zero() {
        let client = ScriptedClient::with_rows(vec![json!({"close": -1.0})]);
        let provider = EodhdProvider::new(client);

        assert_eq!(
            provider
                .quote("AAPL", AssetClass::Shares, None)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_commodity_unknown_to_eodhd_declines_without_calling() {
        let client = ScriptedClient::with_rows(vec![json!({"close": 1.0})]);
        let provider = EodhdProvider::new(client.clone());

        assert_eq!(
            provider
                .quote("CL", AssetClass::Commodities, None)
                .await
                .unwrap(),
            None
        );
        assert!(client.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_whole_market_feed_carries_volume_and_strips_the_suffix() {
        let client = ScriptedClient::with_rows(vec![
            json!({"code": "AAPL.US", "close": 105.0, "previousClose": 100.0,
                   "timestamp": 1_700_000_000, "volume": 5000}),
            // A row with no volume is still a price; only a row with no price is dropped.
            json!({"code": "NOVOL.US", "close": 2.0}),
            json!({"code": "BAD.US", "close": 0, "volume": 1}),
        ]);
        let provider = EodhdProvider::new(client);

        let quotes = provider.whole_market_quotes(Some("US")).await.unwrap();

        assert_eq!(quotes.len(), 2);
        let quote = &quotes[0];
        assert_eq!(quote.symbol, "AAPL");
        assert_eq!(quote.price, 105.0);
        assert_eq!(quote.previous_close, Some(100.0));
        assert_eq!(quote.volume, 5000);
        assert_eq!(quote.currency.as_deref(), Some("USD"));
        assert_eq!(quote.raw_ticker.as_deref(), Some("AAPL.US"));
        let change = quote.change_pct().unwrap();
        assert!((change - 5.0).abs() < 1e-12, "got {change}");

        // Missing volume is zero, not `null` and not a reason to lose the row.
        assert_eq!(quotes[1].symbol, "NOVOL");
        assert_eq!(quotes[1].volume, 0);
    }
}
