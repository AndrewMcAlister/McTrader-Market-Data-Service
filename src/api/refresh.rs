//! `POST /refresh`: pull a whole market's daily bars, on demand.
//!
//! The policy lives in [`crate::refresh`] and is shared with the scheduler and the daily job.
//! This handler only maps the request into that policy and the outcome into an HTTP status.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::jobs::daily::LibraryJobs;
use crate::refresh::{refresh_daily_bars, RefreshRequest, RefreshStatus};
use crate::usage::with_caller;

use super::dto::RefreshRequestBody;
use super::errors::ApiError;
use super::request::{admin_guard, caller};
use super::state::AppState;

pub async fn post_refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<RefreshRequestBody>>,
) -> Response {
    if let Err(denied) = admin_guard(&state, &headers) {
        return denied.into_response();
    }
    let Some(products) = state.products() else {
        return ApiError::vendor_unavailable().into_response();
    };
    let Some(store) = state.store() else {
        return ApiError::database_unavailable().into_response();
    };

    let options = body.map(|Json(body)| body).unwrap_or_default();
    let request = RefreshRequest {
        market: options.market,
        session_date: options.session_date,
        force: options.force,
        include_indicators: options.include_indicators,
    };
    let settings = state.settings();
    let attributed = caller(&headers);
    let vendor = products.vendor();

    let outcome = with_caller(attributed.as_deref(), async {
        refresh_daily_bars(
            store.as_ref(),
            vendor.as_deref(),
            &LibraryJobs,
            &settings,
            state.gate(),
            state.clock(),
            request,
        )
        .await
    })
    .await;

    match outcome {
        Ok(summary) => {
            let status = match summary.status {
                RefreshStatus::Refreshed => StatusCode::ACCEPTED,
                RefreshStatus::Busy => StatusCode::CONFLICT,
                RefreshStatus::NoVendorKey => StatusCode::SERVICE_UNAVAILABLE,
                RefreshStatus::AlreadyFresh => StatusCode::OK,
            };
            let body = serde_json::to_value(&summary).unwrap_or(serde_json::Value::Null);
            (status, Json(body)).into_response()
        }
        Err(error) => ApiError::from(error).into_response(),
    }
}
