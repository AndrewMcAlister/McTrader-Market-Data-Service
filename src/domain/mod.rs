//! The rules that do not need a database, a network or a clock.
//!
//! Every module here is pure: values in, values out. That is what makes the arithmetic
//! testable against known-exact series and lets the HTTP, job and MCP layers share one
//! implementation of each rule instead of each carrying its own copy.

pub mod asset_class;
pub mod atr;
pub mod availability;
pub mod sessions;
pub mod symbols;
pub mod venues;
