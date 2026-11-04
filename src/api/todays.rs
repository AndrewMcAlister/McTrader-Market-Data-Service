//! The two "is there data for this country" endpoints.
//!
//! Both are gated by the same rule: a price must have been observed at or after the exchange's
//! most recent session open. `/todays-data-is-available` answers one boolean; `/todays-big-movers`
//! refuses to rank on yesterday's cache and says so with an empty, 200 result rather than an
//! error. That difference is the contract: "no data yet" is an answer a screener branches on,
//! not a failure it retries.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{TimeZone, Utc};
use serde_json::{json, Value};

use crate::domain::availability::{check, ExchangeSession};
use crate::store::models::Mover;
use crate::usage::with_caller;

use super::dto::{require_country, require_limit, require_non_negative, CountryQuery, MoversQuery};
use super::errors::ApiError;
use super::request::caller;
use super::state::AppState;

pub async fn get_availability(
    State(state): State<AppState>,
    Query(query): Query<CountryQuery>,
    headers: HeaderMap,
) -> Response {
    let attributed = caller(&headers);
    with_caller(attributed.as_deref(), async {
        let country =
            match require_country(query.country_code.as_deref(), &["query", "country_code"]) {
                Ok(country) => country,
                Err(error) => return error.into_response(),
            };
        let Some(store) = state.store() else {
            return ApiError::database_unavailable().into_response();
        };
        let now = state.now_utc();
        let Some(window) = state
            .market()
            .availability_session(country.session_exchange, now)
        else {
            return ApiError::internal(format!(
                "{} is not in the clock reference",
                country.session_exchange
            ))
            .into_response();
        };
        let latest = match store.latest_exchange_price(country.exchange).await {
            Ok(latest) => latest,
            Err(error) => {
                // An unreadable store cannot vouch for data; false is the honest answer.
                tracing::warn!(
                    "todays-data-is-available: could not read the price cache ({error})"
                );
                None
            }
        };
        let result = match check(country.key, latest, &window) {
            Ok(result) => result,
            Err(error) => {
                return ApiError::reason(StatusCode::BAD_REQUEST, error.message).into_response()
            }
        };

        let mut headers = HeaderMap::new();
        set_header(&mut headers, "X-Dataset-Country", &result.country);
        set_header(&mut headers, "X-Dataset-Exchange", &result.exchange);
        set_header(
            &mut headers,
            "X-Dataset-Latest-Price",
            &result
                .latest_utc
                .map(crate::clock::iso_utc)
                .unwrap_or_default(),
        );
        set_header(
            &mut headers,
            "X-Session-Open",
            &result
                .session_open_utc
                .map(crate::clock::iso_utc)
                .unwrap_or_default(),
        );
        set_header(
            &mut headers,
            "X-Session-Close",
            &result
                .session_close_utc
                .map(crate::clock::iso_utc)
                .unwrap_or_default(),
        );
        set_header(
            &mut headers,
            "X-Session-Is-Open",
            if result.session_is_open {
                "true"
            } else {
                "false"
            },
        );
        (headers, Json(result.available)).into_response()
    })
    .await
}

pub async fn get_big_movers(
    State(state): State<AppState>,
    Query(query): Query<MoversQuery>,
    headers: HeaderMap,
) -> Response {
    let attributed = caller(&headers);
    with_caller(attributed.as_deref(), async {
        if let Err(error) = require_limit(query.limit) {
            return error.into_response();
        }
        if let Err(error) = require_non_negative("min_price", query.min_price) {
            return error.into_response();
        }
        if let Err(error) = require_non_negative("min_dollar_volume", query.min_dollar_volume) {
            return error.into_response();
        }
        let country =
            match require_country(query.country_code.as_deref(), &["query", "country_code"]) {
                Ok(country) => country,
                Err(error) => return error.into_response(),
            };
        let Some(store) = state.store() else {
            return ApiError::database_unavailable().into_response();
        };
        let now = state.now_utc();
        let Some(window) = state
            .market()
            .availability_session(country.session_exchange, now)
        else {
            return ApiError::internal(format!(
                "{} is not in the clock reference",
                country.session_exchange
            ))
            .into_response();
        };
        let latest = match store.latest_exchange_price(country.exchange).await {
            Ok(latest) => latest,
            Err(error) => {
                tracing::warn!("todays-big-movers: could not read the price cache ({error})");
                None
            }
        };
        let availability = match check(country.key, latest, &window) {
            Ok(result) => result,
            Err(error) => {
                return ApiError::reason(StatusCode::BAD_REQUEST, error.message).into_response()
            }
        };
        let session = session_json(&window);
        if !availability.available {
            return Json(json!({
                "available": false,
                "reason": "no price for this exchange was observed at or after its most recent \
                           session open",
                "country": availability.country,
                "exchange": availability.exchange,
                "session": session,
                "latest_price_utc": availability.latest_utc.map(crate::clock::iso_utc),
                "winners": [],
                "losers": [],
            }))
            .into_response();
        }

        let movers = match store
            .big_movers(
                country.exchange,
                query.limit,
                query.min_price,
                query.min_dollar_volume,
                availability.session_open_utc.map(|open| open.naive_utc()),
            )
            .await
        {
            Ok(movers) => movers,
            Err(error) => {
                tracing::error!("todays-big-movers: query failed: {error}");
                return ApiError::error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "could not read the price cache",
                    format!("StoreError: {error}"),
                )
                .into_response();
            }
        };

        Json(json!({
            "available": true,
            "country": availability.country,
            "exchange": availability.exchange,
            "session": session,
            "latest_price_utc": availability.latest_utc.map(crate::clock::iso_utc),
            "filters": {
                "limit": query.limit,
                "min_price": query.min_price,
                "min_dollar_volume": query.min_dollar_volume,
            },
            "counts": {
                "winners": movers.winners.len(),
                "losers": movers.losers.len(),
            },
            "market_cap_source": null,
            "winners": movers.winners.iter().map(mover_json).collect::<Vec<Value>>(),
            "losers": movers.losers.iter().map(mover_json).collect::<Vec<Value>>(),
        }))
        .into_response()
    })
    .await
}

fn session_json(window: &ExchangeSession) -> Value {
    json!({
        "open_utc": window.open_utc.map(crate::clock::iso_utc),
        "close_utc": window.close_utc.map(crate::clock::iso_utc),
        "is_open": window.is_open,
    })
}

fn mover_json(row: &Mover) -> Value {
    json!({
        "symbol": row.symbol,
        "price": row.price,
        "change_pct": row.change_pct,
        "volume": row.volume,
        "dollar_volume": row.dollar_volume,
        "previous_close": row.previous_close,
        "as_of_utc": row
            .as_of_utc
            .map(|stamp| crate::clock::iso_utc(Utc.from_utc_datetime(&stamp))),
        "source": row.source,
        "market_cap": null,
    })
}

fn set_header(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::try_from(value) {
        headers.insert(name, value);
    }
}
