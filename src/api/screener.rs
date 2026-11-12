//! `POST /screener` and `POST /profile`: the two vendor-query routes.
//!
//! Both are transports. `screener` sends the caller's query to the vendor and returns the rows
//! untouched; `profile` pins one code-and-venue query per symbol and maps the row onto a fixed
//! shape. Neither decides which companies matter — that is policy and stays in the caller.
//!
//! The skeleton is `data.rs`'s: name the caller, refuse when the product layer is absent,
//! delegate, render the refusal in the contract shape.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::products::ProfileBatch;
use crate::usage::with_caller;

use super::dto::{
    require_between, require_country, require_non_negative_integer, require_profile_symbols,
    require_symbols, ProfilesRequest, ScreenerRequest, SCREENER_LIMIT_DEFAULT, SCREENER_LIMIT_MAX,
};
use super::errors::ApiError;
use super::request::caller;
use super::state::AppState;

/// `POST /screener` — the vendor's screen for one market, rows untouched.
pub async fn post_screener(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ScreenerRequest>,
) -> Response {
    let country = match require_country(body.country_code.as_deref(), &["body", "country_code"]) {
        Ok(country) => country,
        Err(error) => return error.into_response(),
    };
    let limit = body.limit.unwrap_or(SCREENER_LIMIT_DEFAULT);
    if let Err(error) = require_between(1, SCREENER_LIMIT_MAX, limit, &["body", "limit"]) {
        return error.into_response();
    }
    let offset = body.offset.unwrap_or(0);
    if let Err(error) = require_non_negative_integer(offset, &["body", "offset"]) {
        return error.into_response();
    }
    let attributed = caller(&headers);
    let outcome = with_caller(attributed.as_deref(), async {
        let Some(products) = state.products() else {
            return Err(ApiError::vendor_unavailable());
        };
        products
            .screener(
                country.key,
                body.exchange.as_deref(),
                &body.filters,
                body.sort.as_deref(),
                limit,
                offset,
            )
            .await
            .map_err(ApiError::from)
    })
    .await;
    match outcome {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => error.into_response(),
    }
}

/// `POST /profile` — up to 25 listings, one pinned vendor call per uncached symbol.
pub async fn post_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ProfilesRequest>,
) -> Response {
    if let Err(error) = require_symbols(&body.symbols) {
        return error.into_response();
    }
    if let Err(error) = require_profile_symbols(&body.symbols) {
        return error.into_response();
    }
    let country = match require_country(body.country_code.as_deref(), &["body", "country_code"]) {
        Ok(country) => country,
        Err(error) => return error.into_response(),
    };
    let attributed = caller(&headers);
    let outcome = with_caller(attributed.as_deref(), async {
        let Some(products) = state.products() else {
            return Err(ApiError::vendor_unavailable());
        };
        products
            .profiles(ProfileBatch {
                symbols: &body.symbols,
                exchange: body.exchange.as_deref(),
                country_code: Some(country.key),
            })
            .await
            .map_err(ApiError::from)
    })
    .await;
    match outcome {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => error.into_response(),
    }
}
