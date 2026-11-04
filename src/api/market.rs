//! The market-clock endpoints and the schedule's operator surface.
//!
//! The dual-clock model is the source service's, read from `data/clock.json` (or the live
//! `dbo.Exchanges` table) so the books can point at this service by changing a URL rather than
//! a parser. `/trading_sessions` deliberately reads a bare time in the operator's zone;
//! `/market/clock` reads it in UTC. See [`super::reference`] for why those differ.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

use crate::clock::{resolve_time_zone, BrokerRow};
use crate::error::py_repr;
use crate::store::models::ExchangeRow;
use crate::usage::with_caller;

use super::dto::{query_bool, ClockQuery, ScheduleRunQuery, SessionQuery, TradingSessionsQuery};
use super::errors::ApiError;
use super::reference::{clock_reference, local_instant_in_zone, operator_time};
use super::request::{admin_guard, caller};
use super::state::AppState;

pub async fn get_schedule(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let attributed = caller(&headers);
    with_caller(attributed.as_deref(), async {
        let Some(scheduler) = state.scheduler() else {
            return ApiError::reason(
                StatusCode::SERVICE_UNAVAILABLE,
                "no schedule: the product layer is unavailable",
            )
            .into_response();
        };
        let settings = state.settings();
        match scheduler.status() {
            Ok(mut payload) => {
                payload["offset_minutes"] = json!(settings.schedule_offset_minutes);
                payload["retry_minutes"] = json!(settings.schedule_retry_minutes);
                Json(payload).into_response()
            }
            Err(error) => ApiError::internal(error.message).into_response(),
        }
    })
    .await
}

pub async fn post_schedule_run(
    State(state): State<AppState>,
    Query(query): Query<ScheduleRunQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(denied) = admin_guard(&state, &headers) {
        return denied.into_response();
    }
    let attributed = caller(&headers);
    with_caller(attributed.as_deref(), async {
        let Some(scheduler) = state.scheduler() else {
            return ApiError::reason(
                StatusCode::SERVICE_UNAVAILABLE,
                "no schedule: the product layer is unavailable",
            )
            .into_response();
        };
        let result = scheduler.run_market(&query.market.to_uppercase()).await;
        let status = if result.ok {
            StatusCode::OK
        } else {
            StatusCode::CONFLICT
        };
        (status, Json(result.payload)).into_response()
    })
    .await
}

pub async fn get_market_clock(
    State(state): State<AppState>,
    Query(query): Query<ClockQuery>,
    headers: HeaderMap,
) -> Response {
    let attributed = caller(&headers);
    with_caller(attributed.as_deref(), async {
        let now = state.now_utc();
        let reference = match clock_reference(query.time.as_deref(), query.at.as_deref(), now) {
            // "omitted means now" — and "now" is the injected clock, not a second reading of
            // the system time inside the clock reference.
            Ok(reference) => reference.unwrap_or(now),
            Err(message) => {
                return ApiError::reason(StatusCode::BAD_REQUEST, message).into_response()
            }
        };
        match state
            .market()
            .broker_clock(query.broker_id as i32, Some(reference))
        {
            Ok(payload) => Json(payload).into_response(),
            Err(error) => {
                ApiError::reason(StatusCode::NOT_FOUND, py_repr(&error.message)).into_response()
            }
        }
    })
    .await
}

pub async fn get_market_session(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
    headers: HeaderMap,
) -> Response {
    let attributed = caller(&headers);
    with_caller(attributed.as_deref(), async {
        let market = state.market();
        // The rule resolved is always the shares rule: the request no longer names an asset
        // class. The reference tables still carry the other rules; a caller reads them from
        // `/market/exchanges` and no longer picks one here.
        let Some(asset_class_id) = market.asset_class_id("shares") else {
            return ApiError::internal("the clock reference has no shares asset class")
                .into_response();
        };
        let now = state.now_utc();
        let reference = match clock_reference(None, query.at.as_deref(), now) {
            Ok(reference) => reference.unwrap_or(now),
            Err(message) => {
                return ApiError::reason(StatusCode::BAD_REQUEST, message).into_response()
            }
        };
        let session = market.resolve(query.broker_id as i32, asset_class_id, reference);
        let mut payload = session.as_json();
        if let Some(object) = payload.as_object_mut() {
            object.insert("broker_id".to_string(), json!(query.broker_id));
            object.insert("asset_class".to_string(), json!("shares"));
            object.insert(
                "reference_source".to_string(),
                Value::String(market.source().to_string()),
            );
        }
        Json(payload).into_response()
    })
    .await
}

pub async fn get_market_exchanges(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let attributed = caller(&headers);
    with_caller(attributed.as_deref(), async {
        let market = state.market();
        let brokers: Vec<Value> = market.brokers().map(broker_json).collect();
        let exchanges: Vec<Value> = market.exchanges().map(exchange_json).collect();
        let mut broker_exchanges = Map::new();
        for broker in market.brokers() {
            let ids: Vec<i32> = market.broker_exchange_ids(broker.id).to_vec();
            broker_exchanges.insert(broker.id.to_string(), json!(ids));
        }
        let rules: Vec<Value> = market
            .broker_rules()
            .map(|rule| {
                let name = market
                    .asset_classes()
                    .find(|row| row.id == rule.asset_class_id)
                    .and_then(|row| row.name.clone());
                json!({
                    "broker_id": rule.broker_id,
                    "asset_class_id": rule.asset_class_id,
                    "asset_class": name,
                    "active": rule.active,
                    "exchange_hours": rule.exchange_hours,
                    "synthetic_24": rule.synthetic_24,
                    "weekend": rule.weekend,
                })
            })
            .collect();
        Json(json!({
            "source": market.source(),
            "brokers": brokers,
            "exchanges": exchanges,
            "broker_exchanges": broker_exchanges,
            "rules": rules,
        }))
        .into_response()
    })
    .await
}

pub async fn get_trading_sessions(
    State(state): State<AppState>,
    Query(query): Query<TradingSessionsQuery>,
    headers: HeaderMap,
) -> Response {
    let attributed = caller(&headers);
    with_caller(attributed.as_deref(), async {
        let settings = state.settings();
        let market = state.market();
        let now = state.now_utc();
        let use_broker_timezone = query_bool(query.use_broker_timezone.as_deref(), false);
        let broker_id = query.broker_id as i32;

        let mut reference = now;
        if let Some(time) = query.time.as_deref() {
            let (hour, minute) = match operator_time(time) {
                Ok(parts) => parts,
                Err(message) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "error": message,
                            "error_type": "validation_error",
                            "timestamp": crate::clock::iso_utc(now),
                        })),
                    )
                        .into_response();
                }
            };
            let zone = if use_broker_timezone {
                let broker: Option<&BrokerRow> = market.brokers().find(|row| row.id == broker_id);
                resolve_time_zone(broker.and_then(|row| row.timezone.as_deref()))
            } else {
                resolve_time_zone(Some(&settings.clock_user_timezone))
            };
            reference = local_instant_in_zone(now, zone, hour, minute);
        }

        match market.broker_clock(broker_id, Some(reference)) {
            Ok(mut payload) => {
                payload["reference_time_interpreted_in"] = Value::String(if use_broker_timezone {
                    "broker".to_string()
                } else {
                    settings.clock_user_timezone.clone()
                });
                Json(payload).into_response()
            }
            Err(error) => (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": py_repr(&error.message),
                    "error_type": "not_found",
                    "timestamp": crate::clock::iso_utc(now),
                })),
            )
                .into_response(),
        }
    })
    .await
}

fn broker_json(row: &BrokerRow) -> Value {
    json!({
        "id": row.id,
        "name": row.name,
        "timezone": row.timezone,
        "active": row.active,
    })
}

fn exchange_json(row: &ExchangeRow) -> Value {
    json!({
        "id": row.id,
        "code": row.code,
        "name": row.name,
        "country": row.country,
        "currency": row.currency,
        "timezone": row.timezone,
        "opens": row.opens,
        "closes": row.closes,
    })
}
