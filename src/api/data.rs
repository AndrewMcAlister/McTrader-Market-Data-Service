//! The data endpoints.
//!
//! Each one has the same skeleton: name the caller, refuse when the product layer is absent,
//! delegate, render a refusal in the contract shape. The delegation is deliberate — no pricing
//! or caching decision lives here; the `products` tests and these tests therefore check the
//! same rules rather than two drifting copies.
//!
//! `/bars/intraday` and `/bars/bulk` sit here beside `/bars` because they are bar products: the
//! same exchange resolution, the same caller attribution and the same refusal shapes.

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::products::{AtrRequest, IntradayRequest as IntradayBatch};
use crate::usage::with_caller;

use super::dto::{
    require_asset_class, require_between, require_bulk_dates, require_country, require_sessions,
    require_symbols, require_timeframe, AtrRequestBody, BarsRequestBody, BulkBody, IntradayBody,
    PriceQuery, PricesRequest, INTRADAY_LIMIT_DEFAULT, INTRADAY_LIMIT_MAX,
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
    let country = match require_country(query.country_code.as_deref(), &["query", "country_code"]) {
        Ok(country) => country,
        Err(error) => return error.into_response(),
    };
    let asset_class = match require_asset_class(query.asset_class.as_deref())
    {
        Ok(asset_class) => asset_class,
        Err(error) => return error.into_response(),
    };
    let attributed = caller(&headers);
    let outcome = with_caller(attributed.as_deref(), async {
        let Some(products) = state.products() else {
            return Err(ApiError::vendor_unavailable());
        };
        products
            .price(
                &symbol,
                query.exchange.as_deref(),
                query.max_age_seconds,
                asset_class.as_deref(),
                Some(country.key),
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
    let country = match require_country(body.country_code.as_deref(), &["body", "country_code"]) {
        Ok(country) => country,
        Err(error) => return error.into_response(),
    };
    let asset_class = match require_asset_class(body.asset_class.as_deref())
    {
        Ok(asset_class) => asset_class,
        Err(error) => return error.into_response(),
    };
    let attributed = caller(&headers);
    let outcome = with_caller(attributed.as_deref(), async {
        let Some(products) = state.products() else {
            return Err(ApiError::vendor_unavailable());
        };
        products
            .prices(
                &body.symbols,
                body.exchange.as_deref(),
                body.max_age_seconds,
                asset_class.as_deref(),
                Some(country.key),
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
    let country = match require_country(body.country_code.as_deref(), &["body", "country_code"]) {
        Ok(country) => country,
        Err(error) => return error.into_response(),
    };
    let asset_class = match require_asset_class(body.asset_class.as_deref())
    {
        Ok(asset_class) => asset_class,
        Err(error) => return error.into_response(),
    };
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
                exchange: body.exchange.as_deref(),
                price: body.price,
                multiplier: body.multiplier,
                direction: body.direction.as_deref().unwrap_or("long"),
                allow_fetch: true,
                asset_class: asset_class.as_deref(),
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
    let country = match require_country(body.country_code.as_deref(), &["body", "country_code"]) {
        Ok(country) => country,
        Err(error) => return error.into_response(),
    };
    let asset_class = match require_asset_class(body.asset_class.as_deref())
    {
        Ok(asset_class) => asset_class,
        Err(error) => return error.into_response(),
    };
    let attributed = caller(&headers);
    let outcome = with_caller(attributed.as_deref(), async {
        let Some(products) = state.products() else {
            return Err(ApiError::vendor_unavailable());
        };
        products
            .bars(
                &body.symbols,
                body.sessions,
                body.exchange.as_deref(),
                body.allow_fetch,
                asset_class.as_deref(),
                Some(country.key),
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

/// `POST /bars/intraday` — 5m bars and their aggregates from the owned tape.
pub async fn post_intraday(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<IntradayBody>,
) -> Response {
    if let Err(error) = require_symbols(&body.symbols) {
        return error.into_response();
    }
    let timeframe = match require_timeframe(body.timeframe.as_deref(), &["body", "timeframe"]) {
        Ok(timeframe) => timeframe,
        Err(error) => return error.into_response(),
    };
    let limit = body.limit.unwrap_or(INTRADAY_LIMIT_DEFAULT);
    if let Err(error) = require_between(1, INTRADAY_LIMIT_MAX, limit, &["body", "limit"]) {
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
            .intraday(IntradayBatch {
                symbols: &body.symbols,
                timeframe,
                limit,
                allow_fetch: body.allow_fetch,
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

/// `POST /bars/bulk` — whole-market closes for one to ten sessions.
pub async fn post_bulk(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<BulkBody>,
) -> Response {
    let dates = match require_bulk_dates(&body.dates) {
        Ok(dates) => dates,
        Err(error) => return error.into_response(),
    };
    // The whole-market pull is keyed by the vendor's market parameter and served for the two
    // markets this service runs; it is not a per-symbol request, so it names `market`.
    let market = match require_country(Some(&body.market), &["body", "market"]) {
        Ok(market) => market,
        Err(error) => return error.into_response(),
    };
    let attributed = caller(&headers);
    let market = market.exchange;
    let outcome = with_caller(attributed.as_deref(), async {
        let Some(products) = state.products() else {
            return Err(ApiError::vendor_unavailable());
        };
        products
            .bulk_closes(market, &dates, body.allow_fetch)
            .await
            .map_err(ApiError::from)
    })
    .await;
    match outcome {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => error.into_response(),
    }
}
