//! The HTTP surface: one route per source endpoint, the same statuses and bodies.
//!
//! Layers, from the route inwards:
//!
//! * [`router`] wires methods and paths, and catches the two framework-shaped refusals the
//!   source service answered itself (404, 405);
//! * the handler modules ([`data`], [`ops`], [`secrets`], [`market`], [`todays`], [`refresh`],
//!   [`screener`]) read the request, name the caller and delegate;
//! * [`state`] is the one shared object every handler receives;
//! * [`dto`], [`errors`] and [`reference`] are the contract's vocabulary — request shapes,
//!   refusal bodies and the two time conventions;
//! * [`layer`] is the seam a contract test replaces with a fake product layer.
//!
//! No handler owns a policy. If a rule decides whether data may be served, it lives in the
//! library crate below this module and is called through [`layer::ProductLayer`].

pub mod data;
pub mod dto;
pub mod errors;
pub mod layer;
pub mod market;
pub mod openapi;
pub mod ops;
pub mod reference;
pub mod refresh;
pub mod request;
pub mod router;
pub mod screener;
pub mod secrets;
pub mod state;
pub mod todays;

pub use router::build_router;
pub use state::{AppState, AppStore, Shared, ShutdownHook};

#[cfg(test)]
mod tests;
