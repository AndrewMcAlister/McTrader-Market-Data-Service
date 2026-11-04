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
                    "requestBody": {"required": true, "content": {"application/json": {
                        "schema": {"$ref": "#/components/schemas/EodhdKeyRequest"},
                        "examples": {
                            "store_a_key": {"summary": "Store the vendor key in this account's keyring", "value": {"api_key": "your-EODHD-api-key"}}
                        }
                    }}},
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
                    "requestBody": {"required": false, "content": {"application/json": {
                        "schema": {"$ref": "#/components/schemas/RefreshRequest"},
                        "examples": {
                            "today_us": {"summary": "Pull today's US session with every default", "value": {}},
                            "asx_session": {"summary": "Pull a named ASX session", "value": {"market": "AU", "session_date": "2026-11-03"}},
                            "forced_backfill": {"summary": "Pull again over today's ledger entry, without rebuilding indicators", "value": {"market": "US", "session_date": "2026-11-03", "force": true, "include_indicators": false}}
                        }
                    }}},
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
                        {"name": "market", "in": "query", "required": false,
                         "schema": {"type": "string", "enum": ["US", "AU"], "default": "US", "example": "US"},
                         "description": "Which market to collect now: US or AU."}
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
                        {"name": "broker_id", "in": "query", "required": false,
                         "schema": {"type": "integer", "default": 1, "example": 1},
                         "description": "The broker whose rule is resolved."},
                        {"name": "at", "in": "query", "required": false,
                         "schema": {"type": ["string", "null"], "example": "2026-11-04T14:30:00Z"},
                         "description": "ISO-8601 instant; defaults to now. The shares rule is always the one resolved."}
                    ],
                    "responses": {
                        "200": {"description": "Successful Response", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "400": {"description": "Malformed instant", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}}
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
                    "summary": "Whether a price has been observed since the exchange's last open",
                    "operationId": "get_availability_todays_data_is_available_get",
                    "parameters": [country_parameter()],
                    "responses": {
                        "200": {"description": "A bare boolean", "content": {"application/json": {"schema": {"type": "boolean"}}}},
                        "422": {"description": "Validation Error", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/HTTPValidationError"}}}},
                        "503": {"description": "No database is configured", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}}
                    }
                }
            },
            "/todays-big-movers": {
                "get": {
                    "tags": ["market data"],
                    "summary": "The biggest winners and losers since the exchange's last open",
                    "operationId": "get_big_movers_todays_big_movers_get",
                    "parameters": [
                        country_parameter(),
                        {"name": "limit", "in": "query", "required": false, "schema": {"type": "integer", "minimum": 1, "maximum": 200, "default": 25}},
                        {"name": "min_price", "in": "query", "required": false, "schema": {"type": "number", "minimum": 0, "default": 1.0}},
                        {"name": "min_dollar_volume", "in": "query", "required": false, "schema": {"type": "number", "minimum": 0, "default": 0.0}}
                    ],
                    "responses": {
                        "200": {"description": "Ranked movers, or an explicit not-available answer", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "422": {"description": "Validation Error", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/HTTPValidationError"}}}},
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
                        {"name": "symbol", "in": "path", "required": true,
                         "schema": {"type": "string", "example": "AAPL"},
                         "description": "Bare ticker or one with a market suffix, such as CBA.AU."},
                        {"name": "country_code", "in": "query", "required": true, "schema": country_schema()},
                        {"name": "exchange", "in": "query", "required": false, "schema": exchange_schema()},
                        {"name": "max_age_seconds", "in": "query", "required": false,
                         "schema": {"type": ["number", "null"], "minimum": 0, "example": 60},
                         "description": "Maximum cache age in seconds. The default is 300; a caller may only ask for something newer, never staler."}
                    ],
                    "responses": {
                        "200": {"description": "Successful Response", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "400": {"description": "Exchange or symbol refusal", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "422": {"description": "Validation Error", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/HTTPValidationError"}}}},
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
                    "requestBody": {"required": true, "content": {"application/json": {
                        "schema": {"$ref": "#/components/schemas/PricesRequest"},
                        "examples": {
                            "us_batch": {"summary": "Three US tickers, cache may be at most 60 seconds old", "value": {"symbols": ["AAPL", "MSFT", "NVDA"], "country_code": "US", "max_age_seconds": 60}},
                            "asx_batch": {"summary": "Two ASX tickers with the exchange spelled out", "value": {"symbols": ["CBA", "BHP"], "country_code": "AU", "exchange": "ASX"}}
                        }
                    }}},
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
                    "requestBody": {"required": true, "content": {"application/json": {
                        "schema": {"$ref": "#/components/schemas/AtrRequest"},
                        "examples": {
                            "default_definition": {"summary": "ATR only, using period 14, method tr and smoothing simple", "value": {"symbols": ["AAPL", "MSFT"], "country_code": "US"}},
                            "stop_levels": {"summary": "ATR and a long stop two ATRs below the given price", "value": {"symbols": ["AAPL"], "country_code": "US", "period": 14, "method": "tr", "smoothing": "simple", "price": 231.4, "multiplier": 2.0, "direction": "long"}},
                            "wilder_high_low": {"summary": "Every non-default option spelled out", "value": {"symbols": ["CBA"], "country_code": "AU", "exchange": "ASX", "period": 21, "method": "high_low", "smoothing": "wilder", "price": 118.2, "multiplier": 3.0, "direction": "short"}}
                        }
                    }}},
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
                    "requestBody": {"required": true, "content": {"application/json": {
                        "schema": {"$ref": "#/components/schemas/BarsRequest"},
                        "examples": {
                            "cache_only": {"summary": "Fifteen completed sessions from the cache, never calling a vendor", "value": {"symbols": ["AAPL"], "country_code": "US", "sessions": 15, "allow_fetch": false}},
                            "asx_history": {"summary": "A year of ASX history, fetching when the cache is short", "value": {"symbols": ["CBA.AU"], "country_code": "AU", "exchange": "ASX", "sessions": 250}}
                        }
                    }}},
                    "responses": {
                        "200": {"description": "Successful Response", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "400": {"description": "Exchange or symbol refusal", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
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
                        "symbols": {"type": "array", "items": {"type": "string"}, "minItems": 1,
                                    "example": ["AAPL", "MSFT", "NVDA"],
                                    "description": "One or more bare or suffixed symbols. One request is one market, so every symbol must belong to country_code."},
                        "country_code": country_schema(),
                        "exchange": exchange_schema(),
                        "max_age_seconds": {"type": ["number", "null"], "minimum": 0, "example": 60,
                                            "description": "Maximum cache age in seconds. The default is 300; a caller may only ask for something newer, never staler."}
                    },
                    "required": ["symbols", "country_code"]
                },
                "AtrRequest": {
                    "type": "object",
                    "properties": {
                        "symbols": {"type": "array", "items": {"type": "string"}, "minItems": 1,
                                    "example": ["AAPL", "MSFT"],
                                    "description": "One or more bare or suffixed symbols, all in country_code."},
                        "period": {"anyOf": [{"type": "integer", "minimum": 1}, {"type": "null"}],
                                   "default": 14, "example": 14,
                                   "description": "Sessions in the average. Default 14."},
                        "method": {"anyOf": [{"type": "string", "enum": ["tr", "high_low"]}, {"type": "null"}],
                                   "default": "tr", "example": "tr",
                                   "description": "tr (default, gap-inclusive true range) or high_low (high minus low only)."},
                        "smoothing": {"anyOf": [{"type": "string", "enum": ["simple", "wilder"]}, {"type": "null"}],
                                      "default": "simple", "example": "simple",
                                      "description": "simple (default, the N-bar mean) or wilder (Wilder's recursion)."},
                        "country_code": country_schema(),
                        "exchange": exchange_schema(),
                        "price": {"type": ["number", "null"], "example": 231.4,
                                  "description": "Reference price for a stop level. Supplying it without multiplier is a 400 incomplete-stop."},
                        "multiplier": {"type": ["number", "null"], "example": 2.0,
                                       "description": "The ATR multiple for the stop level. Never defaulted, because the books use 1.0, 2.0 and 3.0."},
                        "direction": {"anyOf": [{"type": "string", "enum": ["long", "short"]}, {"type": "null"}],
                                      "default": "long", "example": "long",
                                      "description": "Which side of price the stop sits on."}
                    },
                    "required": ["symbols", "country_code"]
                },
                "BarsRequest": {
                    "type": "object",
                    "properties": {
                        "symbols": {"type": "array", "items": {"type": "string"}, "minItems": 1,
                                    "example": ["AAPL", "MSFT"],
                                    "description": "One or more bare or suffixed symbols, all in country_code."},
                        "sessions": {"type": "integer", "minimum": 1, "default": 15, "example": 15,
                                     "description": "Completed sessions to return. The session still in progress is always excluded."},
                        "country_code": country_schema(),
                        "exchange": exchange_schema(),
                        "allow_fetch": {"type": "boolean", "default": true, "example": false,
                                        "description": "false answers only from the cache and never calls a vendor."}
                    },
                    "required": ["symbols", "country_code"]
                },
                "EodhdKeyRequest": {
                    "type": "object",
                    "properties": {"api_key": {"type": "string", "minLength": 1, "example": "your-EODHD-api-key",
                                               "description": "The EODHD API key. Never echoed back."}},
                    "required": ["api_key"]
                },
                "RefreshRequest": {
                    "type": "object",
                    "properties": {
                        "market": {"type": "string", "enum": ["US", "AU"], "default": "US", "example": "US",
                                   "description": "Which market's daily bars to pull."},
                        "session_date": {"type": ["string", "null"], "format": "date", "example": "2026-11-03",
                                         "description": "YYYY-MM-DD. Default is the latest completed session."},
                        "force": {"type": "boolean", "default": false, "example": false,
                                  "description": "Pull again even when one is already recorded today. This spends the bulk tariff again."},
                        "include_indicators": {"type": "boolean", "default": true, "example": true,
                                               "description": "Rebuild IndicatorCache for the pulled sessions."}
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

fn country_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["US", "AU"],
        "example": "US",
        "description": "The country the tickers belong to. Required: a ticker alone does not name a market, because CBA is both an ASX and a US listing."
    })
}

fn exchange_schema() -> Value {
    json!({
        "anyOf": [
            {"type": "string", "enum": [
                "US", "NYSE", "NASDAQ", "AMEX", "NYSEARCA",
                "AU", "ASX", "AX", "AUS"
            ]},
            {"type": "null"}
        ],
        "example": "ASX",
        "description": "Optional second spelling of the country. US: US, NYSE, NASDAQ, AMEX, NYSEARCA. AU: AU, ASX, AX, AUS. Must agree with country_code; a disagreement is a 400 exchange-conflict."
    })
}

fn country_parameter() -> Value {
    json!({"name": "country_code", "in": "query", "required": true,
           "schema": country_schema(),
           "description": "Country code: us/US or au/AU; required"})
}

fn admin_token_parameter() -> Value {
    json!({"name": "X-Admin-Token", "in": "header", "required": false,
           "schema": {"type": "string"},
           "description": "Required when MDS_ADMIN_TOKEN is configured"})
}
