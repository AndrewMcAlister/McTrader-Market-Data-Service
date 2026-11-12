# mMds — the EODHD egress endpoints

Version 2 — 2026-11-12. **Planning only: this document changes no code.**

**Companion plan:** `D:\McTrader\TODO\eodhd-egress-retirement-plan.md` (the McTrader half).

**Relationship to the umbrella plan:** `McTrader\TODO\Implement_Single_Market_Data_Egress_Plan.md`
covers eight vendor capabilities across three vendors. This plan takes over its **EODHD half** —
`POST /bars/intraday`, `POST /bars/bulk`, `POST /screener`, `POST /profile` and the optional
`asset_class` field — plus slices S1–S3, S5, S6 and the EODHD deletions in S9 of that document
(S6 is the profile/fundamentals slice; McTrader's M5 keeps its Yahoo fallback, so the umbrella keeps
the Yahoo-leg deletion). The umbrella
plan keeps only its Yahoo/Binance remainder: `GET /symbol`, `GET /fx`, `GET /quote`, slices S4
(venue leg), S7, S8, and S9's non-EODHD deletions. Where the two disagree, **this document is
authoritative for the EODHD endpoints**; the McTrader plan says which umbrella slices are trimmed.

The rule being served is the owner ruling of 2026-11-10: *all market data other than what the
broker supplies comes from mMds.* Only mMds may hold an EODHD key, reach an EODHD host or speak
the hosted EODHD MCP protocol.

---

## 0. Two scope amendments this plan asks the owner to ratify

Both amendments are narrow, deliberate and recorded in the docs named. Neither widens what the
service *decides*; both widen only what it *transports*.

### 0.1 The README says screener and profiles are out of scope

`README.md:1183` states:

> **Out:** screener, company profiles, fundamentals, news, streaming, order-book depth, tick history,
> 5-minute bar construction, and portfolio or risk logic.

and `README.md:1189-1192` explains why: *"If it grows a screener, profiles, crypto and a scoring
layer, it becomes a second trading engine and the reason to have it disappears."*

**Amendment:** the boundary that matters is **policy**, not payload shape. mMds may transport a
screener query, a profile lookup and an intraday fetch; it must not own the liquidity floors, the
ranking, the venue post-filter or any scoring. Those stay in `market_data/eodhd_screener.py` today
(renamed `market_data/equity_screener.py` by McTrader's slice M4) and in
`services/market_data_service.py`, where their tests live.

| mMds owns | McTrader owns |
|---|---|
| the vendor call, retry, rate-limit handling, timeout, billing and ledger | which filters, floors, sort and limit to send |
| the transport contract: 14 screener fields, operators, sort syntax, limit bounds, one-venue agreement | resolving a broker venue code to an EODHD venue value |
| rendering and parsing vendor rows | deciding which rows are candidates, rejections or duplicates |
| the owned caches and their TTLs | when a screen runs and what it means |

No scoring, no ranking, no threshold that decides whether a company is investable. A future reader
who wants to add one is adding it in the wrong process.

**Scope limit stated up front.** The service's market model is US and AU: `country_code` is
required and `exchange` is an optional second spelling resolved by `resolve_exchange`
(`src/domain/exchanges.rs:143-245`). `POST /screener` therefore serves the US and AU markets only.
McTrader's venue vocabulary also names `LSE`, `TO`, `F`, `PA`, `V` and `NEO`
(`market_data/eodhd_field_reference.py`, `KNOWN_EXCHANGE_VENUES`); no live watchlist, broker row or
stored screener query in this estate names one (probe P7). A venue outside US/AU resolves through
`exchange_for` (`LSE` → `L`, `src/domain/exchanges.rs:69-74`) and, because `country_code` must be US
or AU, is refused as `400 exchange-conflict`; a code in neither table (`NEO`) is `400
unknown-exchange`. The US/AU limit is therefore an explicit check that the resolved market is in
`COUNTRIES`, not a refusal `resolve_exchange` already performs. Widening the
service to a third country is a separate change that adds it to `COUNTRIES`, the warehouses and the
schedules.

### 0.2 INTERFACE.md says there is no asset-class request field

`INTERFACE.md:50-53` and `README.md:104-106` state:

> **There is no asset-class request field.** The vendor taxonomy behind it was not reliable, so
> every symbol is treated as a share… Crypto, indices, commodities, rates and forex are not
> selectable over this API.

That restriction arrived in commit `0adcb5c` (2026-11-04, *"docs(interface): remove asset-class
field from API docs"*) and it is the **cause** of a live defect: McTrader's market picture sends
`asset_class` on `/price`, `/prices`, `/bars` and `/atr` today
(`market_data/data_service_client.py:355-405, 614-736`), mMds's library layer accepts and parses it
(`src/api/layer.rs:30-51` already takes `asset_class: Option<&str>`; `src/products.rs:615,657,898`;
`src/domain/asset_class.rs`), but `src/api/data.rs` drops it (`:44-46`, `:81-83`, `:123-124`,
`:163-164`). The
seven market-picture instruments that are indices, commodity ETF proxies and a rate are therefore
priced as shares, and the ones whose rendering differs come back `unpriced` — the degraded snapshot
`services/market_picture_service.py` logs names the EODHD `.INDX` trap.

**Amendment:** add an **optional** `asset_class` field to `GET /price/{symbol}`, `POST /prices`,
`POST /atr` and `POST /bars`:

- absent ⇒ `shares` (every existing caller is unchanged);
- present and recognised ⇒ that class, forwarded to `Products`;
- present and **not** recognised ⇒ `422` `unknown-asset-class`. The library's
  `AssetClass::parse` keeps its infallible share fallback for internal callers; the HTTP boundary
  uses a new **strict** parser so a typo is refused rather than silently priced as a share. That
  strictness is the answer to the "unreliable taxonomy" objection: an explicit, validated field is
  harder to get wrong than the silent fold that replaced it.

The seven McTrader instruments that need this are `IXIC`/`GSPC` (`indices`), `GLD`/`SLV`/`BNO`/`UUP`
(`commodities`) and `US10Y` (`rates`) — see `services/market_picture_service.py`
`MARKET_PICTURE_INSTRUMENTS`. All are US-market requests.

---

## 1. Current state

mMds serves, on `http://127.0.0.1:8003`:

`GET /price/{symbol}`, `POST /prices`, `POST /atr`, `POST /bars` (completed **daily** sessions
only), `GET /todays-data-is-available`, `GET /todays-big-movers`, `GET /market/clock`,
`GET /market/session`, `GET /market/exchanges`, `GET /trading_sessions`, `GET /schedule`,
`POST /schedule/run`, `POST /refresh`, `GET /stats`, `GET /healthz`, `POST /admin/shutdown`,
`POST /secrets/eodhd`, plus the hand-written OpenAPI document on `/docs` and `/openapi.json`.

It owns `SymbolPriceCache`, `DailySnapshot`, `IndicatorCache` (`sql/001_schema.sql`) and reads the
estate tables `dbo.BarHourly`, `dbo.Bar5Min`/`BarReal5Min` read-only through
`src/store/warehouse.rs`. It has an EODHD client that can already make five vendor calls:
`real_time`, `real_time_whole_market`, `eod`, `intraday` (5 units flat, 600-day cap) and
`bulk_last_day` (100 units flat, optional session date) — `src/providers/eodhd_client.rs:312-486`;
the trait those tests script is `EodhdApi` at `src/providers/eodhd_client.rs:518-550`, "five vendor
calls" in its doc comment.

McTrader still reaches EODHD through **four transports** that this plan exists to retire:

| # | Transport | Where | Calls |
|---|---|---|---|
| T1 | EODHD REST client | `market_data/eodhd_market_data_adapter.py` (`EODHDClient`) | `/eod`, `/intraday`, `/real-time`, `/screener`, `/eod-bulk-last-day` |
| T2 | Hosted EODHD MCP | `market_data/eodhd_mcp_client.py` → `market_data/eodhd_screener.py` | a discovered screener tool |
| T3 | hand-rolled REST inside a route | `api/mc_trader_controller.py:5418` `_run_direct_eodhd_screener` | `/screener` |
| T4 | a second `EODHDClient` built inside the ASX200 route | `api/mc_trader_controller.py:3921`, screener at `:3943`, `get_fundamentals` at `:4025` | `/screener`, `/fundamentals` |

The full site-by-site inventory lives in the McTrader plan. What matters here is the capability
matrix:

| Capability McTrader needs | mMds today | This plan adds |
|---|---|---|
| Raw screener rows, one market at a time | nothing | `POST /screener` |
| Company profile for up to 25 symbols, one request | nothing | `POST /profile` |
| Intraday 5m/15m/30m/1h/4h bars | nothing exposed (`EodhdClient::intraday` exists) | `POST /bars/intraday` |
| Whole-market daily closes for several sessions, one request | nothing exposed (`bulk_last_day` exists, `DailySnapshot` holds rows) | `POST /bars/bulk` |
| A price/bars/ATR for an index, commodity ETF proxy or rate | the library accepts `asset_class`; the HTTP layer drops it | optional `asset_class` on four existing routes |

---

## 2. Contract conventions (unchanged for the new routes)

Every new route follows the conventions the existing four use:

1. `POST` with a JSON body. The body names `country_code` (`US`/`AU`) where a market is needed;
   `/bars/bulk` names `market` instead, the same word `POST /refresh` uses for a whole-market
   operation, and validates it against the same country table. `/screener` takes `country_code`
   plus an optional `exchange` alias; an `exchange` filter inside `filters` remains the vendor's
   own spelling and is lifted (see §3.1).
2. One request is **one market**. A body that names two markets is refused, never half-served.
3. Refusals are FastAPI-shaped: `422 {"detail":[{"type","loc","msg","input"}]}` through
   `ApiError::validation` (`src/api/errors.rs`).
4. Error bodies keep the pair shape `{"error": "...", "detail": "..."}`: `400 exchange-conflict` /
   `mixed-exchanges`, `404 unpriced` / `not-pulled`, `422 vendor-rejected` and the domain
   screener/asset-class refusals (`MarketDataError` with status 422), `502` provider failures,
   `503 vendor-unavailable` / `database-unavailable` / `database-busy`. Boundary validation
   refusals instead use the FastAPI shape in item 3 — the two shapes are deliberately distinct.
5. `X-MarketData-Caller` names the caller for the ledger; `mctrader` is the only caller these
   routes will have. An absent header records `unknown`.
6. Every **new** response states `provider` once at the top level: `"eodhd"` for a vendor answer,
   `"cache"` for an owned-table or memo answer. Today `/price` carries `provider`, `/bars` carries it
   per series, `/prices` only per quote, and `/atr` carries `source`; the new routes follow the
   top-level rule and the existing shapes are left alone.
7. No route is added to the MCP proxy (`src/mcp.rs`, `src/bin/mmds-mcp.rs`). These are REST
   surfaces for a service-to-service caller; an MCP tool would invite a model to drive them.
8. Hand-written OpenAPI entries are mandatory — there is no derive (`src/api/openapi.rs`).
9. **New behaviour on the new routes:** a vendor 4xx caused by the query is a **caller** error and
   maps to `422 vendor-rejected`, never to `502`. A vendor 5xx or transport failure stays at `502`,
   whose `error` is the provider reason (`transport` / `rate-limited` / `http-error` /
   `unparseable`) — `src/api/errors.rs:102-106` already renders it that way. The existing four
   routes map every provider failure to 502 today; the 4xx split is added for the new routes only.

---

## 3. The four new endpoints and the one amended field

### 3.1 `POST /screener` — raw vendor rows for one market

```json
{
  "country_code": "US",
  "filters": [["market_capitalization", ">", 300000000], ["refund_5d_p", ">", 5]],
  "sort": "refund_5d_p.desc",
  "limit": 50,
  "offset": 0
}
```

Response:

```json
{
  "provider": "eodhd",
  "country_code": "US",
  "exchange": "US",
  "filters": [["market_capitalization", ">", 300000000], ["refund_5d_p", ">", 5],
              ["exchange", "=", "US"]],
  "sort": "refund_5d_p.desc",
  "limit": 50,
  "offset": 0,
  "count": 50,
  "rows": [
    {"code": "AAPL", "exchange": "US", "name": "Apple Inc", "sector": "Technology",
     "industry": "Consumer Electronics", "market_capitalization": 3500000000000,
     "refund_5d_p": 2.1, "...": "every other vendor field untouched"}
  ]
}
```

**Request fields.**

| Field | Type | Default | Validation |
|---|---|---|---|
| `country_code` | string | none | required, `US` or `AU` (`require_country`, `src/api/dto.rs:28-54`) |
| `exchange` | string, nullable | none | optional second spelling; must resolve to the same market (`resolve_exchange`) |
| `filters` | array of triples | `[]` | shape, field and operator validated (§ below); at most 64 triples |
| `sort` | string, nullable | vendor default | `<field>.<asc\|desc>`; field must be supported; a derived field is refused |
| `limit` | integer | 100 | 1..500 (`LIMIT_MAX`, the verified vendor cap) |
| `offset` | integer | 0 | ≥ 0 |

**The venue travels in `filters`.** The vendor's screener takes the venue as its own `exchange`
filter; a bare top-level `exchange` parameter is ignored by the vendor (the current
`EODHDClient.get_screener` encodes every filter into one `filters` parameter for this reason,
`market_data/eodhd_market_data_adapter.py:372-405`). The endpoint therefore:

1. resolves the requested market with `resolve_exchange(exchange, country_code, &[], None)` — a
   contradiction (`exchange` and `country_code` naming different markets) is `400 exchange-conflict`;
2. scans `filters` for `exchange` entries: each value must resolve to the same market; `in` with
   more than one distinct market is `400 mixed-exchanges`; an agreeing entry is **lifted**;
3. appends exactly one `["exchange", "=", "<venue>"]` triple **last**, where `<venue>` is chosen by
   the precedence below, so the vendor sees one venue regardless of what the caller sent.

**The appended venue is the caller's value, not the resolved market.** `resolve_exchange("NASDAQ",
"US", …)` returns `US`; appending that would collapse every per-venue US screen into the same US-wide
screen, while McTrader's per-venue loop deliberately asks for NASDAQ, NYSE, AMEX and BATS separately.
The resolved market is therefore used only for agreement and for the response's `exchange`. The venue
appended is, in order: the requested top-level `exchange`; else the single agreeing `exchange` filter
value that step 2 lifted; else the resolved market. The response's `filters` echoes the venue
actually sent.

**Accepted venue values.** The US set is `US`, `NASDAQ`, `NYSE`, `AMEX`, `BATS`, `NYSEARCA`, `ARCA`
and `IEX`; the AU set is `AU`, `ASX`, `AX` and `AUS` (McTrader's `EXCHANGE_VENUE_CODES`,
`market_data/eodhd_field_reference.py:60-135`, folds its other US spellings to `US` before sending).
`BATS`, `ARCA` and `IEX` are in McTrader's vocabulary but missing from `exchange_alias`
(`src/domain/exchanges.rs:65-91`) today, so those three arms must be added before `/screener`
ships; `NYSEARCA` already resolves. `BATS` is the mandatory case — a live US watchlist sends it
(`services/watchlist_service.py:137` returns `['NYSE','NASDAQ','NYSEARCA','AMEX','BATS','IEX']`) —
and `ARCA`/`IEX` would otherwise refuse a legitimate raw proxy query. A code in neither alias table
is `400 unknown-exchange`.

A request naming a venue outside US/AU is refused before the vendor call: `exchange_for` maps
`LSE` → `L`, and with the required `country_code` the mismatched pair is `400 exchange-conflict`;
a code in neither alias table (`NEO`) is `400 unknown-exchange`. The explicit US/AU check on the
resolved market (§0.1) makes the two supported markets the whole scope.

**Full field validation is deliberate transport work.** The earlier draft proposed shape-only
validation to avoid a second copy of McTrader's field reference. The design reverses that: a
vendor 4xx is mapped to `422 vendor-rejected` with the vendor's message, so a misspelt field would
otherwise still reach the vendor — but a 422 that echoes `"invalid field"` is a worse contract than
a local 422 that names the offending filter and the supported list, and the vendor's 200-zero-row
answer for some bad values is silent. The vocabulary is pinned in
`src/domain/screener.rs` and tested against the 14 names:

```
code, name, exchange, sub_exchange, sector, industry, market_capitalization,
earnings_share, dividend_yield, adjusted_close, avgvol_1d, avgvol_200d,
refund_1d_p, refund_5d_p
```

Rules, each with a unit test:

- a filter must be a three-element array: `[field, operator, value]`;
- `field` must be one of the 14; an unknown field's 422 names the field and the list;
- operators: numeric fields accept `=`, `!=`, `>`, `<`, `>=`, `<=`; text fields (`code`,
  `name`, `exchange`, `sub_exchange`, `sector`, `industry`) accept `=` and `in` only
  (`eodhd_field_reference.py:40-43`); `in` requires an array value, every other operator a scalar
  (the module's `NUMERIC_OPS` deliberately omits `in`);
- numeric `in` is refused — the vendor answers HTTP 500 for it (recorded in that module's
  `GOTCHAS`, `:425-433`);
- a derived field (`return_3m_p`, `return_100d_p`, `return_200d_p`) is refused with the local
  workaround named: McTrader computes those from whole-market closes (`/bars/bulk`);
- a rejected field (`country`, `pe_ttm`, `refund_1m_p`, …) is refused as not a screener field
  (`eodhd_field_reference.py:369-417`);
- a GICS-style sector spelling caught by `SECTOR_TRAPS` is refused with its replacement
  (EODHD's `sector`/`industry` are Yahoo-style, not GICS).

The duplication with `market_data/eodhd_field_reference.py` is a recorded risk (§12): MDS owns its
Rust table, McTrader's module stays the human authority, and each side pins the 14 names in a test
so drift fails loudly.

**Sort, limit and offset.** `sort` is `<field>.<asc|desc>`; the field must be one of the 14 and a
derived field is refused; the suffix is required, because the vendor's default direction is not
documented. `limit` is 1..500 and `offset` is ≥ 0; both default as the vendor does.

**Response.** `rows` are the vendor's objects, untouched — field names, `null`s and extra columns
preserved. The service adds no `symbol`, no normalisation and no filtering; McTrader's
`_extract_rows`/`_normalise_candidate` already parse this shape from the MCP transport and are
re-pointed at it unchanged. An empty result is **200** with `count: 0`, not an error: "no company
matched" is an answer.

**Billing:** `Billing::new("screener", 5, 0)` — 5 units flat per call, the measured `/screener`
tariff (`src/usage.rs` tests; `README.md:566` shows a real ledger row of 108 calls costing 540
units). `PURPOSE_REQUEST_TYPE` gains `("screener", "/screener")`; a guarded INSERT seeds the
tariff row (§4.5).

**No cache, no table.** A screen is a question about now; answering it from yesterday's rows would
be worse than paying 5 units.

### 3.2 `POST /profile` — up to 25 listings, one request

```json
{"symbols": ["WDS", "BHP", "CBA"], "country_code": "AU", "exchange": "ASX"}
```

Response:

```json
{
  "provider": "eodhd",
  "country_code": "AU",
  "exchange": "AU",
  "requested": 3,
  "complete": false,
  "missing": ["CBA"],
  "profiles": {
    "WDS": {"symbol": "WDS", "exchange": "AU", "found": true,
            "name": "Woodside Energy Group Ltd", "sector": "Energy",
            "industry": "Oil & Gas E&P", "market_cap": 51234567890.0,
            "currency": "AUD", "country": null, "gic_sector": null,
            "gic_industry": null, "description": null, "as_of": "2026-11-11",
            "error": null},
    "BHP": {"symbol": "BHP", "exchange": "AU", "found": true,
            "name": "BHP Group Ltd", "sector": "Basic Materials", "...": "..."},
    "CBA": {"symbol": "CBA", "exchange": "AU", "found": false,
            "name": null, "sector": null, "industry": null, "market_cap": null,
            "currency": null, "country": null, "gic_sector": null,
            "gic_industry": null, "description": null, "as_of": null,
            "error": null}
  }
}
```

**Request.** `symbols` (1..25 — the batch cost bound), `country_code` required, `exchange`
optional. Validation order mirrors `/bars`: `require_symbols` (`src/api/dto.rs:57-67`),
`require_profile_symbols` (new, 1..25), `require_country`, then
`resolve_exchange(exchange, country_code, &symbols, None)` so suffixes and the one-market rule
apply exactly as for `/bars`. Each symbol is rendered to its vendor root by the same
`domain::symbols` resolution `/price` uses; a symbol the renderer cannot place goes into
`missing`, never a guessed venue.

**The only entitled source is `/screener`.** EODHD `/fundamentals/{ticker}` answers **HTTP 403 for
every ticker on this subscription**, verified 2026-10-12 (recorded in
`McTrader/market_data/eodhd_market_data_adapter.py:412-419`). The profile is therefore a screener
query pinned to `code` **and** `exchange` — the two filters whose necessity is documented at
`get_screener_profile` (`:421-426`): `code` alone is not unique because EODHD keeps one row per
listing, so `WDS` alone answers with Woodside's ASX *and* US records.

**One vendor call per symbol; no `code in` batching.** The vendor's `in` operator is verified on
text fields (`eodhd_field_reference.py:40-43`) but its **coverage is unbalanced**: a live probe of
`["exchange","in",["US","AU"]]` returned 0 AU rows at limits 25/50/100 and 9 of 500 at limit 500
(the module's `GOTCHAS`, `:430-433`). A multi-code `in` on `code` is unproven and, if it silently
omitted a code, a profile would be reported missing with no way to tell "no row" from "not
covered". So each uncached symbol is exactly one call:

```
filters = [["code", "=", root], ["exchange", "=", venue]], limit = 5, no sort
```

and the first returned row whose `code`, trimmed and upper-cased, equals the requested root is the
profile. No match is `found: false`, not an error for the batch. Probe P2 (one call, 5 units)
records whether a future `code in` batch is safe; until it passes, the fan-out is the design and
its cost is stated below.

**Mapping.**

| Response field | Vendor source | When absent |
|---|---|---|
| `symbol` | requested root, upper-cased | never null |
| `exchange` | resolved market code (`US` / `AU`), never a venue name | never null |
| `found` | row match on `code` | `false` |
| `name` | `row.name` | null |
| `sector`, `industry` | `row.sector`, `row.industry` (Yahoo-style, **not GICS**) | null |
| `market_cap` | `row.market_capitalization` (already absolute USD) | null if not numeric |
| `currency` | `row.currency_symbol` | null unless exactly three ASCII uppercase letters (the deterministic ISO-4217-shape rule; probe P3 records the live value) |
| `country`, `gic_sector`, `gic_industry` | no entitled source | always null |
| `description` | `/screener` holds no prose | always null |
| `as_of` | `row.last_day_data_date` | null |
| `error` | the vendor reason when **this symbol's** call failed | null |

`dividend_yield` is deliberately omitted: EODHD reports it as a fraction, Yahoo as a percentage,
and both feed one `{:.2f}%` formatter (`eodhd_market_data_adapter.py:430-434`). Adding it needs a
unit decision, not a field.

**Memo.** An in-process memo, no table, no persistence:

- key `(root upper-cased, resolved venue)`; a duplicate spelling costs one call;
- TTL `PROFILE_TTL_SECONDS = 3600.0`, a constant beside `BARS_REFRESH_TTL_SECONDS`
  (`src/products.rs:68`), clamped by `self.clock.now_utc()`;
- a miss and a failed call are **never** inserted, so a retry asks again;
- cap `PROFILE_CACHE_MAX_ENTRIES = 4096` (about 1.5 MB); entries older than the TTL are pruned on
  insert, and at the cap the fresh entry is returned but not cached with a one-time warning;
- the fan-out runs at most `PROFILE_CONCURRENCY = 5` calls at once with
  `tokio::task::JoinSet` (the client is `Send + Sync` and its methods take `&self`); worst case at
  25 symbols is about 300 s — five waves at concurrency 5, each call bounded by the 30 s
  `eodhd_timeout_seconds` (`src/config.rs:179`) plus the one transport retry in `get_json`
  (`src/providers/eodhd_client.rs:191-196`), i.e. 2 × 30 s + 0.5 s. That bound is part of the
  contract: the caller's client timeout must exceed it, and McTrader's
  `PROFILE_TIMEOUT_SECONDS = 330.0` does (companion plan §3.1).

A persistent `ScreenerProfileCache` table was designed and rejected: it would need DDL,
TableDoc/ColumnDoc, retention and a store repository for a cache whose cold refill at 25 symbols is
at most 125 units, and the README's thinness principle argues against it.

**Billing:** 5 units flat per **uncached symbol**; a 25-symbol batch costs ≤ 125; a memo hit costs
nothing and makes no vendor call. The ledger shows `Calls = N`, `CostTotal = 5N` under `/screener`
for the caller.

**`complete`** is true when no entry is missing and no entry carries an error.

### 3.3 `POST /bars/intraday` — 5m and its aggregates

```json
{"symbols": ["TTD"], "country_code": "US", "timeframe": "4Hour",
 "limit": 48, "allow_fetch": true}
```

Response:

```json
{
  "exchange": "US",
  "provider": "eodhd",
  "fetched": ["TTD"],
  "short_history": [],
  "bars": {
    "TTD": {
      "symbol": "TTD",
      "exchange": "US",
      "as_of_session": "2026-11-12",
      "sessions": 1,
      "adjusted": false,
      "provider": "eodhd",
      "source": "eodhd",
      "rows": [
        {"d": "2026-11-12T09:30:00", "o": 231.4, "h": 233.1,
         "l": 230.9, "c": 232.7, "v": 1200300, "cr": null}
      ]
    }
  }
}
```

**Series shape is `/bars`'s, unchanged.** `bars` is an object keyed by the symbol as sent; each
series carries `symbol`, `exchange`, `as_of_session`, `sessions`, `adjusted`, `provider`, `source`
and `rows` with the same `d/o/h/l/c/v/cr` keys as `POST /bars`'s `BarsEntry`
(`src/products.rs:297-306`). The one difference is stated rather than implied: **`d` is
an exchange-local ISO timestamp, not a session date**, because an intraday bar is a moment. This is
what lets McTrader reuse `_series_key` (`data_service_client.py:495`) and `_series_from`
(`:968-998`) without a second decoder. `cr` is null: the vendor's intraday tape is unadjusted and
carries no separate raw
print. `provider` is `"eodhd"` when anything was fetched and `"cache"` when the owned table
answered every symbol; `source` names the tape per series.

**Timeframes:** `5Min`, `15Min`, `30Min`, `1Hour`, `4Hour`, plus the aliases `5m`, `15m`, `30m`,
`1h`, `4h`, case-insensitive. McTrader's `BarTimeframe` enum names `4Hour`, `1Hour`, `30Min`,
`15Min` (`models/domain/enums.py:619-627`); `5Min` is not an enum member there — 5m is the vendor
interval `get_historical_bars` requests and the base the others are aggregated from. `1Day` and
`1Week` are not intraday and stay with `/bars`.

**One tape per request.** The vendor's native intraday interval is 5m (`EodhdClient::intraday`,
`src/providers/eodhd_client.rs:410-458`). Every timeframe in a request is derived from **that same
5m series** — never mixed with `BarHourly`, which is adjusted, or `Bar5Min`, which is unadjusted,
US-only and was last written on 2026-10-30 (§5.2). A response therefore carries one
`adjusted: false` for every series in it.

**Aggregation, pinned.** For each symbol, group the 5m rows by the **exchange's local session
date**, then chunk each session's rows consecutively in timestamp order:

| Timeframe | Bars per bucket |
|---|---|
| `5Min` | 1 |
| `15Min` | 3 |
| `30Min` | 6 |
| `1Hour` | 12 |
| `4Hour` | 48 |

Each bucket: `o` = first bar's open, `h` = max, `l` = min, `c` = last bar's close, `v` = sum,
`d` = first bar's local timestamp. A short final bucket is kept, not dropped.

**This is a deliberate correction, not parity.** McTrader's current `_aggregate_by_count`
(`eodhd_market_data_adapter.py:266-295`) chunks the whole returned series from its first bar, and
the series begins at `now - window_days` — an arbitrary mid-session instant. The same 4H candle
therefore changes shape as the clock advances and can straddle the overnight gap. Session-anchored
buckets reset at each session's first bar, so a 4H bar is stable across requests and never mixes
two sessions. The McTrader plan records this as an expected behaviour change (M1) and its tests pin
the new rule.

The session-date grouping uses the exchange timezone from the market clock
(`ResolvedSession.exchange_timezone`, `src/clock.rs:196`, populated by
`MarketClock::exchange_session` at `:621`), keyed by the exchange resolved in
`src/domain/exchanges.rs` — not UTC: a US session spans two UTC dates in winter, and a UTC grouping
would split the last hour of every session into a second, one-bar "session".

**Storage.** A new owned table `dbo.IntradayBar5Min` (§5.1) stores the **native 5m rows only**,
keyed `(Exchange, SymbolCode, TsUtc)`. Derived timeframes are computed on read; storing them would
duplicate derived state and allow the 5m and 4H rows to disagree after a re-pull.

**Fetch policy.** `allow_fetch: false` serves the owned table only; a symbol with no stored rows is
listed in `short_history` and omitted from `bars`. `allow_fetch: true` fetches from
`EodhdClient::intraday` when the stored series is shorter than `limit` buckets or older than the
TTL (`MDS_INTRADAY_TTL_SECONDS`, default 300), upserts the 5m rows, and re-derives. The request
window handed to the vendor is `start = now - window_days`, with `window_days` scaled per timeframe
exactly as `get_historical_bars` does today (4H: `max(30, limit*1.5)`; 1H: `max(14, limit*2)`;
otherwise `max(7, limit)`), capped at the vendor's 600 days. The vendor call runs under
`eodhd_bulk_timeout_seconds` (60 s default) with the one transport retry inside `get_json`, so the
caller's client timeout must exceed about 120 s (companion plan §2).

**Billing:** 5 units flat per symbol fetch (`Billing::new("intraday", 5, 1)`). A cache hit costs
nothing. The response's `fetched` list is the spend evidence.

**Crypto and forex:** `asset_class` is not a field on this route. Binance is not reachable through
it; McTrader's crypto intraday leg is out of scope for this plan (the umbrella plan's S2 owns the
intraday leg and S8 the crypto quotes, and no current crypto intraday consumer exists).

### 3.4 `POST /bars/bulk` — whole-market closes for several sessions

```json
{"market": "US", "dates": ["latest", "2026-08-13"], "allow_fetch": true}
```

Response:

```json
{
  "market": "US",
  "provider": "cache",
  "partial": [],
  "dates": {
    "latest": {"session": "2026-11-11",
               "rows": {"AAPL": {"c": 231.4}, "MSFT": {"c": 512.7}}},
    "2026-08-13": {"session": "2026-08-13",
                   "rows": {"AAPL": {"c": 218.9}}}
  }
}
```

**Request.** `market` (`US`/`AU`, defaults `US` like `POST /refresh`), `dates` (1..10 entries, each
`"latest"` or `YYYY-MM-DD`; duplicates collapse), `allow_fetch` (default true). `symbols` is not a
parameter: no caller filters a bulk pull, and a filter would be policy. The response's `dates`
object is keyed by the requested key, so `"latest"` is echoed as `"latest"` and its `session`
names the session the service resolved.

**`c` is the adjusted close.** `DailySnapshot` stores the adjusted series in `Close` and the
printed price in `RawClose` (`sql/001_schema.sql:113-133`). Return windows are computed from
adjusted closes, so `c` is `Close`; the raw print is not exposed because nothing consumes it
through this route. (The vendor row's two scales are reconciled at the upsert, not at the
response.)

**Completeness marker.** A session is served **as complete** from `DailySnapshot` only when a
completed pull is recorded for `(market, session)` in the new `dbo.BulkSnapshotPull` table (§5.3).
The latest session in `DailySnapshot` can be partial — 1,881 rows against the ~27,714 codes a
completed pull writes (probe P8 records the live count) — and a partial set silently changes every
derived return that reads it. The marker is the completeness fact the read path trusts; whether a
pull earned one is decided at write time, by the pull itself.

- `allow_fetch: true` with no marker calls `EodhdClient::bulk_last_day(market, Some(date))`
  (`latest` first resolves the session through the same `latest_completed_session` the `/bars`
  path uses), upserts every returned row into `DailySnapshot` (`Source='eodhd'`, `IsAdjusted=1`,
  `RawClose` from the vendor's `close`), and answers. It writes the marker only when the pull is
  complete; a pull whose row count is materially below the previous completed session's is upserted
  but writes no marker (the live 1,881-vs-27,714 gap is the evidence such a check is needed), and
  its requested date key is listed in the response's `partial` array.
- `allow_fetch: false` with no marker is `404 not-pulled`:
  *"no completed bulk pull for US 2026-11-07; this service will not return a partial session as if
  it were whole"*.
- A failed pull writes no marker; a crash between upsert and marker leaves the session marked
  absent, which is the safe direction (a refetch, not a wrong answer).

**`partial` is the incomplete-pull list.** It names the requested date keys whose rows came from a
pull without a `BulkSnapshotPull` marker, and it is the only way an unmarked session is returned —
only on the response of the pull that produced it, never on a later cache read. The service keeps no
partial-state timer: the next `allow_fetch: true` request for that date calls the vendor again at 100
units. How often the caller re-asks is the caller's decision (McTrader's
`PARTIAL_RESULT_TTL_SECONDS = 300`, companion plan §3.8).

**Dated pulls carry delisted listings.** The client's own doc comment records it
(`eodhd_client.rs:462-465`): a dated pull returns ~50,270 codes against ~27,714 for the current
session, because listings that stopped trading are still returned. The endpoint returns them —
filtering is policy. McTrader already intersects a dated set with the current session's codes
(`services/eodhd_bulk_close_service.py` — `EODHDBulkCloseService.return_window_pct_by_symbol` at
`:128`, renamed `ReturnWindowService` at M2 by the companion plan — over the pure arithmetic in
`Utils/eodhd_return_utils.py`), and that logic stays there.

**Billing:** 100 units flat per **dated vendor pull**, filed under `/eod-bulk-last-day/US` or
`/AU` (the AU row exists at `sql/001_schema.sql:477-487`; the US row is the purpose default). A
request with two uncached dates costs 200; a cache hit costs nothing. A **completed** (marked)
session is free indefinitely; a date returned in `partial` carries no marker, so the next
`allow_fetch: true` request re-pulls it at 100 units; the service's marker is never written for a
partial session. The dated vendor pull runs under `eodhd_bulk_timeout_seconds` (60 s default) with
the one transport retry inside `get_json`, so the caller's client timeout must exceed about 120 s
(companion plan §2).

### 3.5 `asset_class` on the four existing routes

| Route | Field | Applies to |
|---|---|---|
| `GET /price/{symbol}` | query param `asset_class` | that price |
| `POST /prices` | body field `asset_class` | the whole batch |
| `POST /atr` | body field `asset_class` | the whole batch |
| `POST /bars` | body field `asset_class` | the whole batch |

Accepted values: `shares`, `crypto`, `indices`, `commodities`, `rates`, `forex`, plus the aliases
`AssetClass::parse` already accepts (`equities`/`equity`, `index`, `indicies`, `futures`/`future`,
`commodity`, `bonds`/`rate`, `fx`/`currency`, `cryptocurrency` — `src/domain/asset_class.rs:39-47`).
Absent ⇒ `shares`. Unknown ⇒ `422 unknown-asset-class` with the accepted list in `detail`.

`ProductLayer::price`/`bars` and `AtrRequest.asset_class` already take the value
(`src/api/layer.rs:30-51`; `src/products.rs:615,657,898,1124`); only `src/api/dto.rs`,
`src/api/data.rs`, `src/domain/asset_class.rs` (the strict parser) and `src/api/openapi.rs` (the four
request schemas) change. The library's infallible fallback stays and its tests stay green; the
HTTP layer calls a new `AssetClass::parse_strict` (or an equivalent `Option`-returning free
function) so the refusal is at the boundary, where a caller can see it.

**Compatibility:** an absent field behaves exactly as today. No existing caller breaks.

---

## 4. Code changes by file

### 4.1 Domain (new, pure, unit-tested)

- `src/domain/screener.rs` (new)
  - `SCREENER_FIELDS: &[&str]` — the 14 names in §3.1;
  - `NUMERIC_OPS`, `TEXT_OPS`, `DERIVED_FIELDS`, `REJECTED_FIELDS`, `SECTOR_TRAPS` — the
    tables from `eodhd_field_reference.py`, restated with the McTrader line quoted in a comment;
  - `validate_filters(filters: &[Value]) -> Result<(), MarketDataError>` — shape, field, operator
    and value type; 422 locators `["body","filters",index]`;
  - `parse_sort(raw: Option<&str>) -> Result<Option<String>, MarketDataError>` — `<field>.<asc|desc>`;
  - `screener_wire_filters(filters: &[Value], requested: Option<&str>, market: &str) -> Result<Vec<Value>, MarketDataError>`
    — lift every agreeing `exchange` entry, refuse a conflict or a multi-venue `in`, append exactly
    one `["exchange","=",venue]` last, where `venue` is `requested`, else the lifted value, else
    `market` (§3.1);
  - `profile_filters(code: &str, venue: &str) -> Vec<Value>` and `PROFILE_QUERY_LIMIT: i64 = 5`;
  - tests: the 14-name table; shape; unknown field names itself and the list; `country` refused;
    derived refused with the local workaround; numeric `in` refused; text comparison operators
    refused; sector trap refused with replacement; exchange filter agreement/lift/append-once;
    `in` spanning two venues refused; sort syntax; derived sort refused; limit 501 and negative
    offset refused; venue and country naming different markets refused; a `BATS` screen appends
    exactly `["exchange","=","BATS"]` rather than the resolved market code.
- `src/domain/intraday.rs` (new)
  - `Timeframe` enum (`FiveMin`, `FifteenMin`, `ThirtyMin`, `OneHour`, `FourHour`) with
    `parse`/`bars_per_bucket`/`as_str`; aliases as in §3.3;
  - `aggregate(rows: &[IntradayRow], timeframe: Timeframe, session_date_of: impl Fn(i64) -> NaiveDate) -> Vec<IntradayRow>`
    — session-grouped consecutive chunking, the §3.3 rule. The caller supplies the timezone
    conversion so the function stays pure.
  - tests: a three-day series with unequal day lengths; a half-day session; a DST transition; a
    short final bucket kept; bucket `d` = first bar's timestamp; 4H never straddles a session date.
- `src/domain/asset_class.rs`: add `parse_strict(raw: &str) -> Option<AssetClass>` (or a free
  function) used only by the HTTP boundary; `parse` is unchanged.

### 4.2 Provider client

`src/providers/eodhd_client.rs`

- Add
  `pub async fn screener(&self, filters: &[Value], sort: Option<&str>, limit: i64, offset: i64) -> Result<Value, ProviderFailure>`
  — GET `/screener` with `filters` as a JSON-encoded string (the vendor ignores individual
  top-level params), `sort`, `limit`, `offset`; `Billing::new("screener", 5, 0)`; returns the raw
  body. The call uses `self.timeout` (the 30 s `eodhd_timeout_seconds`), like `real_time` and `eod`,
  with the one transport retry `get_json` already applies — this is the bound `/profile` multiplies.
  The vendor answers `{"data": [...]}`; `Products` unwraps `data` so an unexpected envelope
  is a visible `unparseable` failure rather than an empty screen.
- `EodhdApi` (`src/providers/eodhd_client.rs:518-550`) gains `screener`; its doc comment's "five
  vendor calls" becomes six. `ScriptedClient` (`src/providers/eodhd.rs:240-308`; `impl EodhdApi`
  `:255-308`) implements the new method. The trait exists so
  provider tests can script the calls the router uses; `/screener` and `/profile` both go through
  it.
- Wiremock tests beside the existing harness (`eodhd_client.rs:727`): one 200 with rows; one 422
  carrying the vendor message; one 429 retry; assert the billed units and that a 422 is billed
  once and not retried (the existing `get_json` rule, `:196-307`).

### 4.3 Products layer

`src/products.rs`

- `pub async fn screener(&self, country_code: &str, exchange: Option<&str>, filters: &[Value], sort: Option<&str>, limit: i64, offset: i64) -> Result<ScreenerResponse, ProductError>`
  — `resolve_exchange`, then `screener_wire_filters(filters, exchange, &market)` (§3.1: the requested
  or lifted venue travels, never the resolved market), `parse_sort`, then one `client.screener`; 503
  `vendor-unavailable` when `self.client` is `None`. A vendor 4xx becomes
  `ProductError::Refused(validation)` (`422 vendor-rejected`); other failures go through
  `From<ProviderFailure>` (502) — `ProductError` has no `MarketData` variant (`src/products.rs:182-187`).
- `pub async fn profiles(&self, request: ProfilesRequest) -> Result<ProfilesResponse, ProductError>`
  — resolves the market, renders each symbol to its root, dedupes, reads the memo, fans out at
  `PROFILE_CONCURRENCY`, maps each row, writes the memo. A per-symbol provider failure becomes
  `ProfileEntry { found: false, error: Some(reason), .. }` and does not fail the batch. A missing
  client is the same 503 refusal.
- `pub async fn intraday(&self, symbols, timeframe, limit, allow_fetch, exchange, country_code) -> Result<IntradayResponse, ProductError>`
  — owned-table read; fetch through `client.intraday`; derive with `domain::intraday`; upsert.
- `pub async fn bulk_closes(&self, market: &str, dates: &[String], allow_fetch: bool) -> Result<BulkResponse, ProductError>`
  — marker check, `DailySnapshot` read, `client.bulk_last_day`, upsert, marker write.
- New constants: `PROFILE_TTL_SECONDS = 3600.0`, `PROFILE_CACHE_MAX_ENTRIES = 4096`,
  `PROFILE_CONCURRENCY = 5`, beside `BARS_REFRESH_TTL_SECONDS` (`:68`).
- `Settings` (`src/config.rs:93`) gains `intraday_ttl_seconds` (default 300) parsed from
  `MDS_INTRADAY_TTL_SECONDS` in `src/config.rs`; `intraday_retention_days` (default 30) from
  `MDS_INTRADAY_RETENTION_DAYS`.
- `Products::client()` (`:602-604`) is how `screener` reaches the concrete client; no new state.

### 4.4 API layer

- `src/api/dto.rs`
  - `PriceQuery` gains `pub asset_class: Option<String>`; `PricesRequest`, `AtrRequestBody`,
    `BarsRequestBody` gain the same.
  - New `ScreenerRequest { country_code: Option<String>, exchange: Option<String>, filters: Vec<Value>, sort: Option<String>, limit: Option<i64>, offset: Option<i64> }`;
    `ProfilesRequest { symbols: Vec<String>, country_code: Option<String>, exchange: Option<String> }`;
    `IntradayRequest { symbols, timeframe, limit, allow_fetch, exchange, country_code }`;
    `BulkRequest { market: Option<String>, dates: Vec<String>, allow_fetch: bool }`.
  - `require_asset_class(raw: Option<&str>, location: &[&str]) -> Result<Option<String>, ApiError>`
    — the strict parse, returns the canonical spelling.
  - `require_limit_between(low, high, value, location)`; `require_bulk_dates` (1..10);
    `require_offset`.
  - `require_profile_symbols(symbols: &[String], location: &[&str]) -> Result<(), ApiError>` —
    1..25, FastAPI-shaped `too_short`/`too_long`, in the `require_symbols` family
    (`src/api/dto.rs:57-67`). This is the only profile-symbol validator; §4.1's domain module does
    not duplicate it.
- `src/api/screener.rs` (new) — the two handlers, one module:
  - `pub async fn post_screener(State, HeaderMap, Json(body)) -> Response`: caller →
    `state.products()` or 503 → `require_country` → `ProductLayer::screener` → render;
  - `pub async fn post_profile(...)`: the same skeleton with `require_profile_symbols` and
    `resolve_exchange` before the product call.
  - `src/api/mod.rs` gains `pub mod screener;`.
- `src/api/data.rs`
  - The four existing handlers pass `asset_class.as_deref()` instead of `None`, and delete the
    four "no asset class is a request field" comments (`:44-45`, `:81`, `:123`, `:163`).
  - `post_intraday` and `post_bulk` handlers live here, beside the existing data handlers, because
    they are bar products; register them in `src/api/router.rs` beside `POST /bars`.
- `src/api/router.rs`: four `.route(...)` lines (two in `data.rs`'s group, two in `screener.rs`'s).
- `src/api/layer.rs`: `ProductLayer` gains `screener` and `profiles`; `impl ProductLayer for
  Products` forwards. `src/api/tests.rs`'s `FakeProducts` (`:225-299`) implements both.
- `src/api/openapi.rs`: four path entries and the `asset_class` field on the four existing request
  schemas; `ScreenerRequest` and `ProfilesRequest` components; `IntradayRequest` and
  `BulkRequest`. The document is hand-written and nothing fails the build if a route is missing,
  so each entry ships in the same slice as its route.
- `src/api/errors.rs`: reuse `ApiError::validation` for boundary refusals; `MarketDataError` with
  status 422/400 renders through the existing `From`.

### 4.5 Billing and configuration

- `src/usage.rs`: `PURPOSE_REQUEST_TYPE` (`:32-38`) gains `("screener", "/screener")` after
  `("intraday", "/intraday/")`.
- No new request types: `/intraday/` (5 per call, `per_call`) and `/eod-bulk-last-day/US` + `/AU`
  (100 flat) exist.
- `sql/001_schema.sql`:
  - guarded, idempotent DDL for `dbo.IntradayBar5Min` and `dbo.BulkSnapshotPull` (§5);
  - `TableDoc`/`ColumnDoc` rows for both, following the existing pattern at `:167-259`;
  - a guarded `sp_executesql` insert of the `/screener` RequestTypes row after the AU bulk block
    (`:462-488`), following that block's pattern. The row is priced flat 5 because
    `README.md:566` records 108 calls costing 540 units and the usage fixture already prices it
    (`src/usage.rs:345-349`); `Is_Estimated = 0`. Probe P1 re-reads the live row; if the live row
    disagrees, the row wins over the literal 5 in code (`spent`, `eodhd_client.rs:167-189`; the
    tariff override is `:177-181`).
- `sql/002_indexes.sql`: **no change** — `IX_IntradayBar5Min_Symbol` is created with the table
  inside the §5.1 guard in `sql/001_schema.sql`. `002_indexes.sql` is a manual load-quiet index swap
  that the service never applies (`src/store/schema.rs:20` embeds only `001_schema.sql`), so an index
  placed there would never exist.

### 4.6 No change

`src/mcp.rs`, `src/bin/mmds-mcp.rs` (REST-only), `src/store/warehouse.rs` (the intraday endpoint
does not read the estate tape — §5.2), `src/api/state.rs` (Products already carries the client and
store).

---

## 5. Storage

### 5.1 `dbo.IntradayBar5Min` (new, owned)

```sql
IF OBJECT_ID('dbo.IntradayBar5Min', 'U') IS NULL
BEGIN
    CREATE TABLE dbo.IntradayBar5Min (
        Exchange    varchar(8)    NOT NULL,
        SymbolCode  varchar(24)   NOT NULL,
        TsUtc       datetime2(0)  NOT NULL,
        [Open]      decimal(18,6) NOT NULL,
        [High]      decimal(18,6) NOT NULL,
        [Low]       decimal(18,6) NOT NULL,
        [Close]     decimal(18,6) NOT NULL,
        Volume      bigint        NOT NULL CONSTRAINT DF_IntradayBar5Min_Volume DEFAULT (0),
        Adjusted    bit           NOT NULL CONSTRAINT DF_IntradayBar5Min_Adjusted DEFAULT (0),
        FetchedUtc  datetime2     NOT NULL CONSTRAINT DF_IntradayBar5Min_FetchedUtc DEFAULT (SYSUTCDATETIME()),
        CONSTRAINT PK_IntradayBar5Min PRIMARY KEY CLUSTERED (Exchange, SymbolCode, TsUtc)
    );

    CREATE NONCLUSTERED INDEX IX_IntradayBar5Min_Symbol
        ON dbo.IntradayBar5Min (SymbolCode, Exchange, TsUtc DESC);
END
GO
```

Rationale:

- **Owned, not the estate tape.** `Bar5Min` is unadjusted, US-only, and its newest row is
  2026-10-30 (probe below); its provenance per symbol is not knowable from the table. A table
  written only by this endpoint has one writer, one meaning and one TTL.
- **5m only.** Derived timeframes stay derived (rule 19: one source of truth for a bar).
- **Headroom (rule 22).** 5m rows for 200 symbols × 78 bars × 60 sessions ≈ 936,000 rows ≈
  ~100 MB with the clustered key. The table is pruned by TTL: a daily job (or the upsert itself)
  deletes rows older than `MDS_INTRADAY_RETENTION_DAYS` (default 30). No unbounded growth.
- `Adjusted` is stored, not assumed, so a future adjusted source cannot silently mix.

### 5.2 Why `Bar5Min`/`BarHourly` are not read

Probe P4 records the facts. Expected today: `Bar5Min` ~80.9M rows, newest 2026-10-30, US-only;
`BarHourly` adjusted, newest 2026-10-14. Both are stale, only `BarHourly` lacks an `Exchange`
column, and neither carries a per-symbol freshness guarantee. Serving them would make an intraday
request
return days-old bars indistinguishable from fresh ones unless a per-symbol freshness gate were
invented here — and the estate tape is not this service's to police. If P4 ever shows the tape is
being written in real time for US symbols, a follow-up may add it as a `provider: "warehouse"`
warm source behind an explicit freshness gate. That is a separate plan, not this one.

### 5.3 `dbo.BulkSnapshotPull` (new, owned)

```sql
IF OBJECT_ID('dbo.BulkSnapshotPull', 'U') IS NULL
BEGIN
    CREATE TABLE dbo.BulkSnapshotPull (
        Market       varchar(8)  NOT NULL,
        SessionDate  date        NOT NULL,
        RowCount     int         NOT NULL,
        Provider     varchar(16) NOT NULL,
        CompletedUtc datetime2   NOT NULL CONSTRAINT DF_BulkSnapshotPull_CompletedUtc DEFAULT (SYSUTCDATETIME()),
        CONSTRAINT PK_BulkSnapshotPull PRIMARY KEY CLUSTERED (Market, SessionDate)
    );
END
GO
```

One row per completed whole-market pull. `POST /bars/bulk` reads it before serving a cached
session. It is deliberately separate from `DailySnapshot`: a row in `DailySnapshot` says "this
symbol traded that day", not "this market was fully pulled that day". The column is `Market`, not
`Exchange`, because the bulk pull is keyed by the vendor's market parameter and the response uses
the same word.

### 5.4 No table for `/screener`; only an in-process memo for `/profile`

`/screener` answers a question about now and is not cached. `/profile`'s memo is process memory
(§3.2) with no DDL: persisting it would add a table, TableDoc/ColumnDoc, retention and a store
repository for a cache whose cold refill is bounded by the 25-symbol cap.

---

## 6. Billing and the tariff rows

| Call | Purpose | Request type | Units | Evidence |
|---|---|---|---|---|
| `/screener` | `screener` (new) | `/screener` | 5 flat | measured; `README.md:566` shows a live ledger row |
| `/profile`, uncached symbol | `screener` | `/screener` | 5 each | one pinned screener call per symbol |
| `/profile`, memo hit | — | — | 0 | no vendor call |
| `/bars/intraday` fetch | `intraday` | `/intraday/` | 5 per call, one call per symbol | `eodhd_client.rs:453`; the tariff fixture records its basis as `per_call` (`src/usage.rs:330-333`) |
| `/bars/bulk` fetch, per date | `eod_bulk` | `/eod-bulk-last-day/US` or `/AU` | 100 flat | `eodhd_client.rs:476-482`; AU row at `sql/001_schema.sql:477-487` |

Every call is recorded through `UsageRecorder` under the `X-MarketData-Caller` value, so the
McTrader plan's verification can say "the screener spend appears under mMds's ledger, caller
`mctrader`" rather than "we think it moved".

**Probe P1 (read-only, before S3):** on `Market_Data`, confirm the RequestTypes rows:

```sql
SELECT RequestType, CostBasis, CostEach FROM dbo.RequestTypes
WHERE RequestType IN (N'/screener', N'/intraday/', N'/eod-bulk-last-day/US', N'/eod-bulk-last-day/AU');
```

Expected: one row each (`/screener` 5 flat, `/intraday/` 5 `per_call`, both bulk rows 100 flat). The
guarded INSERT creates `/screener` where it is absent and leaves a different existing row alone;
the endpoint must never be billed as `unknown`.

---

## 7. Errors, refusals and observability

| Case | Status | `error` | Detail carries |
|---|---|---|---|
| asset class not a known name | 422 | `unknown-asset-class` | the accepted list |
| screener country missing or unknown | 422 | `missing` / `literal_error` | `US` or `AU` |
| screener exchange/country disagree | 400 | `exchange-conflict` | both markets |
| exchange code in neither alias table | 400 | `unknown-exchange` | the accepted codes |
| screener filter shape/field/operator bad | 422 | `unsupported-filter` / `unknown-field` / `unsupported-operator` | the offending filter and, for a field, the list |
| screener `in` on a numeric field | 422 | `unsupported-operator` | why the vendor 500s |
| screener derived field | 422 | `derived-field` | the `/bars/bulk` workaround |
| screener sector trap | 422 | `sector-trap` | the replacement spelling |
| screener sort malformed or derived | 422 | `invalid-sort` | `<field>.<asc\|desc>` and the list |
| screener limit/offset out of bounds | 422 | `less_than_equal` / `greater_than_equal` | the bound |
| screener exchange filter spans two markets | 400 | `mixed-exchanges` | both markets |
| profile symbol count 0 or >25 | 422 | `too_short` / `too_long` | the bound |
| profile symbol cannot be placed | per-symbol `found:false` + `missing` | — | not an HTTP error |
| vendor rejects the screener query (400/422) | 422 | `vendor-rejected` | the vendor's message |
| two markets named | 400 | `exchange-conflict` | both markets |
| bulk date malformed or more than 10 | 422 | `invalid-date` / `too_long` | the offending value |
| bulk session not pulled, `allow_fetch:false` | 404 | `not-pulled` | the market and date |
| vendor failed (5xx / transport / rate-limited) | 502 | the provider reason (`transport` / `rate-limited` / `http-error` / `unparseable`) | the provider detail |
| no EODHD client configured | 503 | `vendor-unavailable` | — |
| database unavailable | 503 | `database-unavailable` | — |
| database lock timeout (1222) | 503 | `database-busy` | retryable |

Observability:

- `/stats` counters: add `intraday_fetches`, `intraday_cache_hits`, `screener_calls`,
  `profile_calls`, `profile_memo_hits`, `bulk_pulls` beside the existing price counters.
- Each fetch logs one line naming the route, the market, the symbols and the units via the usage
  ledger; no symbol loop logs per row.
- The `provider` field is the response's own account of which tape answered; a cache answer never
  claims `eodhd`.

---

## 8. Documentation and guard-catalogue updates

- `INTERFACE.md`
  - Add a route table near the top of the file (before §1, which is the `GET /price/{symbol}`
    section; no route table exists there today), with the four new endpoints.
  - New sections for each, in the style of `/bars` and `/prices`.
  - Replace the "There is no asset-class request field" paragraph (`:50-53`) with the optional
    field, its accepted values, the default and the 422.
  - §6 provider routing: update the line at `:377-378`.
  - §7 traps: replace trap 7 (`:406-407`) with "an asset class must be stated; an index sent as a
    share is a different instrument".
- `README.md`
  - The "Out:" paragraph (`:1183-1192`) becomes "Out: scoring, ranking policy, floors,
    portfolio/risk logic" with the §0.1 rationale appended. Update the In sentence (`:1179-1181`)
    to name screener rows and basic company profiles.
  - The "no asset-class parameter" paragraphs (`:104-106`, `:255-263`) gain the optional field.
  - The endpoint list near the top of the file gains the four routes.
- `WIRING.md`
  - §6's "There is no asset-class field" bullet (`:163-164`) gains the optional field.
  - Add a migration table (capability → mMds route → McTrader leg deleted) — none exists today; or
    add the four rows to §1's two-column delete list (`:14-20`). Include
    the caller header, the one-market rule, the 25-symbol profile cap, the one-hour memo and the
    `/screener` tariff.
- `docs/GUARD_CATALOGUE.md`
  - G-17 · Screener venue agreement — `src/domain/screener.rs::screener_wire_filters`; triggers on
    an `exchange` filter naming another market or more than one venue; clear by stating one market
    at the top level.
  - G-18 · Profile memo — `src/products.rs::PROFILE_TTL_SECONDS`; gates whether `/profile` spends
    5 units or answers from memory; misses and failures are never memoised.
  - G-19 · `unknown-exchange` — why a 0-row screen is the trap.
  - G-20 · `unknown-asset-class` — why the silent fold was removed.
  - G-21 · `not-pulled` — why an unmarked session is never served as complete; a partial pull is
    returned only with its key in `partial`, and `allow_fetch: false` answers 404.
  - Amend the asset-class note (the "internal caller may still state a crypto asset class… no HTTP
    request can" sentence) to say an HTTP request now can, strictly.
  - `tests/guard_catalogue.rs:16-43` machine-checks the `Location` lines; add the matching rows.
- `TODO/Loose-Ends.md`: record, below a line of 32 asterisks per rule 21, that
  `D:/McTrader/tests/test_analysis_screener_live.py` and the Postman `EODHD` collection held a
  literal EODHD API key. The fix is the companion plan's step 0 — delete both files and rotate the
  key in the EODHD account — and no keyring re-point is needed, because McTrader loses the key
  entirely at M7. The value is not reproduced here. Rotation is an operator step outside HTTP:
  edit the service account's `market_data:eodhd:global` / `eodhd:api_key` entry in Windows
  Credential Manager and restart the service; `POST /secrets/eodhd` is set-once and refuses with
  409 `key-already-set`, so it cannot replace a key.
- The plan file itself: when the last slice lands, delete it (rule 21) and move any deferred coding
  work to `TODO/Loose-Ends.md`.

---

## 9. Tests

All additive; no existing test changes except where this plan alters an existing response shape
(none do — `asset_class` absent is byte-identical to today).

| Tier | Location | Cases |
|---|---|---|
| Domain | `src/domain/screener.rs` | the 14 cases in §4.1 |
| Domain | `src/domain/intraday.rs` | session grouping; half-day; DST; short bucket; no cross-session 4H |
| Client | `src/providers/eodhd_client.rs` tests | `/screener` 200; envelope without `data`; 422 billed once; 429 retry; params encoded as one JSON string |
| Usage | `src/usage.rs` | `the_screener_purpose_prices_at_five_flat`; the existing `every_internal_purpose_maps_to_a_tariff_row` (`:422-430`) covers the new entry |
| Products | `tests/products.rs` (the `FakeProvider`/`FakeStore` doubles in `tests/common`, no HTTP) | screener pins one venue and returns rows untouched; screener is not cached; profile pins code+exchange and asks for 5 rows; profile costs 5 units a symbol; memo hit costs nothing; memo expires after the hour; miss not cached; duplicate symbol asked once; one vendor failure is that symbol only; cache never exceeds its cap; intraday cache hit vs fetch; bulk marker gate |
| API | `src/api/tests.rs` (`FakeProducts` extended) | screener without country refused; unknown exchange keeps the source reason; exchange conflict before the product layer; bad filter is FastAPI-shaped 422; request reaches the product layer with the venue appended once; sort/limit defaults; profile cap 422; profile suffixes resolve through the country; unavailable fields null; missing product layer is 503 for both routes; `asset_class` default/known/unknown on all four existing routes; `/bars/intraday` cache-only; `/bars/bulk` `not-pulled` |
| Integration | `tests/store_integration.rs` (skipped unless `MDS_TEST_DATABASE` is set; `MDS_TEST_SCHEMA=1` additionally runs the embedded-schema test) | `IntradayBar5Min` upsert is idempotent; `BulkSnapshotPull` marker round-trip; a re-run does not duplicate; `/screener` tariff row seeded once and a pre-existing row left alone |
| Guard | `tests/guard_catalogue.rs` | every new refusal appears in `GUARD_CATALOGUE.md` (the existing pattern) |
| Docs | `src/api/openapi.rs` tests if present | each new path present with its refusal statuses |

The wiremock harness at `eodhd_client.rs:727` is reused; no new test dependency.

---

## 10. Slices

Each slice compiles, passes `cargo test`, and is independently shippable. No slice requires a
feature flag or a coexistence path (rule 1). S1–S5 unlock the McTrader M-slices; S6 closes the docs.

### S1 — `POST /bars/intraday` (unlocks McTrader M1)

- `domain/intraday.rs`, `IntradayBar5Min` DDL + repository, `Products::intraday`, DTO/handler/route/
  OpenAPI, `MDS_INTRADAY_TTL_SECONDS` and `MDS_INTRADAY_RETENTION_DAYS`, tests.
- **Done when** a 5Min request returns vendor rows (`adjusted:false`), a second request inside the
  TTL is a cache hit, a 4Hour request's first `d` equals that session's first 5m bar, and the
  ledger shows 5 units per fetched symbol.

### S2 — `POST /bars/bulk` (unlocks M2)

- `BulkSnapshotPull` DDL + repository, `DailySnapshot` upsert path, `Products::bulk_closes`,
  DTO/handler/route/OpenAPI, tests.
- **Done when** a first call for a date fetches and writes the marker; a second call answers from
  `DailySnapshot` with `provider: "cache"` and no vendor spend; a two-date request with two
  uncached dates costs 200; `allow_fetch:false` on an unpulled date is `404 not-pulled`.

### S3 — `POST /screener`, with its tariff (unlocks M3, then M4)

- `domain/screener.rs`, `EodhdClient::screener`, `EodhdApi` + `ScriptedClient`, `Products::screener`,
  `src/api/screener.rs`, DTO/handler/route/OpenAPI, `PURPOSE_REQUEST_TYPE`, guarded RequestTypes
  insert, guard G-17, tests.
- Probe P1 first; probe P2 runs here (it is a `/screener` call and may change a later profile
  follow-up, never S4 itself).
- **Done when** a 5-row US screen returns the vendor's rows and the ledger shows `/screener` 5
  under caller `mctrader`; an unknown field is 422, not an empty screen; a venue outside US/AU is
  `400 exchange-conflict` or `400 unknown-exchange`, never a silent pick.

### S4 — `POST /profile` (unlocks M5)

- `Products::profiles` with memo, dedupe and bounded fan-out, DTO/handler/route/OpenAPI, guard
  G-18, tests.
- **Done when** `{"symbols":["WDS","BHP"],"country_code":"AU","exchange":"ASX"}` returns two named
  profiles (or one and a `missing` entry) from at most two vendor calls, a repeat inside the hour
  spends nothing, and a symbol with no listing is `found:false`, never an error for the batch.

### S5 — optional `asset_class` on the four existing routes (unlocks M6)

- DTO fields, `parse_strict`, handler forwarding, OpenAPI, INTERFACE/README/WIRING.
- Tests: default shares; each of the six classes; unknown ⇒ 422.
- **Done when** `GET /price/IXIC?country_code=US&asset_class=indices` returns a price where the
  same request without the field returns `404 unpriced`, and every existing request is unchanged.

### S6 — Documentation close-out

- README scope amendment, WIRING, INTERFACE, GUARD_CATALOGUE wording the guards changed, plan file
  deleted per rule 21.
- **Done when** the estate documents the new scope truthfully and `/openapi.json` matches §3.

### Deploy order and rollback

1. `cargo test`, then `cargo build --release`.
2. Stop/start the NSSM service **with elevation** (the 8003 service keeps the old binary
   otherwise). Prove the build first on a temporary `MDS_PORT` using `run.ps1` — the service
   restart is the last step, not the test.
3. Exercise the new route by hand (probe P6) before telling the McTrader half it may land.
4. **Rollback:** the previous binary runs against the new schema (all DDL is guarded and additive);
   `git revert` + rebuild + elevated restart. The owned tables are left in place and harmless.

Deployment order for the McTrader half: S1 before M1, S2 before M2, S3 before M3 (and M4),
S4 before M5, S5 before M6.

---

## 11. Probes

| # | When | What | Expected |
|---|---|---|---|
| P1 | before S3 | `SELECT … FROM dbo.RequestTypes WHERE RequestType IN (…)` on `Market_Data` | `/screener` 5 `flat`, `/intraday/` 5 `per_call`, both bulk rows 100 `flat`; the guarded INSERT covers an absent `/screener` |
| P2 | within S3 | one `/screener` call with `[["code","in",["WDS","BHP"]],["exchange","=","AU"]]` (5 units) | two rows, or the recorded decision that the per-symbol profile fan-out is final |
| P3 | within S4 | one `/screener` call pinned to `code`/`exchange` for WDS | records `currency_symbol`'s value form; the three-uppercase-letter rule stands either way |
| P4 | before S1 | `SELECT COUNT(*), MAX(BarTimeUtc) FROM dbo.Bar5Min` and `BarHourly` (read-only) | records the tape's depth/freshness; expected stale, confirming §5.2 |
| P5 | after S2 | two `/bars/bulk` calls for the same date | second is `cache`; ledger unchanged; a two-date miss costs 200 |
| P6 | after each slice | the new route against the live service | the §3 example shape, `provider` correct |
| P7 | before S3 | grep every live broker row, watchlist and stored `Screener_Settings` for a venue outside US/AU | none; if one exists, record it and widen §0.1's scope decision before S3 ships |
| P8 | before S2 | `SELECT COUNT(*) FROM dbo.DailySnapshot WHERE SnapshotDate = <latest session>` (read-only) | records how partial the latest session is; justifies the completeness marker |

Probes run against the live database are **read-only** (`market_data` credentials; use
`create_db_engine` for McTrader-side probes). A probe that would write is run on a disposable copy
first.

---

## 12. Risks and the facts that gate them

1. **The field vocabulary is duplicated.** MDS's Rust table and McTrader's
   `eodhd_field_reference.py` are two lists by design (transport validation vs policy). Each pins
   the 14 names in a test, so a value added on one side only fails loudly. The alternative — shape
   validation only — makes every caller typo a vendor round-trip and a `422 vendor-rejected` whose
   message names no field.
2. **Venue scope is US/AU.** A venue outside the two markets is `400 exchange-conflict` (it
   resolves to another market) or `400 unknown-exchange` (it resolves to neither), even though
   McTrader's table can name it. Probe P7 proves no live caller uses one; if that changes, adding
   a country is a deliberate change across `COUNTRIES`, the warehouse and the schedules.
3. **`code in` on the screener is unproven** (P2). The initial `/profile` does not depend on it:
   one pinned call per symbol, capped at 25, memoised. A later batch is a follow-up, not a slice
   prerequisite.
4. **Profile spend scales with batch size.** ≤125 units per 25-symbol pass, one-hour memo,
   dedupe before fan-out, and the 25-symbol cap. The ledger is the monitor.
5. **A hung vendor stalls a batch.** Worst case about 300 s at 25 symbols with concurrency 5, a
   30 s per-call timeout and the one transport retry. The caller's client timeout must exceed it;
   McTrader's is 330 s. Stated, not hidden.
6. **Vendor 422 vs 502.** A bad filter must not read as a vendor outage. The mapping is explicit
   in §3.1 and tested.
7. **Aggregation changes the 4H bars.** Session anchoring fixes a real defect (buckets anchored to
   query time, overnight straddle). McTrader's M1 tests pin the new rule and the live path is
   watched for a cycle. This is the one intentional semantic change in the plan.
8. **Intraday spend.** 5 units per symbol per fetch; a refetch storm is the largest tariff risk in
   this estate. Controls: TTL 300s, `allow_fetch:false` on every caller that only needs a cached
   answer, `fetched` as evidence, and the ledger. The plan does not add a per-caller budget — the
   ledger is the monitor.
9. **`Bar5Min` is in the estate.** This plan deliberately does not read it (§5.2). If a future
   reader wants it, the freshness gate and the tape's provenance must be solved first.
10. **Bulk completeness marker.** Without it a partial latest session is served as whole. The
    marker is written only after the vendor call returns; a crash between upsert and marker leaves
    the session marked absent, which is the safe direction (a refetch, not a wrong answer).
11. **`latest` bulk session and the partial-session window.** The service resolves `latest` via
    `latest_completed_session`, the same rule `/bars` uses; a session the vendor is still writing is
    returned with its key in `partial` and is never served as complete.
12. **DDL on a live database.** All statements are guarded and idempotent; `TableDoc`/`ColumnDoc`
    rows are insert-if-absent (`IF NOT EXISTS … INSERT`), so an existing curated row is left alone. The service's own schema repair runs them on start, as it does today. Take a
    backup of `Market_Data` before S1/S2 land (both add tables).
13. **Elevated restart.** NSSM keeps the old binary until elevated; proving on a temporary port
    first is mandatory (memory: *MMDS restart needs elevation*). Never leave the live port running
    the new code untested.
14. **Other callers.** Mini-McTrader and PortfolioAdvisor share the service. The four routes are
    new and the asset-class field is optional, so nothing they send changes meaning. Announce the
    routes rather than let another repo copy a client.

---

## 13. Out of scope

- `GET /symbol`, `GET /fx`, `GET /quote` — Yahoo/Binance; they stay in the umbrella plan.
- MCP tools for any new route.
- Any scoring, ranking, floor or post-filter policy (stays in McTrader).
- Fundamentals beyond the profile fields the screener carries; `/fundamentals` is not entitled and
  is not proxied.
- Markets outside US/AU.
- News, streaming, order-book depth, tick history.
- Crypto/forex intraday; the umbrella plan's S2 owns the intraday leg and S8 the crypto quotes.
- Backfilling `Bar5Min`/`BarHourly` or changing the estate collector.

---

## 14. Plan lifecycle

This plan is deleted when S1–S6 are live and the McTrader companion's guard test is green. If any
slice is deferred, it moves into `TODO/Loose-Ends.md` as its own `********************************`
section with the probe that gates it, and this document is deleted with the remainder named there
(rule 21).
