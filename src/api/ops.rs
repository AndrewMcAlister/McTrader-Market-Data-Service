//! `/healthz` and `/stats`: infrastructure, not data.
//!
//! Both must answer even when a dependency is broken — that is the point of a health check —
//! so failures are reported as fields, never as an error response. `/stats` is the exception
//! only for an unreadable *store count*, which is the whole body it was asked for.

use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::connect_info::ConnectInfo;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::NaiveDate;
use serde_json::{json, Value};

use crate::store::models::{ExchangeBarCount, StoreStats, UsageSummary};

use super::errors::ApiError;
use super::state::AppState;

pub async fn healthz(State(state): State<AppState>) -> Response {
    let settings = state.settings();
    let (database_ok, database_error) = match state.store() {
        None => (
            false,
            Some("StoreError: no database is configured".to_string()),
        ),
        Some(store) => match store.ping().await {
            Ok(()) => (true, None),
            Err(error) => (false, Some(format!("StoreError: {error}"))),
        },
    };
    let product_layer = state.products().is_some();
    let status = if database_ok && product_layer {
        "ok"
    } else {
        "degraded"
    };
    Json(json!({
        "status": status,
        "version": env!("CARGO_PKG_VERSION"),
        "vendor_configured": !settings.eodhd_api_key.is_empty(),
        // The source, never the key. "keyring" under a LocalSystem service would mean the
        // service is running as the interactive user, which is worth seeing here.
        "key_source": settings.eodhd_key_source.as_str(),
        "database": settings.db_database,
        "database_ok": database_ok,
        "database_error": database_error,
    }))
    .into_response()
}

pub async fn stats(State(state): State<AppState>) -> Response {
    let settings = state.settings();
    let Some(store) = state.store() else {
        return ApiError::database_unavailable().into_response();
    };
    let store_stats = match store.stats().await {
        Ok(stats) => stats,
        Err(error) => return ApiError::store(&error).into_response(),
    };

    let mut payload = json!({
        "database": store_stats_json(&store_stats),
        "settings": {
            "price_ttl_seconds": settings.price_ttl_seconds,
            "bulk_breakeven": settings.bulk_breakeven,
            "atr_periods": settings.atr_periods,
            "database": settings.db_database,
        },
    });

    // The product layer's stats replace `settings` with its fuller eight-key version, exactly
    // as the source service's `payload.update(products.stats())` did.
    if let Some(products) = state.products() {
        let product_stats =
            serde_json::to_value(products.stats().await).unwrap_or_else(|_| json!({}));
        if let (Some(target), Value::Object(extra)) = (payload.as_object_mut(), product_stats) {
            target.extend(extra);
        }
    }

    // The ledger survives a restart and names a caller, which is what accounting needs; the
    // process counters cannot. A ledger failure must not stop `/stats` answering.
    match store.usage_summary(None).await {
        Ok(summary) => payload["usage_today"] = usage_summary_json(&summary),
        Err(error) => {
            payload["usage_today_error"] = Value::String(format!("StoreError: {error}"));
        }
    }

    Json(payload).into_response()
}

/// Stop this process from inside it.
///
/// Nothing in an interactive shell can terminate a session-0 service, but a process can always
/// end itself. This is the last resort for a wedged child; it does *not* stop the Windows
/// service, because NSSM restarts an exited process after five seconds. `stop.bat` is the
/// command that means "stay down".
///
/// The 400 ms delay lets this response leave the process before the hook runs, exactly as the
/// source service's `threading.Timer(0.4, os._exit)` did. A test builds no hook, so calling
/// this endpoint in a test has no side effect.
pub async fn shutdown(
    State(state): State<AppState>,
    connect: Option<Extension<ConnectInfo<SocketAddr>>>,
) -> Response {
    let requested_by = connect
        .map(|Extension(ConnectInfo(address))| address.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    tracing::warn!("shutdown requested via /admin/shutdown by {requested_by}");

    let shared = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        shared.request_shutdown();
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "ok": true,
            "stopping": true,
            "requested_by": requested_by,
            "note": "the supervisor restarts this within ~5s; use stop.bat to stop the service",
        })),
    )
        .into_response()
}

/// The `store.stats()` body, field for field.
pub fn store_stats_json(stats: &StoreStats) -> Value {
    json!({
        "price_rows": stats.price_rows,
        "daily_rows": stats.daily_rows,
        "daily_symbols": stats.daily_symbols,
        "indicator_rows": stats.indicator_rows,
        "latest_session": stats.latest_session.map(iso_date),
        "latest_price_at": stats.latest_price_at.map(iso_naive),
        "daily_by_exchange": stats
            .daily_by_exchange
            .iter()
            .map(exchange_count_json)
            .collect::<Vec<Value>>(),
    })
}

fn exchange_count_json(row: &ExchangeBarCount) -> Value {
    json!({"exchange": row.exchange, "bars": row.bars})
}

/// The `usage_summary()` body, field for field.
pub fn usage_summary_json(summary: &UsageSummary) -> Value {
    json!({
        "date": iso_date(summary.date),
        "calls": summary.calls,
        "symbols": summary.symbols,
        "cost": summary.cost,
        "rows": summary.rows,
        "by_request_type": summary.by_request_type.iter().map(|row| json!({
            "request_type": row.request_type,
            "calls": row.calls,
            "symbols": row.symbols,
            "cost": row.cost,
        })).collect::<Vec<Value>>(),
        "by_caller": summary.by_caller.iter().map(|row| json!({
            "caller": row.caller,
            "calls": row.calls,
            "cost": row.cost,
        })).collect::<Vec<Value>>(),
    })
}

fn iso_date(value: NaiveDate) -> String {
    value.format("%Y-%m-%d").to_string()
}

fn iso_naive(value: chrono::NaiveDateTime) -> String {
    crate::clock::iso_naive(value)
}
