//! The four data endpoints.
//!
//! Each one has the same skeleton: name the caller, refuse when the product layer is absent,
//! delegate, render a refusal in the contract shape. The delegation is deliberate — no pricing
//! or caching decision lives here; the `products` tests and these tests therefore check the
//! same rules rather than two drifting copies.

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::products::AtrRequest;
use crate::usage::with_caller;

use super::dto::{
    require_sessions, require_symbols, AtrRequestBody, BarsRequestBody, PriceQuery, PricesRequest,
};
use super::errors::ApiError;
use super::request::caller;
use super::state::AppState;

pub async fn get_price(
    State(state): State<AppState>,
    Path(symbol): Path<String>,
    Query(query): Query<PriceQuery>,
    headers: HeaderMap,
) -> Response {
    let attributed = caller(&headers);
    let outcome = with_caller(attributed.as_deref(), async {
        let Some(products) = state.products() else {
            return Err(ApiError::vendor_unavailable());
        };
        products
            .price(
                &symbol,
                query.venue.as_deref(),
                query.max_age_seconds,
                query.asset_class.as_deref(),
                query.country_code.as_deref(),
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

pub async fn post_prices(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PricesRequest>,
) -> Response {
    if let Err(error) = require_symbols(&body.symbols) {
        return error.into_response();
    }
    let attributed = caller(&headers);
    let outcome = with_caller(attributed.as_deref(), async {
        let Some(products) = state.products() else {
            return Err(ApiError::vendor_unavailable());
        };
        products
            .prices(
                &body.symbols,
                body.venue.as_deref(),
                body.max_age_seconds,
                body.asset_class.as_deref(),
                body.country_code.as_deref(),
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

pub async fn post_atr(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<AtrRequestBody>,
) -> Response {
    if let Err(error) = require_symbols(&body.symbols) {
        return error.into_response();
    }
    let attributed = caller(&headers);
    let outcome = with_caller(attributed.as_deref(), async {
        let Some(products) = state.products() else {
            return Err(ApiError::vendor_unavailable());
        };
        products
            .atr_batch(AtrRequest {
                symbols: &body.symbols,
                period: body.period,
                method: body.method.as_deref(),
                smoothing: body.smoothing.as_deref(),
                venue: body.venue.as_deref(),
                price: body.price,
                multiplier: body.multiplier,
                direction: body.direction.as_deref().unwrap_or("long"),
                allow_fetch: true,
                asset_class: body.asset_class.as_deref(),
                country_code: body.country_code.as_deref(),
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

pub async fn post_bars(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<BarsRequestBody>,
) -> Response {
    if let Err(error) = require_symbols(&body.symbols) {
        return error.into_response();
    }
    if let Err(error) = require_sessions(body.sessions) {
        return error.into_response();
    }
    let attributed = caller(&headers);
    let outcome = with_caller(attributed.as_deref(), async {
        let Some(products) = state.products() else {
            return Err(ApiError::vendor_unavailable());
        };
        products
            .bars(
                &body.symbols,
                body.sessions,
                body.venue.as_deref(),
                body.allow_fetch,
                body.asset_class.as_deref(),
                body.country_code.as_deref(),
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
