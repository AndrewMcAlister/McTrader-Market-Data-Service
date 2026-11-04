//! Reading the two request headers the service cares about.
//!
//! `X-Admin-Token` guards a secret write; `X-MarketData-Caller` names who is spending the
//! vendor quota. Both are plaintext localhost conventions, not an authentication scheme —
//! see [`admin_guard`] for what the token does and does not promise.

use axum::http::{HeaderMap, StatusCode};

use super::errors::ApiError;
use super::state::Shared;

/// The caller named by `X-MarketData-Caller`, or `None`.
///
/// The header is the point of attribution: a shared service's spend is only accountable if
/// you can say which book spent it. A blank header is treated as absent, so the endpoint's
/// fallback path records what was asked rather than an empty caller.
pub fn caller(headers: &HeaderMap) -> Option<String> {
    headers
        .get("X-MarketData-Caller")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Refuse a privileged call unless the caller proved it may.
///
/// When `MDS_ADMIN_TOKEN` is set it is required. When it is not, the call is allowed — the
/// service binds to localhost on a single-user machine, and refusing by default would make
/// first install impossible — but every such call is logged, so the gap is never silent.
pub fn admin_guard(state: &Shared, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(expected) = state.admin_token() else {
        tracing::warn!(
            "privileged endpoint called with no MDS_ADMIN_TOKEN configured; any local \
             process can rewrite the vendor key"
        );
        return Ok(());
    };
    let supplied = headers
        .get("X-Admin-Token")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .unwrap_or_default();
    if supplied == expected {
        return Ok(());
    }
    Err(ApiError::error(
        StatusCode::UNAUTHORIZED,
        "unauthorised",
        "X-Admin-Token is required and did not match",
    ))
}
