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
                        {"name": "asset_class", "in": "query", "required": false, "schema": asset_class_schema(),
                         "description": "Optional. Absent means shares. An index, commodity ETF proxy or rate must state its class or it is priced as a share."},
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
                            "asx_history": {"summary": "A year of ASX history, fetching when the cache is short", "value": {"symbols": ["CBA.AU"], "country_code": "AU", "exchange": "ASX", "sessions": 250}},
                            "an_index": {"summary": "A rate, stated as one: the same symbol priced as a share is a different instrument", "value": {"symbols": ["US10Y"], "country_code": "US", "asset_class": "rates", "sessions": 60}}
                        }
                    }}},
                    "responses": {
                        "200": {"description": "Successful Response", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "400": {"description": "Exchange or symbol refusal", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "422": {"description": "Validation Error", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/HTTPValidationError"}}}}
                    }
                }
            },
            "/bars/intraday": {
                "post": {
                    "tags": ["market data"],
                    "summary": "5-minute bars and their 15m/30m/1h/4h aggregates",
                    "operationId": "post_intraday_bars_intraday_post",
                    "description": "Every timeframe is derived from the same native 5-minute tape, anchored to the exchange's local session: a 4-hour bucket resets at each session's first bar and never straddles the overnight gap. `d` is an exchange-local ISO timestamp, not a session date. A cache hit inside MDS_INTRADAY_TTL_SECONDS (300 s) costs no vendor units; a fetch bills 5 per symbol and is named in `fetched`.",
                    "requestBody": {"required": true, "content": {"application/json": {
                        "schema": {"$ref": "#/components/schemas/IntradayRequest"},
                        "examples": {
                            "one_session_of_5m": {"summary": "One US session of 5-minute bars, fetching if the tape is short", "value": {"symbols": ["TTD"], "country_code": "US", "timeframe": "5Min", "limit": 78}},
                            "four_hour_aggregate": {"summary": "Four-hour bars aggregated from the same 5m tape", "value": {"symbols": ["TTD"], "country_code": "US", "timeframe": "4Hour", "limit": 48}},
                            "cache_only": {"summary": "Never call the vendor: the owned tape answers or the symbol is reported short", "value": {"symbols": ["AAPL"], "country_code": "US", "timeframe": "1Hour", "limit": 24, "allow_fetch": false}}
                        }
                    }}},
                    "responses": {
                        "200": {"description": "Bars per symbol, plus `fetched` (the spend evidence) and `short_history`", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "400": {"description": "Exchange or symbol refusal", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "422": {"description": "Validation Error", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/HTTPValidationError"}}}},
                        "502": {"description": "The vendor failed", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "503": {"description": "No vendor client or no database", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}}
                    }
                }
            },
            "/bars/bulk": {
                "post": {
                    "tags": ["market data"],
                    "summary": "Whole-market closes for one to ten sessions",
                    "operationId": "post_bulk_bars_bulk_post",
                    "description": "`c` is the split-adjusted close. A session is served from the cache only when a completed pull is recorded for it: a partial session silently changes every derived return that reads it. A pull that is materially short of the previous completed session is returned with its key in `partial` and writes no completeness marker, so a later cache read re-asks the vendor.",
                    "requestBody": {"required": true, "content": {"application/json": {
                        "schema": {"$ref": "#/components/schemas/BulkRequest"},
                        "examples": {
                            "latest_and_a_past_session": {"summary": "The latest completed session and one dated pull", "value": {"market": "US", "dates": ["latest", "2026-08-13"]}},
                            "cache_only": {"summary": "Refuse rather than return a session no completed pull is recorded for", "value": {"market": "US", "dates": ["2026-11-07"], "allow_fetch": false}}
                        }
                    }}},
                    "responses": {
                        "200": {"description": "One entry per requested date key, keyed as the caller spelled it", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "404": {"description": "The session is not pulled and fetching is off (`not-pulled`)", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "422": {"description": "Validation Error", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/HTTPValidationError"}}}},
                        "502": {"description": "The vendor failed", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "503": {"description": "No vendor client or no database", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}}
                    }
                }
            },
            "/screener": {
                "post": {
                    "tags": ["market data"],
                    "summary": "Raw vendor screener rows for one market",
                    "operationId": "post_screener_screener_post",
                    "description": "Transport only: the caller owns the floors, ranking and limits. The venue travels inside `filters` — exactly one `[\"exchange\", \"=\", venue]` triple is appended last, using the caller's spelling rather than the resolved market, so a per-venue NASDAQ screen is not collapsed into the US umbrella. Rows are the vendor's objects, untouched; every filter is validated against the vendor's 14 fields before a unit is spent.",
                    "requestBody": {"required": true, "content": {"application/json": {
                        "schema": {"$ref": "#/components/schemas/ScreenerRequest"},
                        "examples": {
                            "us_liquid_movers": {"summary": "Liquid US names up 5% over five days, ranked by that return", "value": {"country_code": "US", "filters": [["market_capitalization", ">", 300000000], ["refund_5d_p", ">", 5]], "sort": "refund_5d_p.desc", "limit": 50}},
                            "one_venue": {"summary": "One US venue, which is what the vendor's exchange filter must name", "value": {"country_code": "US", "exchange": "NASDAQ", "filters": [["avgvol_200d", ">", 500000]], "limit": 100}}
                        }
                    }}},
                    "responses": {
                        "200": {"description": "The vendor's rows and the query that produced them", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "400": {"description": "The exchange filter and the country name different markets", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "422": {"description": "A bad field, operator, sort or bound, or the vendor rejected the query", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "502": {"description": "The vendor failed", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "503": {"description": "No vendor client", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}}
                    }
                }
            },
            "/profile": {
                "post": {
                    "tags": ["market data"],
                    "summary": "Company profile for up to 25 listings",
                    "operationId": "post_profile_profile_post",
                    "description": "One pinned vendor call per uncached symbol (5 units each, one hour memo). The profile fields are the ones a screener row carries: there is no entitled source for `description`, GICS names or country, so those keys are always null rather than absent. A symbol with no matching listing is `found: false` and named in `missing`, never an error for the batch.",
                    "requestBody": {"required": true, "content": {"application/json": {
                        "schema": {"$ref": "#/components/schemas/ProfilesRequest"},
                        "examples": {
                            "asx_names": {"summary": "Three ASX listings", "value": {"symbols": ["WDS", "BHP", "CBA"], "country_code": "AU", "exchange": "ASX"}},
                            "us_batch": {"summary": "Fifteen US tickers, within the 25-symbol cap", "value": {"symbols": ["AAPL", "MSFT", "NVDA"], "country_code": "US"}}
                        }
                    }}},
                    "responses": {
                        "200": {"description": "Per-symbol profiles, each with `found` and its own `error`", "content": {"application/json": {"schema": {"type": "object"}}}},
                        "400": {"description": "Exchange or symbol refusal", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                        "422": {"description": "Validation Error", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/HTTPValidationError"}}}},
                        "503": {"description": "No vendor client", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}}
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
                        "asset_class": asset_class_schema(),
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
                        "asset_class": asset_class_schema(),
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
                        "asset_class": asset_class_schema(),
                        "allow_fetch": {"type": "boolean", "default": true, "example": false,
                                        "description": "false answers only from the cache and never calls a vendor."}
                    },
                    "required": ["symbols", "country_code"]
                },
                "ScreenerRequest": {
                    "type": "object",
                    "properties": {
                        "country_code": country_schema(),
                        "exchange": exchange_schema(),
                        "filters": {"type": "array", "maxItems": 64,
                                    "items": {"type": "array", "minItems": 3, "maxItems": 3,
                                              "items": {}},
                                    "example": [["market_capitalization", ">", 300000000], ["refund_5d_p", ">", 5]],
                                    "description": "Triples of [field, operator, value]. The 14 fields are code, name, exchange, sub_exchange, sector, industry, market_capitalization, earnings_share, dividend_yield, adjusted_close, avgvol_1d, avgvol_200d, refund_1d_p, refund_5d_p. Numeric fields take =, !=, >, <, >=, <=; text fields take = and in only. An `exchange` entry is lifted into the single triple the vendor needs. Derived fields (return_3m_p, return_100d_p, return_200d_p) are refused: read whole-market closes from POST /bars/bulk and compute them there."},
                        "sort": {"anyOf": [{"type": "string"}, {"type": "null"}], "example": "refund_5d_p.desc",
                                 "description": "<field>.<asc|desc>. The direction is required because the vendor's default is not documented. A derived field cannot be sorted on."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 100, "example": 50,
                                  "description": "Rows to return. 500 is the verified vendor cap."},
                        "offset": {"type": "integer", "minimum": 0, "default": 0, "example": 0,
                                   "description": "Rows to skip."}
                    },
                    "required": ["country_code"]
                },
                "ProfilesRequest": {
                    "type": "object",
                    "properties": {
                        "symbols": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 25,
                                    "example": ["WDS", "BHP", "CBA"],
                                    "description": "One to 25 bare or suffixed symbols, all in country_code. 25 is the batch cost bound: five units per uncached symbol."},
                        "country_code": country_schema(),
                        "exchange": exchange_schema()
                    },
                    "required": ["symbols", "country_code"]
                },
                "IntradayRequest": {
                    "type": "object",
                    "properties": {
                        "symbols": {"type": "array", "items": {"type": "string"}, "minItems": 1,
                                    "example": ["TTD"],
                                    "description": "One or more bare or suffixed symbols, all in country_code."},
                        "timeframe": {"type": "string",
                                      "enum": ["5Min", "15Min", "30Min", "1Hour", "4Hour",
                                               "5m", "15m", "30m", "1h", "4h"],
                                      "example": "4Hour",
                                      "description": "5Min, 15Min, 30Min, 1Hour or 4Hour, case-insensitively. Every frame is derived from the same native 5-minute tape, anchored to the exchange's local session."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 78, "example": 48,
                                  "description": "Buckets to return, newest first after the session anchor. The default is one US session of 5-minute bars."},
                        "allow_fetch": {"type": "boolean", "default": true, "example": true,
                                        "description": "false serves the owned 5-minute tape only and never calls a vendor."},
                        "country_code": country_schema(),
                        "exchange": exchange_schema()
                    },
                    "required": ["symbols", "timeframe", "country_code"]
                },
                "BulkRequest": {
                    "type": "object",
                    "properties": {
                        "market": {"type": "string", "enum": ["US", "AU"], "default": "US", "example": "US",
                                   "description": "Which market's whole session to read or pull."},
                        "dates": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 10,
                                  "example": ["latest", "2026-08-13"],
                                  "description": "One to ten distinct entries, each \"latest\" or YYYY-MM-DD. \"latest\" resolves through the same completed-session rule POST /bars uses; duplicates collapse."},
                        "allow_fetch": {"type": "boolean", "default": true, "example": true,
                                        "description": "false returns 404 not-pulled rather than returning a session no completed pull is recorded for."}
                    },
                    "required": ["dates"]
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
                "US", "NYSE", "NASDAQ", "AMEX", "NYSEARCA", "ARCA", "BATS", "IEX",
                "AU", "ASX", "AX", "AUS"
            ]},
            {"type": "null"}
        ],
        "example": "ASX",
        "description": "Optional second spelling of the country. US: US, NYSE, NASDAQ, AMEX, NYSEARCA, ARCA, BATS, IEX. AU: AU, ASX, AX, AUS. Must agree with country_code; a disagreement is a 400 exchange-conflict. On POST /screener the caller's spelling is what travels to the vendor, so a per-venue screen stays per-venue."
    })
}

fn asset_class_schema() -> Value {
    json!({
        "anyOf": [
            {"type": "string", "enum": [
                "shares", "equities", "equity",
                "crypto", "cryptocurrency",
                "indices", "indicies", "index",
                "commodities", "commodity", "futures", "future",
                "rates", "rate", "bonds",
                "forex", "fx", "currency"
            ]},
            {"type": "null"}
        ],
        "example": "indices",
        "description": "Optional. Absent means shares, which is what every existing caller means. Stating it matters: an index, commodity ETF proxy or rate priced as a share is a different instrument, and the ones whose rendering differs come back unpriced. An unrecognised spelling is a 422 unknown-asset-class rather than a silent share."
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
