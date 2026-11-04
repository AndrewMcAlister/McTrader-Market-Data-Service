//! The route table.
//!
//! One line per source endpoint, in the source service's order, so a reviewer can compare the
//! two files side by side. Paths and methods are the contract: a book that called `/price/X`
//! yesterday must call it today.
//!
//! Two framework-shaped answers are provided here because axum's defaults are not FastAPI's:
//! an unknown path returns `{"detail": "Not Found"}` and a known path with the wrong method
//! returns `{"detail": "Method Not Allowed"}`, both with the source statuses.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tower_http::trace::TraceLayer;
use utoipa_swagger_ui::SwaggerUi;

use super::state::AppState;
use super::{data, market, openapi, ops, refresh, secrets, todays};

/// Build the complete HTTP surface around one shared state.
pub fn build_router(state: AppState) -> Router {
    let routes = Router::new()
        // -- infrastructure -------------------------------------------------
        .route("/healthz", get(ops::healthz))
        .route("/stats", get(ops::stats))
        // -- the vendor key --------------------------------------------------
        .route(
            "/secrets/eodhd",
            post(secrets::post_secret).get(secrets::get_secret),
        )
        // -- stopping --------------------------------------------------------
        .route("/admin/shutdown", post(ops::shutdown))
        // -- bulk refresh ----------------------------------------------------
        .route("/refresh", post(refresh::post_refresh))
        // -- the clock and the schedule --------------------------------------
        .route("/schedule", get(market::get_schedule))
        .route("/schedule/run", post(market::post_schedule_run))
        .route("/market/clock", get(market::get_market_clock))
        .route("/market/session", get(market::get_market_session))
        .route("/market/exchanges", get(market::get_market_exchanges))
        .route("/trading_sessions", get(market::get_trading_sessions))
        // -- today -----------------------------------------------------------
        .route("/todays-data-is-available", get(todays::get_availability))
        .route("/todays-big-movers", get(todays::get_big_movers))
        // -- market data -----------------------------------------------------
        .route("/price/{symbol}", get(data::get_price))
        .route("/prices", post(data::post_prices))
        .route("/atr", post(data::post_atr))
        .route("/bars", post(data::post_bars))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    // `/docs` and `/openapi.json` live outside the state-carrying router; merging after
    // `with_state` means they cannot accidentally depend on application state.
    let docs = Router::<()>::from(
        SwaggerUi::new("/docs").external_url_unchecked("/openapi.json", openapi::document()),
    );
    routes.merge(docs)
}

async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"detail": "Not Found"}))).into_response()
}

async fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(json!({"detail": "Method Not Allowed"})),
    )
        .into_response()
}
