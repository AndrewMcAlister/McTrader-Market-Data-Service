//! `/secrets/eodhd`: store and inspect the vendor key.
//!
//! The key itself never appears in a response or a log line. `GET` returns a mask and the
//! identifiers that say *where* it lives; the write endpoint is behind the same
//! `X-Admin-Token` guard as a refresh and refuses when a key is already configured.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::config::KeySource;

use super::dto::EodhdKeyRequest;
use super::errors::ApiError;
use super::request::admin_guard;
use super::state::AppState;

pub async fn post_secret(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<EodhdKeyRequest>,
) -> Response {
    if let Err(denied) = admin_guard(&state, &headers) {
        return denied.into_response();
    }
    // Set once: the endpoint exists to get a key into this account's vault at first start. Once
    // a key is configured it cannot be replaced or removed over HTTP.
    let current = state.settings();
    if !current.eodhd_api_key.is_empty() {
        return ApiError::error(
            StatusCode::CONFLICT,
            "key-already-set",
            format!(
                "a vendor key is already configured (source: {}); it cannot be replaced over HTTP",
                current.eodhd_key_source.as_str(),
            ),
        )
        .into_response();
    }
    if let Err(failure) = state.keys().write(&body.api_key) {
        return ApiError::error(
            StatusCode::from_u16(failure.kind.status()).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            failure.kind.reason(),
            failure.detail,
        )
        .into_response();
    }

    // Take effect immediately rather than at the next restart — the endpoint exists exactly
    // so a key written into this account's vault is used without one.
    let mut updated = state.settings();
    updated.eodhd_api_key = body.api_key.trim().to_string();
    updated.eodhd_key_source = KeySource::Keyring;
    state.set_settings(updated);
    state.rebuild_products().await;

    let status = serde_json::to_value(state.keys().status()).unwrap_or_else(|_| json!({}));
    Json(json!({
        "ok": true,
        "stored": true,
        "vendor_configured": state.products().is_some(),
        "status": status,
    }))
    .into_response()
}

pub async fn get_secret(State(state): State<AppState>) -> Response {
    let settings = state.settings();
    let status = serde_json::to_value(state.keys().status()).unwrap_or_else(|_| json!({}));
    let mut body = status;
    if let Some(object) = body.as_object_mut() {
        object.insert(
            "in_use".to_string(),
            Value::Bool(!settings.eodhd_api_key.is_empty()),
        );
        object.insert(
            "in_use_source".to_string(),
            Value::String(settings.eodhd_key_source.as_str().to_string()),
        );
        object.insert(
            "vendor_configured".to_string(),
            Value::Bool(state.products().is_some()),
        );
    }
    Json(body).into_response()
}
