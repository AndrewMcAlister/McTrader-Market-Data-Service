//! Market-data sources and the routing that picks between them.
//!
//! Each provider adapts one vendor to the same two answers; the router owns the order and the
//! fall-through rules. Vendor HTTP and billing semantics live in [`eodhd_client`], not in the
//! provider adapter, so the tariff rule has one implementation.

pub mod base;
pub mod binance;
pub mod eodhd;
pub mod eodhd_client;
pub mod router;
pub mod yahoo;

use std::sync::Arc;

use crate::error::ProviderFailure;

use self::base::{Provider, WholeMarketQuotes};
use self::binance::BinanceProvider;
use self::eodhd::EodhdProvider;
use self::eodhd_client::EodhdClient;
use self::router::ProviderRouter;
use self::yahoo::YahooProvider;

/// The production router: EODHD when a key is configured, then the free providers.
///
/// EODHD is optional rather than fatal. Yahoo and Binance need no credential, so a deployment
/// without a key still answers indices, crypto and FX; a share either falls through to Yahoo
/// or reports `missing`, and every answer still names the provider that served it. The
/// whole-market capability exists only when EODHD does, because it is EODHD's feed.
pub fn build_router(
    client: Option<Arc<EodhdClient>>,
    timeout_seconds: f64,
) -> Result<ProviderRouter, ProviderFailure> {
    let mut providers: Vec<Arc<dyn Provider>> = Vec::with_capacity(3);
    let mut eodhd: Option<Arc<EodhdProvider>> = None;
    if let Some(client) = client {
        let adapter = Arc::new(EodhdProvider::new(client));
        let provider: Arc<dyn Provider> = adapter.clone();
        providers.push(provider);
        eodhd = Some(adapter);
    }
    let yahoo: Arc<dyn Provider> = Arc::new(YahooProvider::new(timeout_seconds)?);
    providers.push(yahoo);
    let binance: Arc<dyn Provider> = Arc::new(BinanceProvider::new(timeout_seconds)?);
    providers.push(binance);

    let mut router = ProviderRouter::new(providers);
    if let Some(adapter) = eodhd {
        let whole_market: Arc<dyn WholeMarketQuotes> = adapter;
        router = router.with_whole_market(whole_market);
    }
    Ok(router)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Arc<EodhdClient> {
        Arc::new(EodhdClient::new("test-key", "http://127.0.0.1:9", 1.0, 1.0).unwrap())
    }

    #[test]
    fn without_a_key_the_free_providers_still_answer() {
        let router = build_router(None, 5.0).unwrap();
        assert_eq!(router.stats().providers, vec!["binance", "yahoo"]);
    }

    #[test]
    fn with_a_key_eodhd_leads_the_router() {
        let router = build_router(Some(client()), 5.0).unwrap();
        assert_eq!(router.stats().providers, vec!["binance", "eodhd", "yahoo"]);
    }
}
