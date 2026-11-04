//! Which provider answers, and the fallback order between them.
//!
//! # The order is per asset class, not one global list
//!
//! Routing by asset class is the whole point of having three providers: a US share belongs on
//! EODHD (metered, but adjusted and consistent), an index belongs on Yahoo where `^IXIC` is a
//! first-class ticker, and a crypto pair belongs on Binance where the pair is traded natively
//! rather than rendered as `BTC.CC`.
//!
//! | asset class                           | order                    |
//! |---------------------------------------|--------------------------|
//! | shares                                | EODHD → Yahoo            |
//! | indices / commodities / rates / forex | EODHD → Yahoo            |
//! | crypto                                | Binance → EODHD → Yahoo  |
//!
//! # Failover changes the tape, so it is always declared
//!
//! Every quote and bar carries the `provider` that answered. That is not decoration: the estate
//! has already been bitten by a silent tape switch — Mini-McTrader logged
//! `FALLBACK to Alpaca IEX prices - a different tape` with nothing flagging it, and a stop
//! computed on one tape while a level was chosen on another is a silent error.
//!
//! A provider that cannot serve an asset class is not asked at all. Binance has no indices, so
//! asking it and reading the failure as "no data" would be both a wasted request and a
//! misleading one.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::domain::asset_class::AssetClass;
use crate::domain::symbols::{BINANCE, EODHD, YAHOO};
use crate::error::ProviderFailure;
use crate::providers::base::{Provider, ProviderBar, ProviderQuote, WholeMarketQuotes};

/// Who answers first, by asset class. Order is a deliberate policy, not a preference: EODHD
/// leads for everything it is *good* at, and the free providers lead where they are strictly
/// better than it.
const SHARES_ORDER: &[&str] = &[EODHD, YAHOO];
const CRYPTO_ORDER: &[&str] = &[BINANCE, EODHD, YAHOO];

/// The provider order for an asset class.
pub fn order_for(asset_class: AssetClass) -> &'static [&'static str] {
    if asset_class == AssetClass::Crypto {
        CRYPTO_ORDER
    } else {
        SHARES_ORDER
    }
}

/// One routing attempt, kept for diagnostics. Not an audit trail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub symbol: String,
    pub asset_class: String,
    pub provider: String,
    pub outcome: String,
}

/// The router's view of what has happened.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RouterStats {
    pub fallbacks: u64,
    pub outcomes: BTreeMap<String, u64>,
    pub providers: Vec<String>,
}

/// Dispatches a quote or a bar request to the first provider that can answer.
pub struct ProviderRouter {
    by_name: BTreeMap<&'static str, Arc<dyn Provider>>,
    whole_market: Option<Arc<dyn WholeMarketQuotes>>,
    attempts: Mutex<Vec<Attempt>>,
    fallbacks: AtomicU64,
}

impl ProviderRouter {
    pub fn new(providers: Vec<Arc<dyn Provider>>) -> Self {
        let by_name = providers
            .into_iter()
            .map(|provider| (provider.name(), provider))
            .collect();
        Self {
            by_name,
            whole_market: None,
            attempts: Mutex::new(Vec::new()),
            fallbacks: AtomicU64::new(0),
        }
    }

    /// Attach the whole-market capability. Only EODHD has one, and it is not a generic
    /// provider operation, so it is injected rather than assumed.
    pub fn with_whole_market(mut self, provider: Arc<dyn WholeMarketQuotes>) -> Self {
        self.whole_market = Some(provider);
        self
    }

    /// The providers to try, in order, skipping those that cannot serve the class.
    fn candidates(&self, asset_class: AssetClass) -> Vec<(&'static str, Arc<dyn Provider>)> {
        order_for(asset_class)
            .iter()
            .filter_map(|key| {
                let provider = self.by_name.get(key)?;
                if !provider.supports(asset_class) {
                    return None;
                }
                Some((*key, Arc::clone(provider)))
            })
            .collect()
    }

    /// The first provider's quote, or `None` when nobody could price it.
    ///
    /// Every failure is recorded rather than raised: a symbol Yahoo cannot serve is not an
    /// error, it is a reason to ask the next one. Only when every candidate has declined does
    /// the answer become `None`, which the caller reports as `missing`.
    pub async fn quote(
        &self,
        symbol: &str,
        asset_class: AssetClass,
        venue: Option<&str>,
    ) -> Option<ProviderQuote> {
        let candidates = self.candidates(asset_class);
        for (index, (key, provider)) in candidates.iter().enumerate() {
            match provider.quote(symbol, asset_class, venue).await {
                Err(error) => {
                    self.record(
                        symbol,
                        asset_class,
                        key,
                        format!("{}: {}", error.reason, error.detail),
                    );
                }
                Ok(None) => self.record(symbol, asset_class, key, "declined".to_string()),
                Ok(Some(quote)) => {
                    if index > 0 {
                        self.fallbacks.fetch_add(1, Ordering::Relaxed);
                        tracing::info!(
                            "quote {symbol} served by fallback {key} after {}",
                            candidates[..index]
                                .iter()
                                .map(|(name, _)| *name)
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                    self.record(symbol, asset_class, key, "ok".to_string());
                    return Some(quote);
                }
            }
        }
        None
    }

    /// `(bars, provider_name)` from the first provider that returns any.
    ///
    /// Unlike a quote, an *empty* list is treated as a refusal worth falling through: a provider
    /// that answers with no bars has not answered the question, and a share with a Yahoo
    /// history but no EODHD one is exactly the case this exists for.
    pub async fn bars(
        &self,
        symbol: &str,
        asset_class: AssetClass,
        sessions: usize,
        venue: Option<&str>,
    ) -> (Vec<ProviderBar>, Option<&'static str>) {
        let candidates = self.candidates(asset_class);
        for (index, (key, provider)) in candidates.iter().enumerate() {
            match provider.bars(symbol, asset_class, sessions, venue).await {
                Err(error) => {
                    self.record(
                        symbol,
                        asset_class,
                        key,
                        format!("bars {}: {}", error.reason, error.detail),
                    );
                }
                Ok(bars) if bars.is_empty() => {
                    self.record(symbol, asset_class, key, "bars declined".to_string());
                }
                Ok(bars) => {
                    if index > 0 {
                        self.fallbacks.fetch_add(1, Ordering::Relaxed);
                        tracing::info!("bars {symbol} served by fallback {key}");
                    }
                    self.record(
                        symbol,
                        asset_class,
                        key,
                        format!("bars ok ({})", bars.len()),
                    );
                    return (bars, Some(key));
                }
            }
        }
        (Vec::new(), None)
    }

    /// Every US listing's latest price from the one provider that has such a feed.
    pub async fn whole_market_quotes(
        &self,
        venue: &str,
    ) -> Result<Vec<ProviderQuote>, ProviderFailure> {
        match &self.whole_market {
            Some(provider) => provider.whole_market_quotes(Some(venue)).await,
            None => Err(ProviderFailure::new(
                "no-bulk-provider",
                "the whole-market feed is an EODHD capability and EODHD is not configured",
                None,
            )),
        }
    }

    fn record(&self, symbol: &str, asset_class: AssetClass, provider: &str, outcome: String) {
        let mut attempts = self
            .attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        attempts.push(Attempt {
            symbol: symbol.to_string(),
            asset_class: asset_class.as_str().to_string(),
            provider: provider.to_string(),
            outcome,
        });
        // Keep the log bounded: this is diagnostics, not an audit trail.
        if attempts.len() > 500 {
            attempts.drain(..250);
        }
    }

    pub fn stats(&self) -> RouterStats {
        let attempts = self
            .attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut outcomes = BTreeMap::new();
        for attempt in attempts.iter() {
            let first_word = attempt
                .outcome
                .split(' ')
                .next()
                .unwrap_or(&attempt.outcome);
            let key = format!("{}:{first_word}", attempt.provider);
            *outcomes.entry(key).or_insert(0) += 1;
        }
        RouterStats {
            fallbacks: self.fallbacks.load(Ordering::Relaxed),
            outcomes,
            providers: self.by_name.keys().map(|name| name.to_string()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::AtomicUsize;

    #[derive(Default)]
    struct FakeProvider {
        name: &'static str,
        supports_crypto: bool,
        supports_other: bool,
        quote_result: Option<ProviderQuote>,
        quote_error: Option<ProviderFailure>,
        bars_result: Vec<ProviderBar>,
        quote_calls: AtomicUsize,
        bars_calls: AtomicUsize,
    }

    impl FakeProvider {
        fn answering(name: &'static str) -> Arc<Self> {
            Arc::new(Self {
                name,
                supports_crypto: true,
                supports_other: true,
                quote_result: Some(quote(name)),
                ..Self::default()
            })
        }

        fn declining(name: &'static str) -> Arc<Self> {
            Arc::new(Self {
                name,
                supports_crypto: true,
                supports_other: true,
                ..Self::default()
            })
        }

        fn cannot_serve(name: &'static str) -> Arc<Self> {
            Arc::new(Self {
                name,
                ..Self::default()
            })
        }

        fn raising(name: &'static str) -> Arc<Self> {
            Arc::new(Self {
                name,
                supports_crypto: true,
                supports_other: true,
                quote_error: Some(ProviderFailure::new("boom", "upstream refused", None)),
                ..Self::default()
            })
        }

        fn with_bars(name: &'static str, bars: Vec<ProviderBar>) -> Arc<Self> {
            Arc::new(Self {
                name,
                supports_crypto: true,
                supports_other: true,
                bars_result: bars,
                ..Self::default()
            })
        }

        fn quote_calls(&self) -> usize {
            self.quote_calls.load(Ordering::Relaxed)
        }

        fn bars_calls(&self) -> usize {
            self.bars_calls.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl Provider for FakeProvider {
        fn name(&self) -> &'static str {
            self.name
        }

        fn supports(&self, asset_class: AssetClass) -> bool {
            if asset_class == AssetClass::Crypto {
                self.supports_crypto
            } else {
                self.supports_other
            }
        }

        async fn quote(
            &self,
            _symbol: &str,
            _asset_class: AssetClass,
            _venue: Option<&str>,
        ) -> Result<Option<ProviderQuote>, ProviderFailure> {
            self.quote_calls.fetch_add(1, Ordering::Relaxed);
            if let Some(error) = &self.quote_error {
                return Err(error.clone());
            }
            Ok(self.quote_result.clone())
        }

        async fn bars(
            &self,
            _symbol: &str,
            _asset_class: AssetClass,
            _sessions: usize,
            _venue: Option<&str>,
        ) -> Result<Vec<ProviderBar>, ProviderFailure> {
            self.bars_calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.bars_result.clone())
        }
    }

    struct FakeWholeMarket {
        quotes: Vec<ProviderQuote>,
    }

    #[async_trait]
    impl WholeMarketQuotes for FakeWholeMarket {
        async fn whole_market_quotes(
            &self,
            _venue: Option<&str>,
        ) -> Result<Vec<ProviderQuote>, ProviderFailure> {
            Ok(self.quotes.clone())
        }
    }

    fn quote(provider: &str) -> ProviderQuote {
        ProviderQuote {
            symbol: "AAPL".into(),
            price: 100.0,
            provider: provider.into(),
            previous_close: Some(99.0),
            as_of: None,
            currency: None,
            raw_ticker: Some(provider.to_string()),
            volume: 0,
        }
    }

    fn bar() -> ProviderBar {
        ProviderBar {
            session: chrono::NaiveDate::from_ymd_opt(2026, 10, 30).unwrap(),
            open: 10.0,
            high: 11.0,
            low: 9.5,
            close: 10.5,
            volume: 1,
            adjusted: false,
            raw_close: Some(10.5),
        }
    }

    async fn total_calls(providers: &[&Arc<FakeProvider>]) -> usize {
        let mut total = 0;
        for provider in providers {
            total += provider.quote_calls() + provider.bars_calls();
        }
        total
    }

    #[test]
    fn the_order_is_per_asset_class() {
        assert_eq!(order_for(AssetClass::Shares)[0], "eodhd");
        assert_eq!(order_for(AssetClass::Crypto)[0], "binance");
    }

    #[tokio::test]
    async fn the_first_provider_that_answers_wins_and_the_rest_are_not_asked() {
        let primary = FakeProvider::answering("eodhd");
        let secondary = FakeProvider::answering("yahoo");
        let router = ProviderRouter::new(vec![primary.clone(), secondary.clone()]);

        let result = router
            .quote("AAPL", AssetClass::Shares, None)
            .await
            .unwrap();

        assert_eq!(result.provider, "eodhd");
        assert_eq!(primary.quote_calls(), 1);
        assert_eq!(secondary.quote_calls(), 0);
    }

    #[tokio::test]
    async fn a_declining_provider_falls_through_and_the_tape_is_declared() {
        let primary = FakeProvider::declining("eodhd");
        let secondary = FakeProvider::answering("yahoo");
        let router = ProviderRouter::new(vec![primary, secondary]);

        let result = router
            .quote("AAPL", AssetClass::Shares, None)
            .await
            .unwrap();

        assert_eq!(result.provider, "yahoo");
        assert_eq!(router.stats().fallbacks, 1);
    }

    #[tokio::test]
    async fn a_raising_provider_falls_through_rather_than_failing_the_request() {
        let primary = FakeProvider::raising("eodhd");
        let secondary = FakeProvider::answering("yahoo");
        let router = ProviderRouter::new(vec![primary, secondary]);

        assert_eq!(
            router
                .quote("AAPL", AssetClass::Shares, None)
                .await
                .unwrap()
                .provider,
            "yahoo"
        );
    }

    #[tokio::test]
    async fn a_provider_that_cannot_serve_the_class_is_never_asked() {
        let binance = FakeProvider::cannot_serve("binance");
        let yahoo = FakeProvider::answering("yahoo");
        let router = ProviderRouter::new(vec![binance.clone(), yahoo]);

        let result = router
            .quote("IXIC", AssetClass::Indices, None)
            .await
            .unwrap();

        assert_eq!(result.provider, "yahoo");
        assert_eq!(binance.quote_calls(), 0);
    }

    #[tokio::test]
    async fn crypto_takes_binance_not_eodhd() {
        let eodhd = FakeProvider::answering("eodhd");
        let binance = FakeProvider::answering("binance");
        let router = ProviderRouter::new(vec![eodhd.clone(), binance]);

        assert_eq!(
            router
                .quote("BTC-USD", AssetClass::Crypto, None)
                .await
                .unwrap()
                .provider,
            "binance"
        );
        assert_eq!(eodhd.quote_calls(), 0);
    }

    #[tokio::test]
    async fn nobody_can_answer_yields_none_rather_than_an_error() {
        let router = ProviderRouter::new(vec![FakeProvider::declining("eodhd")]);
        assert!(router
            .quote("NOPE", AssetClass::Shares, None)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn bars_fall_through_on_an_empty_answer() {
        let empty = FakeProvider::with_bars("eodhd", Vec::new());
        let useful = FakeProvider::with_bars("yahoo", vec![bar()]);
        let router = ProviderRouter::new(vec![empty.clone(), useful.clone()]);

        let (bars, provider) = router.bars("AAPL", AssetClass::Shares, 15, None).await;

        assert_eq!(provider, Some("yahoo"));
        assert_eq!(bars.len(), 1);
        assert_eq!(empty.bars_calls(), 1);
        assert_eq!(useful.bars_calls(), 1);
    }

    #[tokio::test]
    async fn the_whole_market_feed_refuses_when_eodhd_is_not_configured() {
        let router = ProviderRouter::new(vec![FakeProvider::declining("yahoo")]);
        let error = router.whole_market_quotes("US").await.unwrap_err();
        assert_eq!(error.reason, "no-bulk-provider");
    }

    #[tokio::test]
    async fn the_whole_market_feed_delegates_to_the_injected_capability() {
        let router = ProviderRouter::new(vec![]).with_whole_market(Arc::new(FakeWholeMarket {
            quotes: vec![quote("eodhd")],
        }));
        let quotes = router.whole_market_quotes("US").await.unwrap();
        assert_eq!(quotes.len(), 1);
    }

    #[tokio::test]
    async fn stats_count_outcomes_by_provider_and_first_word() {
        let router = ProviderRouter::new(vec![
            FakeProvider::declining("eodhd"),
            FakeProvider::answering("yahoo"),
        ]);
        router.quote("AAPL", AssetClass::Shares, None).await;

        let stats = router.stats();
        assert_eq!(stats.fallbacks, 1);
        assert_eq!(stats.outcomes.get("eodhd:declined"), Some(&1));
        assert_eq!(stats.outcomes.get("yahoo:ok"), Some(&1));
        assert_eq!(
            stats.providers,
            vec!["eodhd".to_string(), "yahoo".to_string()]
        );
    }

    #[tokio::test]
    async fn the_attempt_log_is_bounded() {
        let router = ProviderRouter::new(vec![FakeProvider::declining("eodhd")]);
        for _ in 0..550 {
            router.quote("AAPL", AssetClass::Shares, None).await;
        }
        let attempts = router.attempts.lock().unwrap().len();
        assert!(attempts <= 500, "attempt log grew to {attempts}");
        assert!(attempts >= 250);
    }

    #[tokio::test]
    async fn no_candidate_means_no_calls_at_all() {
        let provider = FakeProvider::cannot_serve("eodhd");
        let router = ProviderRouter::new(vec![provider.clone()]);
        assert!(router
            .quote("AAPL", AssetClass::Shares, None)
            .await
            .is_none());
        assert_eq!(total_calls(&[&provider]).await, 0);
    }
}
