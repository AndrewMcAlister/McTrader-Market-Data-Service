//! The product capability the HTTP handlers and the collector need, as one trait.
//!
//! The handlers could depend on the concrete [`crate::products::Products`], but then an API
//! contract test would need a database, a vendor and a router to check request parsing and
//! error rendering. The Python suite solved that with a `FakeProducts` object injected into
//! app state; this trait is the same seam, expressed so the compiler can check it.
//!
//! Every method is the concrete product method, unchanged. No handler may add policy here:
//! if it belongs to the product layer, it lives in `products.rs` and is called through this
//! trait by both the API and the scheduler.
//!
//! [`CollectionProducts`] carries `prices`, `refresh_snapshot` and `vendor`: the collection
//! capabilities the scheduler needs without importing the HTTP layer. This trait extends it,
//! so one `Arc<dyn ProductLayer>` serves both the handlers and the collector.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::collector::CollectionProducts;
use crate::config::Settings;
use crate::products::{
    AtrBatchResponse, AtrRequest, BarsResponse, BulkDateKey, BulkResponse, IntradayRequest,
    IntradayResponse, ProductError, ProfileBatch, ProfilesResponse, Products, ProductsStore,
    QuotePayload, ScreenerResponse, StatsResponse,
};

/// The product operations the HTTP surface exposes.
#[async_trait]
pub trait ProductLayer: CollectionProducts {
    async fn price(
        &self,
        symbol: &str,
        exchange: Option<&str>,
        max_age_seconds: Option<f64>,
        asset_class: Option<&str>,
        country_code: Option<&str>,
    ) -> Result<QuotePayload, ProductError>;

    async fn bars(
        &self,
        symbols: &[String],
        sessions: i64,
        exchange: Option<&str>,
        allow_fetch: bool,
        asset_class: Option<&str>,
        country_code: Option<&str>,
    ) -> Result<BarsResponse, ProductError>;

    async fn atr_batch(&self, request: AtrRequest<'_>) -> Result<AtrBatchResponse, ProductError>;

    async fn screener(
        &self,
        country_code: &str,
        exchange: Option<&str>,
        filters: &[Value],
        sort: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<ScreenerResponse, ProductError>;

    async fn profiles(&self, request: ProfileBatch<'_>) -> Result<ProfilesResponse, ProductError>;

    async fn intraday(&self, request: IntradayRequest<'_>) -> Result<IntradayResponse, ProductError>;

    async fn bulk_closes(
        &self,
        market: &str,
        dates: &[BulkDateKey],
        allow_fetch: bool,
    ) -> Result<BulkResponse, ProductError>;

    async fn stats(&self) -> StatsResponse;
}

#[async_trait]
impl ProductLayer for Products {
    async fn price(
        &self,
        symbol: &str,
        exchange: Option<&str>,
        max_age_seconds: Option<f64>,
        asset_class: Option<&str>,
        country_code: Option<&str>,
    ) -> Result<QuotePayload, ProductError> {
        Products::price(
            self,
            symbol,
            exchange,
            max_age_seconds,
            asset_class,
            country_code,
        )
        .await
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
        Products::bars(
            self,
            symbols,
            sessions,
            exchange,
            allow_fetch,
            asset_class,
            country_code,
        )
        .await
    }

    async fn atr_batch(&self, request: AtrRequest<'_>) -> Result<AtrBatchResponse, ProductError> {
        Products::atr_batch(self, request).await
    }

    async fn screener(
        &self,
        country_code: &str,
        exchange: Option<&str>,
        filters: &[Value],
        sort: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<ScreenerResponse, ProductError> {
        Products::screener(self, country_code, exchange, filters, sort, limit, offset).await
    }

    async fn profiles(&self, request: ProfileBatch<'_>) -> Result<ProfilesResponse, ProductError> {
        Products::profiles(self, request).await
    }

    async fn intraday(
        &self,
        request: IntradayRequest<'_>,
    ) -> Result<IntradayResponse, ProductError> {
        Products::intraday(self, request).await
    }

    async fn bulk_closes(
        &self,
        market: &str,
        dates: &[BulkDateKey],
        allow_fetch: bool,
    ) -> Result<BulkResponse, ProductError> {
        Products::bulk_closes(self, market, dates, allow_fetch).await
    }

    async fn stats(&self) -> StatsResponse {
        Products::stats(self).await
    }
}

/// Build (or rebuild) the product layer.
///
/// A trait rather than a free function because the secret endpoints rebuild the layer when the
/// key changes, and a test needs that rebuild to produce its fake rather than opening a
/// database. Construction is fallible exactly as the source service's `build_products` is:
/// `None` means the layer could not be built, *not* that the key is missing — a keyless build
/// still serves Yahoo and Binance.
#[async_trait]
pub trait ProductLayerFactory: Send + Sync {
    async fn build(
        &self,
        settings: &Settings,
        store: Arc<dyn ProductsStore>,
    ) -> Option<Arc<dyn ProductLayer>>;
}

/// The production factory: [`Products::new`] over the real store and vendor.
pub struct ProductsFactory;

#[async_trait]
impl ProductLayerFactory for ProductsFactory {
    async fn build(
        &self,
        settings: &Settings,
        store: Arc<dyn ProductsStore>,
    ) -> Option<Arc<dyn ProductLayer>> {
        let clock: Arc<dyn crate::clock::Clock> = Arc::new(crate::clock::SystemClock);
        match Products::new(settings.clone(), store, None, None, clock).await {
            Ok(products) => Some(Arc::new(products)),
            Err(error) => {
                tracing::error!("product layer unavailable: {error}");
                None
            }
        }
    }
}
