//! The state every handler shares.
//!
//! One [`Shared`] per process, behind an [`AppState`] arc. The Python service put the same
//! facts on `app.state`; keeping them in one struct makes it obvious what a handler may read
//! and what it must not mutate.
//!
//! # Why the settings are behind a lock
//!
//! The secret endpoints replace the vendor key while the service runs — that is the point of
//! the endpoint, since a key stored under an interactive account is invisible to a SYSTEM
//! service until the service itself writes it. Replacing the key means replacing the settings
//! snapshot and rebuilding the product layer. Every other handler reads a clone, so a request
//! that began under the old key finishes under it rather than seeing half a change.

use std::sync::{Arc, RwLock};

use chrono::DateTime;

use crate::clock::{Clock, MarketClock};
use crate::collector::{Collector, StoreRefresh};
use crate::config::Settings;
use crate::products::ProductsStore;
use crate::refresh::{RefreshGate, RefreshStore};
use crate::schedule::{MarketRunner, RefreshScheduler, SessionSchedule, MARKETS};
use crate::secrets::EodhdKeyStore;
use crate::store::prices::PriceRepository;
use crate::store::MarketDataStore;

use super::layer::{ProductLayer, ProductLayerFactory, ProductsFactory};

/// Every repository the service needs, as one object the API can hold.
///
/// [`MarketDataStore`] covers the ops, exchange, price and ledger repositories; the product
/// layer needs the same object as a [`ProductsStore`]. Declaring both as supertraits is what
/// lets one `Arc<dyn AppStore>` be handed to either side without a second connection pool.
pub trait AppStore: MarketDataStore + ProductsStore + RefreshStore {}

impl<T: ?Sized> AppStore for T where T: MarketDataStore + ProductsStore + RefreshStore {}

/// Stop the process from inside it. The service binary installs the hook; tests leave it out.
pub type ShutdownHook = Arc<dyn Fn() + Send + Sync>;

/// The process-wide state handlers and the scheduler read.
pub struct Shared {
    settings: RwLock<Settings>,
    products: RwLock<Option<Arc<dyn ProductLayer>>>,
    factory: Arc<dyn ProductLayerFactory>,
    store: Option<Arc<dyn AppStore>>,
    market: Arc<MarketClock>,
    clock: Arc<dyn Clock>,
    gate: Arc<RefreshGate>,
    keys: Arc<EodhdKeyStore>,
    scheduler: RwLock<Option<Arc<RefreshScheduler>>>,
    admin_token: Option<String>,
    shutdown: Option<ShutdownHook>,
}

impl Shared {
    /// A state with no store and no products, for the clock-only endpoints and for tests.
    pub fn new(settings: Settings, market: Arc<MarketClock>, clock: Arc<dyn Clock>) -> Self {
        Self {
            settings: RwLock::new(settings),
            products: RwLock::new(None),
            factory: Arc::new(ProductsFactory),
            store: None,
            market,
            clock,
            gate: Arc::new(RefreshGate::new()),
            keys: Arc::new(EodhdKeyStore::windows()),
            scheduler: RwLock::new(None),
            admin_token: std::env::var("MDS_ADMIN_TOKEN")
                .ok()
                .map(|token| token.trim().to_string())
                .filter(|token| !token.is_empty()),
            shutdown: None,
        }
    }

    pub fn with_store(mut self, store: Arc<dyn AppStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Inject the product layer directly, as the Python suite's `FakeProducts` did.
    pub fn with_products(self, products: Arc<dyn ProductLayer>) -> Self {
        *self
            .products
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(products);
        self
    }

    pub fn with_factory(mut self, factory: Arc<dyn ProductLayerFactory>) -> Self {
        self.factory = factory;
        self
    }

    pub fn with_key_store(mut self, keys: Arc<EodhdKeyStore>) -> Self {
        self.keys = keys;
        self
    }

    pub fn with_admin_token(mut self, token: Option<String>) -> Self {
        self.admin_token = token.filter(|token| !token.trim().is_empty());
        self
    }

    pub fn with_shutdown(mut self, hook: ShutdownHook) -> Self {
        self.shutdown = Some(hook);
        self
    }

    /// The settings, as a snapshot. Mutating the clone cannot change the service.
    pub fn settings(&self) -> Settings {
        self.settings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Replace the settings after a key change, before rebuilding the product layer.
    pub fn set_settings(&self, settings: Settings) {
        *self
            .settings
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = settings;
    }

    pub fn products(&self) -> Option<Arc<dyn ProductLayer>> {
        self.products
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn store(&self) -> Option<Arc<dyn AppStore>> {
        self.store.clone()
    }

    pub fn market(&self) -> &Arc<MarketClock> {
        &self.market
    }

    /// The time source. Distinct from [`Self::market`]: the reference data says when an exchange
    /// trades, this says what time it is.
    pub fn clock(&self) -> &dyn Clock {
        self.clock.as_ref()
    }

    pub fn now_utc(&self) -> DateTime<chrono::Utc> {
        self.clock.now_utc()
    }

    pub fn gate(&self) -> &Arc<RefreshGate> {
        &self.gate
    }

    pub fn keys(&self) -> &Arc<EodhdKeyStore> {
        &self.keys
    }

    pub fn admin_token(&self) -> Option<&str> {
        self.admin_token.as_deref()
    }

    /// Install the collection schedule once the runner (which needs this state) exists.
    pub fn set_scheduler(&self, scheduler: Arc<RefreshScheduler>) {
        *self
            .scheduler
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(scheduler);
    }

    pub fn scheduler(&self) -> Option<Arc<RefreshScheduler>> {
        self.scheduler
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Build and install the production collection schedule around the current products.
    ///
    /// This mirrors the source service's `_build_schedule`: it captures the product layer,
    /// store and settings as they are at startup, and starts the loop only when
    /// `MDS_SCHEDULE_ENABLED` is true. It returns `false` when there is no product layer or no
    /// store — the condition under which `/schedule` and `POST /schedule/run` answer 503.
    ///
    /// `tick_seconds` is the loop interval (the source used twenty); a test pins it lower.
    pub fn install_schedule(&self, tick_seconds: u64) -> bool {
        let Some(products) = self.products() else {
            return false;
        };
        let Some(store) = self.store() else {
            return false;
        };
        let settings = self.settings();
        let schedule = Arc::new(SessionSchedule::new(
            self.market.clone(),
            settings.schedule_offset_minutes,
            settings.schedule_retry_minutes,
        ));
        let refresh_store: Arc<dyn RefreshStore> = store.clone();
        let refresh = Arc::new(StoreRefresh::new(
            refresh_store,
            products.vendor(),
            settings.clone(),
            self.gate.clone(),
            self.clock.clone(),
        ));
        let price_repository: Arc<dyn PriceRepository> = store;
        let runner: Arc<dyn MarketRunner> = Arc::new(Collector::new(
            products,
            price_repository,
            refresh,
            self.clock.clone(),
            schedule.clone(),
            settings.schedule_price_universe,
        ));
        let scheduler = Arc::new(RefreshScheduler::new(schedule, runner, MARKETS.to_vec()));
        self.set_scheduler(scheduler.clone());
        if settings.schedule_enabled {
            scheduler.start(tick_seconds);
        }
        true
    }

    /// Rebuild the product layer from the current settings, after a key change.
    ///
    /// A build failure leaves no product layer, exactly as it does at startup. When there is no
    /// store (a test injection) there is nothing to rebuild from and the injected layer stays.
    pub async fn rebuild_products(&self) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let settings = self.settings();
        let products_store: Arc<dyn ProductsStore> = store;
        let built = self.factory.build(&settings, products_store).await;
        *self
            .products
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = built;
    }

    /// Ask the process to stop, when this state owns a stop hook.
    pub fn request_shutdown(&self) {
        if let Some(hook) = &self.shutdown {
            hook();
        }
    }
}

/// What axum carries as state: one arc, cloned per request.
pub type AppState = Arc<Shared>;
