//! The document `/docs` renders and `/openapi.json` serves.
//!
//! The source service let FastAPI generate this from the same Python signatures that served
//! the routes. Rust cannot introspect a handler, so the document is written once, here, and
//! the routes in [`super::router`] reference it by hand. Keeping it static also keeps it
//! honest: a route added without a line here is visible in review as an omission.
//!
//! Version and title match the source service (`__version__ = "0.1.0"`), so a client that
//! pins either sees no change at cutover.

use serde_json::{json, Value};

/// The OpenAPI 3.1 document, as JSON.
///
/// Returned as a plain [`Value`] rather than utoipa's typed model because the contract here is
/// the document, not the Rust types that happen to serve it. `SwaggerUi::external_url_unchecked`
/// accepts exactly this shape.
pub fn document() -> Value {
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "McTrader_Market_Data_Service",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "One price cache, one daily bar cache, one ATR definition, one vendor \
                            key. Four data endpoints for Mini-McTrader, PaperTrader, \
                            PortfolioAdvisor and McTrader.",
        },
        "paths": {
            "/healthz": {
                "get": {
                    "tags": ["ops"],
                    "summary": "Liveness and configuration summary",
                    "operationId": "healthz_healthz_get",
                    "responses": {
                        "200": {"description": "Successful Response", "content": {"application/json": {"schema": {"type": "object"}}}}
                    }
                }
            },
            "/stats": {
                "get": {
                    "tags": ["ops"],
                    "summary": "Row counts, counters and today's vendor spend",
                    "operationId": "stats_stats_get",
                    "responses": {
                        "200": {"description": "Successful Response", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "503": {"description": "No database is configured", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}}
                    }
                }
            },
            "/secrets/eodhd": {
                "get": {
                    "tags": ["secrets"],
                    "summary": "Whether a vendor key is stored, and a masked form",
                    "operationId": "get_eodhd_secret_secrets_eodhd_get",
                    "responses": {
                        "200": {"description": "Successful Response", "content": {"application/json": {"schema": {"type": "object"}}}}
                    }
                },
                "post": {
                    "tags": ["secrets"],
                    "summary": "Store the vendor key in this account's keyring and use it",
                    "operationId": "post_eodhd_secret_secrets_eodhd_post",
                    "parameters": [admin_token_parameter()],
                    "requestBody": {"required": true, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/EodhdKeyRequest"}}}},
                    "responses": {
                        "200": {"description": "Successful Response", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "400": {"description": "The key is blank", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "401": {"description": "The admin token did not match", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "409": {"description": "A key is already configured", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "503": {"description": "The keyring is unavailable", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}}
                    }
                }
            },
            "/admin/shutdown": {
                "post": {
                    "tags": ["ops"],
                    "summary": "Stop this process from inside it; the supervisor restarts it",
                    "operationId": "shutdown_admin_shutdown_post",
                    "responses": {
                        "202": {"description": "Accepted", "content": {"application/json": {"schema": {"type": "object"}}}}
                    }
                }
            },
            "/refresh": {
                "post": {
                    "tags": ["market data"],
                    "summary": "Pull the whole market's daily bars, on demand",
                    "operationId": "post_refresh_refresh_post",
                    "parameters": [admin_token_parameter()],
                    "requestBody": {"required": false, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/RefreshRequest"}}}},
                    "responses": {
                        "200": {"description": "Already fresh; nothing was spent", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "202": {"description": "The pull ran", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "401": {"description": "The admin token did not match", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "409": {"description": "Another refresh holds the lock", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "503": {"description": "No vendor key or no product layer", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}}
                    }
                }
            },
            "/schedule": {
                "get": {
                    "tags": ["market clock"],
                    "summary": "Each market's collection state",
                    "operationId": "get_schedule_schedule_get",
                    "responses": {
                        "200": {"description": "Successful Response", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "503": {"description": "No scheduler is running", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}}
                    }
                }
            },
            "/schedule/run": {
                "post": {
                    "tags": ["market clock"],
                    "summary": "Collect one market now",
                    "operationId": "post_schedule_run_schedule_run_post",
                    "parameters": [
                        admin_token_parameter(),
                        {"name": "market", "in": "query", "required": false, "schema": {"type": "string", "default": "US"}, "description": "US or AU"}
                    ],
                    "responses": {
                        "200": {"description": "The run succeeded", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "401": {"description": "The admin token did not match", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "409": {"description": "The run was refused", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "503": {"description": "No scheduler is running", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}}
                    }
                }
            },
            "/market/clock": {
                "get": {
                    "tags": ["market clock"],
                    "summary": "A broker's clock at a UTC instant",
                    "operationId": "get_market_clock_market_clock_get",
                    "parameters": [
                        {"name": "broker_id", "in": "query", "required": false, "schema": {"type": "integer", "default": 1}},
                        {"name": "time", "in": "query", "required": false, "schema": {"type": ["string", "null"]}, "description": "HH:MM in UTC"},
                        {"name": "at", "in": "query", "required": false, "schema": {"type": ["string", "null"]}, "description": "ISO-8601 instant"}
                    ],
                    "responses": {
                        "200": {"description": "Successful Response", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "400": {"description": "Malformed time or instant", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "404": {"description": "Unknown broker", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}}
                    }
                }
            },
            "/market/session": {
                "get": {
                    "tags": ["market clock"],
                    "summary": "The session a broker's rule resolves at an instant",
                    "operationId": "get_market_session_market_session_get",
                    "parameters": [
                        {"name": "broker_id", "in": "query", "required": false, "schema": {"type": "integer", "default": 1}},
                        {"name": "asset_class", "in": "query", "required": false, "schema": {"type": "string", "default": "shares"}},
                        {"name": "at", "in": "query", "required": false, "schema": {"type": ["string", "null"]}}
                    ],
                    "responses": {
                        "200": {"description": "Successful Response", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "400": {"description": "Malformed instant", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "404": {"description": "Unknown asset class", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}}
                    }
                }
            },
            "/market/exchanges": {
                "get": {
                    "tags": ["market clock"],
                    "summary": "The clock reference: brokers, exchanges and rules",
                    "operationId": "get_market_exchanges_market_exchanges_get",
                    "responses": {
                        "200": {"description": "Successful Response", "content": {"application/json": {"schema": {"type": "object"}}}}
                    }
                }
            },
            "/trading_sessions": {
                "get": {
                    "tags": ["market clock"],
                    "summary": "A broker's clock at an operator-zone time",
                    "operationId": "get_trading_sessions_trading_sessions_get",
                    "parameters": [
                        {"name": "broker_id", "in": "query", "required": false, "schema": {"type": "integer", "default": 1}},
                        {"name": "time", "in": "query", "required": false, "schema": {"type": ["string", "null"]}, "description": "HH:MM in the operator's zone"},
                        {"name": "use_broker_timezone", "in": "query", "required": false, "schema": {"type": "boolean", "default": false}}
                    ],
                    "responses": {
                        "200": {"description": "Successful Response", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "400": {"description": "Malformed time", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "404": {"description": "Unknown broker", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}}
                    }
                }
            },
            "/todays-data-is-available": {
                "get": {
                    "tags": ["market data"],
                    "summary": "Whether a price has been observed since the venue's last open",
                    "operationId": "get_availability_todays_data_is_available_get",
                    "parameters": [country_parameter()],
                    "responses": {
                        "200": {"description": "A bare boolean", "content": {"application/json": {"schema": {"type": "boolean"}}}},
                        "400": {"description": "Unknown country", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "503": {"description": "No database is configured", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}}
                    }
                }
            },
            "/todays-big-movers": {
                "get": {
                    "tags": ["market data"],
                    "summary": "The biggest winners and losers since the venue's last open",
                    "operationId": "get_big_movers_todays_big_movers_get",
                    "parameters": [
                        country_parameter(),
                        {"name": "limit", "in": "query", "required": false, "schema": {"type": "integer", "minimum": 1, "maximum": 200, "default": 25}},
                        {"name": "min_price", "in": "query", "required": false, "schema": {"type": "number", "minimum": 0, "default": 1.0}},
                        {"name": "min_dollar_volume", "in": "query", "required": false, "schema": {"type": "number", "minimum": 0, "default": 0.0}}
                    ],
                    "responses": {
                        "200": {"description": "Ranked movers, or an explicit not-available answer", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "400": {"description": "Unknown country or out-of-range filter", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "503": {"description": "No database or unreadable cache", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}}
                    }
                }
            },
            "/price/{symbol}": {
                "get": {
                    "tags": ["market data"],
                    "summary": "One symbol's quote",
                    "operationId": "get_price_price__symbol__get",
                    "parameters": [
                        {"name": "symbol", "in": "path", "required": true, "schema": {"type": "string"}},
                        {"name": "venue", "in": "query", "required": false, "schema": {"type": ["string", "null"]}},
                        {"name": "country_code", "in": "query", "required": false, "schema": {"type": ["string", "null"]}},
                        {"name": "max_age_seconds", "in": "query", "required": false, "schema": {"type": ["number", "null"]}},
                        {"name": "asset_class", "in": "query", "required": false, "schema": {"type": ["string", "null"]}}
                    ],
                    "responses": {
                        "200": {"description": "Successful Response", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "400": {"description": "Venue or symbol refusal", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "502": {"description": "Every provider refused", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "503": {"description": "No product layer or busy database", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}}
                    }
                }
            },
            "/prices": {
                "post": {
                    "tags": ["market data"],
                    "summary": "Quotes for up to 200 symbols",
                    "operationId": "post_prices_prices_post",
                    "requestBody": {"required": true, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/PricesRequest"}}}},
                    "responses": {
                        "200": {"description": "Successful Response", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "422": {"description": "Validation Error", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/HTTPValidationError"}}}}
                    }
                }
            },
            "/atr": {
                "post": {
                    "tags": ["market data"],
                    "summary": "ATR and optional stop levels",
                    "operationId": "post_atr_atr_post",
                    "requestBody": {"required": true, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/AtrRequest"}}}},
                    "responses": {
                        "200": {"description": "Successful Response", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "400": {"description": "A price without a multiplier, or insufficient history", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "422": {"description": "Validation Error", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/HTTPValidationError"}}}}
                    }
                }
            },
            "/bars": {
                "post": {
                    "tags": ["market data"],
                    "summary": "Completed daily bars, cached or fetched",
                    "operationId": "post_bars_bars_post",
                    "requestBody": {"required": true, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/BarsRequest"}}}},
                    "responses": {
                        "200": {"description": "Successful Response", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "400": {"description": "Venue or symbol refusal", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "422": {"description": "Validation Error", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/HTTPValidationError"}}}}
                    }
                }
            }
        },
        "components": {
            "schemas": {
                "Error": {
                    "type": "object",
                    "properties": {
                        "error": {"type": "string"},
                        "detail": {},
                        "retryable": {"type": "boolean"}
                    },
                    "required": ["error"]
                },
                "ValidationError": {
                    "type": "object",
                    "properties": {
                        "loc": {"type": "array", "items": {"anyOf": [{"type": "string"}, {"type": "integer"}]}},
                        "msg": {"type": "string"},
                        "type": {"type": "string"},
                        "input": {}
                    },
                    "required": ["loc", "msg", "type"]
                },
                "HTTPValidationError": {
                    "type": "object",
                    "properties": {"detail": {"type": "array", "items": {"$ref": "#/components/schemas/ValidationError"}}}
                },
                "PricesRequest": {
                    "type": "object",
                    "properties": {
                        "symbols": {"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "Bare or suffixed symbols"},
                        "venue": {"type": ["string", "null"]},
                        "country_code": {"type": ["string", "null"]},
                        "max_age_seconds": {"type": ["number", "null"]},
                        "asset_class": {"type": ["string", "null"]}
                    },
                    "required": ["symbols"]
                },
                "AtrRequest": {
                    "type": "object",
                    "properties": {
                        "symbols": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                        "period": {"type": ["integer", "null"], "description": "Default 14"},
                        "method": {"type": ["string", "null"], "description": "gap (default) or high_low"},
                        "smoothing": {"type": ["string", "null"], "description": "simple (default) or wilder"},
                        "venue": {"type": ["string", "null"]},
                        "country_code": {"type": ["string", "null"]},
                        "price": {"type": ["number", "null"]},
                        "multiplier": {"type": ["number", "null"]},
                        "direction": {"type": ["string", "null"], "description": "'long' (default) or 'short'"},
                        "asset_class": {"type": ["string", "null"]}
                    },
                    "required": ["symbols"]
                },
                "BarsRequest": {
                    "type": "object",
                    "properties": {
                        "symbols": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                        "sessions": {"type": "integer", "minimum": 1, "default": 15, "description": "Completed sessions to return"},
                        "venue": {"type": ["string", "null"]},
                        "country_code": {"type": ["string", "null"]},
                        "allow_fetch": {"type": "boolean", "default": true},
                        "asset_class": {"type": ["string", "null"]}
                    },
                    "required": ["symbols"]
                },
                "EodhdKeyRequest": {
                    "type": "object",
                    "properties": {"api_key": {"type": "string", "minLength": 1, "description": "The EODHD API key"}},
                    "required": ["api_key"]
                },
                "RefreshRequest": {
                    "type": "object",
                    "properties": {
                        "market": {"type": "string", "default": "US", "description": "Market to pull; only US is measured"},
                        "session_date": {"type": ["string", "null"], "description": "YYYY-MM-DD; default is the latest completed session"},
                        "force": {"type": "boolean", "default": false, "description": "Pull again even if one is already recorded today"},
                        "include_indicators": {"type": "boolean", "default": true}
                    }
                }
            }
        },
        "tags": [
            {"name": "ops"},
            {"name": "secrets"},
            {"name": "market data"},
            {"name": "market clock"}
        ]
    })
}

fn country_parameter() -> Value {
    json!({"name": "country_code", "in": "query", "required": false,
           "schema": {"type": "string", "default": "US"}, "description": "us/US or au/AU"})
}

fn admin_token_parameter() -> Value {
    json!({"name": "X-Admin-Token", "in": "header", "required": false,
           "schema": {"type": "string"},
           "description": "Required when MDS_ADMIN_TOKEN is configured"})
}
