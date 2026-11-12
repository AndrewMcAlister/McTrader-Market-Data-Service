//! One rendering for every refusal.
//!
//! The source service had three shapes, and the difference was meaningful:
//!
//! * a caller's mistake, rendered as `{"error": reason, "detail": detail}` at the status the
//!   error carries;
//! * a database lock wait, rendered as 503 `{"error": "database-busy", ..., "retryable": true}`;
//! * a vendor refusal, rendered as 502 `{"error": reason, "detail": detail}`.
//!
//! Everything here preserves those bodies. [`ApiError`] owns the status and the JSON so a
//! handler can return one value and cannot accidentally use the wrong shape for a branch.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::error::{MarketDataError, ProviderFailure};
use crate::products::ProductError;
use crate::refresh::RefreshFailure;
use crate::store::StoreError;

/// A refusal on its way out: the status and the exact body.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: StatusCode,
    pub body: Value,
}

impl ApiError {
    pub fn new(status: StatusCode, body: Value) -> Self {
        Self { status, body }
    }

    /// The `{"error", "detail"}` shape, at an arbitrary status.
    pub fn error(status: StatusCode, reason: &str, detail: impl Into<String>) -> Self {
        Self::new(status, json!({"error": reason, "detail": detail.into()}))
    }

    /// A bare `{"error": ...}` body, which several endpoints use instead of the pair.
    pub fn reason(status: StatusCode, reason: impl Into<String>) -> Self {
        Self::new(status, json!({"error": reason.into()}))
    }

    pub fn internal(detail: impl Into<String>) -> Self {
        Self::error(StatusCode::INTERNAL_SERVER_ERROR, "internal-error", detail)
    }

    /// A missing product layer: the source service's `{"error": "vendor-unavailable"}`.
    pub fn vendor_unavailable() -> Self {
        Self::reason(StatusCode::SERVICE_UNAVAILABLE, "vendor-unavailable")
    }

    pub fn database_unavailable() -> Self {
        Self::reason(StatusCode::SERVICE_UNAVAILABLE, "database-unavailable")
    }

    /// A store failure, classified the way the source's exception handler did.
    pub fn store(error: &StoreError) -> Self {
        match error.busy_detail() {
            Some(detail) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error": "database-busy", "detail": detail, "retryable": true}),
            ),
            None => Self::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "database-error",
                format!("StoreError: {error}"),
            ),
        }
    }

    /// A FastAPI-shaped validation refusal. The exact `msg` is pydantic's; the shape is what
    /// a client branches on, and the tests pin the status and the location.
    pub fn validation(location: &[&str], kind: &str, message: &str, input: Value) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({"detail": [{
                "type": kind,
                "loc": location,
                "msg": message,
                "input": input,
            }]}),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

impl From<MarketDataError> for ApiError {
    fn from(error: MarketDataError) -> Self {
        let status =
            StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        Self::error(status, &error.reason, error.detail)
    }
}

impl From<ProviderFailure> for ApiError {
    fn from(error: ProviderFailure) -> Self {
        Self::error(StatusCode::BAD_GATEWAY, &error.reason, error.detail)
    }
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        Self::store(&error)
    }
}

impl From<ProductError> for ApiError {
    fn from(error: ProductError) -> Self {
        match error {
            ProductError::Refused(error) => error.into(),
            ProductError::Store(error) => Self::store(&error),
            ProductError::Provider(error) => error.into(),
            // No vendor client in this process: a keyless build still serves Yahoo and
            // Binance, so this is a per-route 503 rather than a startup failure.
            ProductError::Unavailable => Self::vendor_unavailable(),
        }
    }
}

impl From<RefreshFailure> for ApiError {
    fn from(error: RefreshFailure) -> Self {
        match error {
            RefreshFailure::Store(error) => Self::store(&error),
            RefreshFailure::Provider(error) => error.into(),
            RefreshFailure::Invalid(detail) => {
                Self::error(StatusCode::INTERNAL_SERVER_ERROR, "refresh-failed", detail)
            }
        }
    }
}
