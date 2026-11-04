//! McTrader market data service.
//!
//! One library holds every rule the REST API, the daily job, the scheduler, the tools and the
//! MCP proxy serve. The binaries in `src/bin` are thin entry points; nothing in them may
//! reimplement a cache policy, an ATR definition or a venue rule.

pub mod api;
pub mod clock;
pub mod collector;
pub mod config;
pub mod domain;
pub mod error;
pub mod jobs;
pub mod logging;
pub mod mcp;
pub mod presence;
pub mod products;
pub mod providers;
pub mod refresh;
pub mod schedule;
pub mod secrets;
pub mod store;
pub mod usage;
