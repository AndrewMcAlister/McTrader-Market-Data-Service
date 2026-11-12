# McTrader_Market_Data_Service — interface reference

Everything needed to call this service. No rationale, no history: request and response shapes,
error codes, and the traps that produce wrong code if ignored.

- **Base URL:** `http://127.0.0.1:8003`
- **Auth:** none. Localhost only.
- **Content type:** `application/json` on every POST.
- **OpenAPI:** `GET /openapi.json`, browsable at `GET /docs`
- **Implementation:** Rust crate `mmds`; binaries `mmds-service`, `mmds-daily`, `mmds-mcp` and
  `mmds-tool`

## Routes

| Route | Returns | Format |
|---|---|---|
| `GET /price/{symbol}` | one symbol's latest quote | REST |
| `POST /prices` | quotes for a batch — **also the whole-market call** | REST |
| `POST /atr` | ATR and an optional stop level | REST |
| `POST /bars` | completed **daily** bars | REST |
| `POST /bars/intraday` | 5-minute bars and their 15m/30m/1h/4h aggregates | REST |
| `POST /bars/bulk` | whole-market closes for one to ten sessions | REST |
| `POST /screener` | raw vendor screener rows for one market | REST |
| `POST /profile` | company profile for up to 25 listings | REST |
| `GET /todays-data-is-available`, `GET /todays-big-movers` | today's observation, ranked movers | REST |
| `GET /market/*`, `GET /trading_sessions`, `GET /schedule`, `POST /schedule/run` | the market clock and the collection schedule | REST |
| `POST /refresh`, `POST /secrets/eodhd`, `GET /stats`, `GET /healthz`, `POST /admin/shutdown` | operations | REST |
| `/mcp` (separate binary, optional) | the original four products as MCP tools | MCP |

The four new routes are **REST only**: they are not exposed as MCP tools, because they are
transport for a service-to-service caller rather than a surface to invite a model to drive.

---

## Choosing the market — every symbol endpoint requires a country code

A ticker is not unique across markets: `CBA` is an ASX listing **and** a US listing, with two
prices, two daily series and two ATRs. So every endpoint below that takes a symbol also
**requires** `country_code`, `US` or `AU` (case-insensitively): a query parameter on `GET`, a
body field on `POST`. A missing country is `422` (`"type": "missing"`); an unknown one is `422`
(`"type": "literal_error"`) naming the codes the service serves. There is no default — a ticker
alone does not name a market, and answering it from the US tape is exactly the silent wrong
number this field exists to prevent.

`exchange` is an optional second spelling of the same fact, for a caller that already knows the
provider's exchange code: `US`,
`AU`/`ASX`/`AX`, `L`/`LSE`/`UK`, `TO`/`TSX`, `HK`, `F`/`DE`, `PA`. If stated it must agree with
`country_code`; two spellings naming different markets is `400 exchange-conflict`. A symbol may
also carry its market as a suffix (`CBA.AU`, `VOD.L`, `AAPL.US`), which must likewise agree
with the stated country. A bare symbol takes the stated country.

All of these are valid and mean the ASX listing:

```json
{"symbols": ["CBA"], "country_code": "AU"}
{"symbols": ["CBA"], "country_code": "AU", "exchange": "ASX"}
{"symbols": ["CBA.AU"], "country_code": "AU"}
```

The third still names the country: the suffix corroborates it, it does not replace it —
`{"symbols": ["CBA.AU"]}` with no `country_code` is a `422`.

**One request is one market**: a batch whose symbols carry two different suffixes is
`400 mixed-exchanges`. Split it into two calls.

The market that was used comes back on every answer (`exchange` on `/prices` and `/atr`,
per-symbol on `/bars`, and per-quote on `/price`), so a caller never has to assume which
instrument a number belongs to.

**`asset_class` is optional on the four symbol routes.** Absent means `shares`, which is what
every existing caller means and is unchanged. Present and recognised means that class:

| Class | Accepted spellings |
|---|---|
| `shares` | `shares`, `equities`, `equity` |
| `crypto` | `crypto`, `cryptocurrency` |
| `indices` | `indices`, `indicies`, `index` |
| `commodities` | `commodities`, `commodity`, `futures`, `future` |
| `rates` | `rates`, `rate`, `bonds` |
| `forex` | `forex`, `fx`, `currency` |

A spelling outside that list is `422 unknown-asset-class`, and the `detail` names the accepted
list. This is strict on purpose: the library's internal parser folds an unknown spelling to
`shares`, and at the HTTP boundary that fold is a silent reclassification — an index priced as a
share is a different instrument, which is why `IXIC`, `GSPC`, `GLD`, `SLV`, `BNO`, `UUP` and
`US10Y` came back `unpriced` while the field was unstateable.

---

## 1. `GET /price/{symbol}`

Latest price for one symbol.

**Query parameters**

| Name | Type | Default | Notes |
|---|---|---|---|
| `country_code` | string | **required** | `US` or `AU`. The market the ticker is in — `CBA` is both an ASX and a US listing. Missing or unknown is a 422 |
| `exchange` | string | none | Optional second spelling of the same fact: `US`/`NYSE`/`NASDAQ`/`AMEX`/`NYSEARCA`/`ARCA`/`BATS`/`IEX`, `AU`/`ASX`/`AX`/`AUS`. Must agree with `country_code` |
| `asset_class` | string | `shares` | Optional. `indices` for `IXIC`/`GSPC`, `commodities`/`rates` for the ETF proxies and rates. Unknown spelling is `422 unknown-asset-class` (§Choosing the market) |
| `max_age_seconds` | float | `300` | **Lowers** the cache window; cannot raise it. |

`country_code` is required: `GET /price/AAPL?country_code=US` is the shape. `exchange` is
optional and must agree with it; a disagreement — or a symbol suffix that contradicts it — is
`400 exchange-conflict`.

**200 response**

```json
{
  "symbol": "AAPL",
  "exchange": "US",
  "price": 343.83,
  "previous_close": 347.10,
  "change_pct": -0.9421,
  "bid": null,
  "ask": null,
  "provider": "eodhd",
  "fetched_at": "2026-10-29T04:34:16.685844",
  "age_seconds": 0.381,
  "as_of": "2026-10-28T20:29:00",
  "as_of_age_seconds": 25424.31
}
```

**Errors**

| Status | Body `error` | Meaning |
|---|---|---|
| 400 | `unknown-exchange`, `exchange-conflict` | the market could not be resolved — see §Choosing the market |
| 404 | `unpriced` | nothing could price it |
| 422 | FastAPI-shaped `detail` | `country_code` missing or not `US`/`AU` |
| 422 | `unknown-asset-class` | `asset_class` is not a recognised spelling |
| 502 | vendor reason | the vendor refused |
| 503 | `vendor-unavailable` | the service has no vendor client configured |
| 503 | `database-busy` | transient lock; **retry** |

`bid`/`ask` are always `null` — the vendor has no quote side.
`age_seconds` is our cache age; `as_of_age_seconds` is the vendor's observation age, which can be
hours. Gate freshness on `age_seconds`.

---

## 2. `POST /prices`

Latest prices for many symbols in one call. **This is also the whole-market call** — there is no
separate endpoint.

**Request**

```json
{ "symbols": ["AAPL", "MSFT"], "country_code": "US", "max_age_seconds": 300 }
```

`country_code` is required; `exchange` is an optional spelling of the same fact and must agree
with it (§Choosing the market). `asset_class` is optional and applies to the whole batch. The
market used is echoed as `exchange` in the response.

**200 response**

```json
{
  "quotes": {
    "AAPL": { "symbol": "AAPL", "exchange": "US", "price": 343.83, "previous_close": 347.10, "change_pct": -0.9421,
              "bid": null, "ask": null, "provider": "eodhd",
              "fetched_at": "2026-10-29T04:34:16", "age_seconds": 0.4,
              "as_of": "2026-10-28T20:29:00", "as_of_age_seconds": 25424.3 }
  },
  "exchange": "US",
  "fetched": ["AAPL"],
  "missing": [],
  "deferred": [],
  "complete": true,
  "route": "per_symbol",
  "requested": 2,
  "count": 2
}
```

| Field | Meaning |
|---|---|
| `route` | `per_symbol` or `bulk` — which vendor path was used, chosen from the list length |
| `missing` | symbols the vendor could not price. **Not cached**, so a later call retries them |
| `deferred` | symbols not reached before the deadline; ask again |
| `complete` | `false` when anything is missing or deferred |

**The route is automatic and it matters:** the vendor bills 1 unit per symbol, or 100 units flat for
the whole US market. Below 100 symbols per-symbol is cheaper; at 100 or more, bulk wins by up to 100×.
Send the list you need and let the service decide.

**Errors:** 400 `no-symbols`, `unknown-exchange`, `exchange-conflict`, `mixed-exchanges`; 422
`country_code` missing or unknown; 502 vendor; 503 `database-busy`.

---

## 3. `POST /atr`

Average True Range, from the precomputed `IndicatorCache` when it can answer (costs no vendor call),
else computed from daily bars.

**Request**

```json
{
  "symbols": ["AAPL"],
  "period": 14,
  "method": "tr",
  "smoothing": "simple",
  "country_code": "US",
  "price": 343.83,
  "multiplier": 1.0,
  "direction": "long"
}
```

| Field | Default | Notes |
|---|---|---|
| `country_code` | **required** | `US` or `AU` — the market the series is measured on. See §Choosing the market |
| `exchange` | none | optional second spelling of the same fact; must agree with `country_code` |
| `asset_class` | `shares` | optional; the class the whole batch is measured as (§Choosing the market) |
| `period` | `14` | |
| `method` | `tr` | `tr` = gap-inclusive true range (the standard); `high_low` = High−Low only |
| `smoothing` | `simple` | `simple` = mean of the last N ranges (the standard); `wilder` |
| `price` | none | supply **with** `multiplier` to also get `stop_level` |
| `multiplier` | **none** | **never defaulted** — the books use 1.0, 2.0 and 3.0 |
| `direction` | `long` | `long` places the stop below, `short` above |

**200 response**

```json
{
  "atr": {
    "AAPL": {
      "symbol": "AAPL", "value": 6.18285, "percent": 1.7982, "reference_price": 343.83,
      "period": 14, "method": "tr", "smoothing": "simple",
      "sessions_used": 51, "as_of_session": "2026-10-28",
      "source": "indicator_cache", "reason": null,
      "stop_level": 337.64715
    }
  },
  "exchange": "US",
  "period": 14, "method": "tr", "smoothing": "simple", "complete": true
}
```

| Field | Meaning |
|---|---|
| `value` | ATR in **price units**; `null` when history is insufficient |
| `percent` | `value / reference_price * 100` — percent points (1.7982 means 1.80%) |
| `reference_price` | the close the ATR was measured against |
| `source` | `indicator_cache`, `computed`, or `unavailable` |
| `reason` | why `value` is `null` |
| `stop_level` | present only when `price` and `multiplier` were both supplied |

`country_code` chooses the series the ATR is measured on: `IndicatorCache` is keyed by
`(symbol, exchange)`, so an ASX `CBA` and a US `CBA` have separate ATRs and neither is served
for the other. The market used is echoed as `exchange` at the top of the response.

**The ATR definition, exactly:**

```
TR      = max(High - Low, |High - PreviousClose|, |Low - PreviousClose|)
ATR(n)  = sum(TR for the last n bars) / n
```

**Errors:** 400 `bad-method`, `bad-smoothing`, `bad-period`, `incomplete-stop` (price without
multiplier), `no-symbols`, `unknown-exchange`, `exchange-conflict`, `mixed-exchanges`; 422
`country_code` missing or unknown; 502 vendor.

---

## 4. `POST /bars`

Completed daily bars.

**Request**

```json
{ "symbols": ["AAPL"], "sessions": 15, "country_code": "US", "allow_fetch": true }
```

**200 response**

```json
{
  "bars": {
    "AAPL": {
      "symbol": "AAPL", "exchange": "US",
      "as_of_session": "2026-10-28",
      "sessions": 15,
      "adjusted": true,
      "provider": "cache",
      "source": "cache",
      "rows": [
        { "d": "2026-10-08", "o": 250.1, "h": 254.9, "l": 249.2, "c": 253.7, "cr": 253.9,
          "v": 51200000 }
      ]
    }
  },
  "fetched": [], "short_history": [], "complete": true, "requested_sessions": 15
}
```

| Field | Meaning |
|---|---|
| `as_of_session` | the last completed session in `rows` |
| `adjusted` | whether `c` is split-adjusted |
| `cr` | the **unadjusted** close; `null` means not known (a `BarHourly` seed, or a row the 5-minute tape does not cover). Candle bodies and order prices read this, never `c` on a series that says adjusted. Filled in bulk from `Bar5Min` by `jobs.daily --fill-raw-close`, and per symbol by the `/bars` refresh |
| `provider` | `cache` or the provider that answered (`eodhd`, `yahoo`, `binance`) |
| `short_history` | symbols with fewer sessions than asked |

A cache hit requires a raw close on every served row. If any is missing the service refreshes
that symbol from the vendor once — one unit whatever the window — and the response's `fetched`
names it. `allow_fetch: false` never refreshes, so a caller that wants raw closes must allow
the fetch.

A refresh also fills **depth**: while a symbol's stored series is shorter than
`MDS_HISTORY_SESSIONS` (560 sessions, about two years), the request asks for that whole horizon
and stores all of it. The same one unit buys it, so `sessions` only controls what is returned,
not what is kept. A later request for more sessions is then answered from the database instead
of the vendor — which is what makes support and resistance levels computable.

`country_code` chooses the series: the cache is keyed by `(symbol, exchange)`, so `AAPL`
at `AU` is a different instrument from `AAPL` at `US`. An exchange with a short series answers
`short_history` rather than borrowing the other exchange's bars.

`allow_fetch: false` answers only from the database and never calls a vendor.

`asset_class` is optional and applies to the whole batch; absent means `shares`.

**The session in progress is already excluded.** Do not filter it again downstream.

**Errors:** 400 `bad-sessions`, `no-symbols`, `unknown-exchange`, `exchange-conflict`, `mixed-exchanges`;
422 `country_code` missing or unknown; 422 `unknown-asset-class`; 502 vendor.

---

## 4a. `POST /bars/intraday` — 5-minute bars and their aggregates

```json
{"symbols": ["TTD"], "country_code": "US", "timeframe": "4Hour", "limit": 48, "allow_fetch": true}
```

| Field | Default | Notes |
|---|---|---|
| `symbols` | **required** | one or more; one request is one market |
| `timeframe` | **required** | `5Min`, `15Min`, `30Min`, `1Hour`, `4Hour`, or `5m`/`15m`/`30m`/`1h`/`4h`. Case-insensitive. `1Day`/`1Week` are not intraday — use `POST /bars` |
| `limit` | `78` | buckets to return, 1..500. The default is one US session of 5-minute bars |
| `allow_fetch` | `true` | `false` serves the owned tape only and never calls a vendor |
| `country_code` | **required** | `US` or `AU` (§Choosing the market) |
| `exchange` | none | optional second spelling; must agree with `country_code` |

**200 response**

```json
{
  "exchange": "US",
  "provider": "eodhd",
  "fetched": ["TTD"],
  "short_history": [],
  "bars": {
    "TTD": {
      "symbol": "TTD", "exchange": "US",
      "as_of_session": "2026-11-12",
      "sessions": 2,
      "adjusted": false,
      "provider": "eodhd", "source": "eodhd",
      "rows": [{"d": "2026-11-12T09:30:00", "o": 231.4, "h": 233.1,
                "l": 230.9, "c": 232.7, "v": 1200300, "cr": null}]
    }
  }
}
```

The series shape is `POST /bars`'s, so one decoder serves both. The differences, stated:

- **`d` is an exchange-local ISO timestamp, not a session date.** An intraday bar is a moment.
  A 78-bar US session therefore starts at `09:30:00` on the session's own date.
- **`adjusted` is always `false`.** The vendor's intraday tape is unadjusted and carries no
  separate raw print, so `cr` is always `null`.
- **`provider` is `eodhd` when anything was fetched and `cache` when the owned tape answered every
  symbol.** `fetched` is the spend evidence: 5 units per symbol fetched, nothing for a cache hit.

**The buckets are anchored to the session, not to the series.** Each exchange-local session date
is chunked consecutively from that session's first bar: 1 native bar per `5Min` bucket, 3 per
`15Min`, 6 per `30Min`, 12 per `1Hour`, 48 per `4Hour`. A short final bucket is kept. This means
a 4-hour bar is stable across requests and never straddles the overnight gap — unlike chunking
from an arbitrary `now - window_days` start, where the same candle changes shape as the clock
advances.

The tape lives in the service's own table (`IntradayBar5Min`, native 5-minute rows only) and is
refreshed when it is older than `MDS_INTRADAY_TTL_SECONDS` (300) or cannot supply `limit`
buckets. Rows older than `MDS_INTRADAY_RETENTION_DAYS` (30) are pruned.

**Errors:** 400 `no-symbols`, `bad-limit`, `unknown-exchange`, `exchange-conflict`,
`mixed-exchanges`; 422 FastAPI-shaped for a missing/unknown `timeframe`, a `limit` outside 1..500,
or a missing/unknown `country_code`; 502 vendor; 503 `vendor-unavailable` (no key),
`database-unavailable`, `database-busy`.

---

## 4b. `POST /bars/bulk` — whole-market closes for several sessions

```json
{"market": "US", "dates": ["latest", "2026-08-13"], "allow_fetch": true}
```

| Field | Default | Notes |
|---|---|---|
| `market` | `US` | `US` or `AU` — the whole-market pull's own word, the same one `POST /refresh` uses |
| `dates` | **required** | 1..10 distinct entries, each `"latest"` or `YYYY-MM-DD`. `"latest"` resolves through the same completed-session rule as `POST /bars` |
| `allow_fetch` | `true` | `false` returns `404 not-pulled` rather than answering from an unmarked session |

**200 response**

```json
{
  "market": "US",
  "provider": "cache",
  "partial": [],
  "dates": {
    "latest":    {"session": "2026-11-11", "rows": {"AAPL": {"c": 231.4}, "MSFT": {"c": 512.7}}},
    "2026-08-13": {"session": "2026-08-13", "rows": {"AAPL": {"c": 218.9}}}
  }
}
```

`c` is the **adjusted** close: return windows are computed from the adjusted series. The served
`dates` object is keyed by the requested key, so `"latest"` stays `"latest"` and its `session`
names the session the service resolved.

**A cached session is served only when a completed pull is recorded for it.** The latest session
in the daily table can be partial — 1,881 rows against ~27,714 — and a partial set silently
changes every derived return that reads it. So:

- a **completed** pull is marked and every later read of it is free and complete;
- a pull **materially below** the previous completed session's row count (or empty) is upserted but
  **unmarked**, and its requested key is listed in `partial` on that response only;
- `allow_fetch: false` on an unmarked session is `404 not-pulled`;
- a failed pull writes no marker, so the next request re-pulls rather than serving a wrong answer.

A dated pull returns delisted listings too (~50,270 codes against ~27,714) because filtering is
policy and stays with the caller. Intersect a dated set with a live listing set before computing a
market-wide statistic.

**Cost: 100 units per dated pull**, filed under `/eod-bulk-last-day/US` or `/AU`. A cache hit is
free.

**Errors:** 400 `unknown-exchange`; 404 `not-pulled`; 422 FastAPI-shaped for an empty or >10-date
`dates` list, a malformed date, or a `market` outside `US`/`AU`; 502 vendor; 503 vendor/database
unavailable.

---

## 4c. `POST /screener` — raw vendor rows for one market

```json
{"country_code": "US",
 "filters": [["market_capitalization", ">", 300000000], ["refund_5d_p", ">", 5]],
 "sort": "refund_5d_p.desc", "limit": 50, "offset": 0}
```

| Field | Default | Notes |
|---|---|---|
| `country_code` | **required** | `US` or `AU` |
| `exchange` | none | optional venue spelling; also decides which venue is sent (see below) |
| `filters` | `[]` | up to 64 `[field, operator, value]` triples |
| `sort` | vendor default | `<field>.<asc\|desc>`; the direction is **required** |
| `limit` | `100` | 1..500 |
| `offset` | `0` | ≥ 0 |

**The 14 fields** — the only ones the vendor filters and sorts on:

```
code, name, exchange, sub_exchange, sector, industry, market_capitalization,
earnings_share, dividend_yield, adjusted_close, avgvol_1d, avgvol_200d,
refund_1d_p, refund_5d_p
```

- numeric fields (`market_capitalization`, `earnings_share`, `dividend_yield`, `adjusted_close`,
  `avgvol_1d`, `avgvol_200d`, `refund_1d_p`, `refund_5d_p`) take `=`, `!=`, `>`, `<`, `>=`, `<=`;
- text fields (`code`, `name`, `exchange`, `sub_exchange`, `sector`, `industry`) take `=` and `in`
  only;
- `in` takes an array and is refused on a numeric field (the vendor answers HTTP 500 for it);
- **derived fields** (`return_3m_p`, `return_100d_p`, `return_200d_p`) are refused: read closes
  from `POST /bars/bulk` and compute them there;
- a **rejected** field (`country`, `pe_ttm`, `refund_1m_p`, `currency_symbol`, …) is refused as not
  a screener field, with the guidance the vendor's own trap list records;
- a **GICS-style sector spelling** (`Information Technology`, `Health Care`) is refused with the
  spelling EODHD holds — otherwise it answers 200 with zero rows.

**The venue travels in `filters`.** The vendor ignores a bare top-level `exchange`, so the service
lifts every agreeing `exchange` filter and appends exactly one `["exchange", "=", venue]` triple
last. `<venue>` is, in order: the requested `exchange`; else the single agreeing filter value that
was lifted; else the resolved market. **The caller's spelling is what travels**, so a per-venue
`NASDAQ` screen stays per-venue rather than becoming a US-wide one. A venue outside `US`/`AU`
resolves to another market (`400 exchange-conflict`) or to nothing (`400 unknown-exchange`).

**200 response** — `count` and `rows`, plus the query as sent:

```json
{"provider": "eodhd", "country_code": "US", "exchange": "US",
 "filters": [["market_capitalization", ">", 300000000], ["refund_5d_p", ">", 5],
             ["exchange", "=", "US"]],
 "sort": "refund_5d_p.desc", "limit": 50, "offset": 0, "count": 50,
 "rows": [{"code": "AAPL", "exchange": "US", "name": "Apple Inc", "sector": "Technology"}]}
```

`rows` are the vendor's objects **untouched** — field names, `null`s and columns this service does
not know pass through. No normalisation, no filtering, no `symbol` added. **An empty result is a
200 with `count: 0`**: "no company matched" is an answer, not an error.

**No cache.** A screen is a question about now.

**Cost: 5 units flat per call.**

**Errors:** 400 `exchange-conflict`, `mixed-exchanges`, `unknown-exchange`; 422 `unknown-field`,
`unsupported-operator`, `unsupported-filter`, `derived-field`, `sector-trap`, `invalid-sort`,
`vendor-rejected` (the vendor refused the query — the caller's mistake, not an outage), or
FastAPI-shaped for a bad bound or a missing/unknown `country_code`; 502 vendor; 503
`vendor-unavailable`.

---

## 4d. `POST /profile` — company profile for up to 25 listings

```json
{"symbols": ["WDS", "BHP", "CBA"], "country_code": "AU", "exchange": "ASX"}
```

`symbols` is 1..25 (the batch cost bound); `country_code` is required and `exchange` optional,
resolved exactly as for `POST /bars`, so suffixes and the one-market rule apply.

**200 response**

```json
{
  "provider": "eodhd", "country_code": "AU", "exchange": "AU",
  "requested": 3, "complete": false, "missing": ["CBA"],
  "profiles": {
    "WDS": {"symbol": "WDS", "exchange": "AU", "found": true,
            "name": "Woodside Energy Group Ltd", "sector": "Energy",
            "industry": "Oil & Gas E&P", "market_cap": 51234567890.0,
            "currency": "AUD", "country": null, "gic_sector": null,
            "gic_industry": null, "description": null,
            "as_of": "2026-11-11", "error": null},
    "CBA": {"symbol": "CBA", "exchange": "AU", "found": false, "...": "every field null"}
  }
}
```

| Field | Source | When absent |
|---|---|---|
| `symbol` | the requested root, upper-cased | never null |
| `exchange` | the resolved market code (`US`/`AU`), never a venue name | never null |
| `found` | a vendor row matched on `code` | `false` |
| `name`, `sector`, `industry` | the vendor row (Yahoo-style sectors, **not GICS**) | null |
| `market_cap` | `market_capitalization` (absolute USD) | null |
| `currency` | `currency_symbol`, only when it is exactly three upper-case letters | null |
| `as_of` | `last_day_data_date` | null |
| `country`, `gic_sector`, `gic_industry`, `description` | **no entitled source** | always null |
| `error` | the vendor reason when *this symbol's* call failed | null |

`dividend_yield` is deliberately absent: the vendor reports a fraction where a percentage is
usually expected, and that is a unit decision for the caller, not a field.

A symbol with no listing is `found: false` and appears in `missing` — **not** an error for the
batch, and a single failed call is that symbol's `error`, never the batch's. `complete` is true
when nothing is missing and no entry carries an error.

**Cost: 5 units per uncached symbol** (one pinned vendor call each), so a 25-symbol pass is at
most 125. Profiles are memoised in-process for 3600 seconds, keyed by symbol and market; a miss and
a failed call are never memoised. Worst case for a 25-symbol pass is about 300 seconds (five waves
at concurrency five, each call bounded by 30 s plus one transport retry), so **set a client timeout
above that** — 330 s or more.

**Errors:** 400 `unknown-exchange`, `exchange-conflict`, `mixed-exchanges`; 422 FastAPI-shaped for
an empty or over-25 symbol list, or a missing/unknown `country_code`; 503 `vendor-unavailable`.

---

## 5a. `POST /refresh` — pull the whole market now

```json
{"market": "US", "session_date": null, "force": false, "include_indicators": true}
```

| Field | Default | Notes |
|---|---|---|
| `market` | `US` | only US is measured |
| `session_date` | `null` | `YYYY-MM-DD` for a dated backfill; bypasses the freshness check |
| `force` | `false` | pull even though a bulk pull is already recorded today |
| `include_indicators` | `true` | rebuild `IndicatorCache` afterwards |

| Status | HTTP | Meaning |
|---|---|---|
| `refreshed` | **202** | the pull ran; body carries sessions, rows, units, duration |
| `already-fresh` | 200 | a bulk pull is recorded today; **spent nothing**, returns in ms |
| `busy` | 409 | another refresh holds the lock; ask again later |
| `no-vendor-key` | 503 | no EODHD key configured |

**Cost: 100 units and about 30 seconds.** That is one flat-priced `/eod-bulk-last-day` call, plus the
ATR rebuild (~2,866 symbols). Pull-only is about 10 seconds.

**It is idempotent** — the pull upserts by (symbol, session), so running it twice is harmless — and it
**does not double-spend**: a non-blocking lock makes the second concurrent caller `409 busy`.

`expected_session` and `behind_expected` are the useful fields for monitoring. `expected_session` is
what `data_service.sessions` says the exchange's clock allows; `behind_expected: true` means the vendor
returned an older session, i.e. lag or a holiday — visible rather than silent.

**Send `X-MarketData-Caller`.** The 100 units land in the spend ledger against it, which is the only
way to tell an automatic refresh from a manual one later.

---

## 5. Operations

| Endpoint | Purpose |
|---|---|
| `GET /healthz` | `{"status","version","vendor_configured","key_source","database","database_ok","database_error"}` |
| `GET /stats` | counters, process vendor units, row counts, and **`usage_today`** from the spend ledger |
| `POST /admin/shutdown` | stop the process from inside it — what `stop.bat` calls |
| `GET /docs` | OpenAPI UI |

### Tell it who you are: `X-MarketData-Caller`

Every data endpoint records the vendor spend it caused against this header, falling back to the path
when it is absent:

```powershell
Invoke-RestMethod http://127.0.0.1:8003/price/AAPL -Headers @{ 'X-MarketData-Caller' = 'minictrader' }
```

It costs nothing and makes the shared quota accountable. Send it:

```
caller        request type   calls  symbols  cost
mctrader      /real-time/        1        1     1
minictrader   /real-time/        1        1     1
http:/price   /real-time/        1        1     1     <- no header, attributed by path
```

Without it the row still lands, attributed to the path. That is deliberate — an unattributed call is
a question worth asking, whereas a call that vanished answers nothing.

**Spend is priced from `dbo.RequestTypes`, not from the code.** The four bases behave differently and
getting one wrong is silent: `per_symbol` multiplies by the batch (3 tickers = 3 units, so batching
saves time and not quota), while `flat` and `per_call` ignore it entirely (the whole-market feed is 100
whether it returns 527 rows or 12,737).

**A failed call is still billed.** A 4xx is recorded before the error is raised, because a rejected
request costs quota — measured: a 404 from `/eod/` cost 1 and a 404 from `/intraday/` cost 5. Only a
transport failure, where the request never landed, is free and unrecorded.

---

## 6. Provider routing — read this before interpreting `provider`

The router orders providers per asset class, and `asset_class` is how a request states one.
Absent, every symbol is treated as `shares`.

| Asset class | Order |
|---|---|
| `shares` (the default) | EODHD → Yahoo |
| `indices`, `commodities`, `rates`, `forex` | EODHD → Yahoo |
| `crypto` | **Binance** → EODHD → Yahoo |

Every quote and bar declares the provider that answered. **Two calls for the same symbol can be
answered by different providers.** That is allowed, but it must be visible: never compute a change
percentage by mixing a price from one provider with a previous close from another.

A provider that cannot serve an asset class is never asked — Binance is never asked for an index.

---

## 7. Traps

1. **`multiplier` is never defaulted.** Omitting it with `price` is a 400, by design.
2. **`value: null` is not an error** — it means insufficient history. Read `reason`.
3. **`percent` is percent points**, not a fraction: `1.80`, not `0.018`. Do not multiply by 100 again.
4. **Do not keep your own cache.** Prices live 300 s here, bars are keyed on the completed session,
   ATR is precomputed. A second cache with its own TTL will disagree after any restart.
5. **`503 database-busy` is retryable.** It means the daily load held a lock; it clears in seconds.
6. **Symbol format:** every request names its market with `country_code` (`US` or `AU`); the
   symbol may be bare (`AAPL`) or already-suffixed (`AAPL.US`). A suffix must agree with the
   stated country (`CBA.AU` with `country_code: "AU"` is ASX data; with `US` it is
   `400 exchange-conflict`), and a missing country is a `422` rather than a US default.
7. **An asset class must be stated when it is not shares.** `asset_class` is optional and absent
   means `shares`; an index, commodity ETF proxy or rate sent as a share is a *different
   instrument*, and the ones whose rendering differs come back `unpriced` (`IXIC.INDX` is not
   `IXIC.US`). State `indices`, `commodities` or `rates`. A spelling the service does not know is
   `422 unknown-asset-class` rather than a silent share.
8. **Whole-market responses are large** (~12,700 rows, ~2 MB). If you only need the top movers, sort
   locally rather than shortening the list — a shorter list can drop below the 100-symbol break-even
   and become *more* expensive.
9. **A ticker is not unique across exchanges.** `CBA` is both an ASX listing and a US one, so the market
   is part of the instrument's identity, not a formatting hint: send `country_code`, and expect
   `short_history` rather than another exchange's bars when the requested exchange has no series.
   One request is one market: a batch with two different suffixes is `400 mixed-exchanges`.

---

## 8. Minimal Python client

```python
import requests

BASE = "http://127.0.0.1:8003"

def price(symbol, country_code):
    r = requests.get(f"{BASE}/price/{symbol}",
                     params={"country_code": country_code},
                     timeout=30)
    r.raise_for_status()
    return r.json()

def atr_with_stop(symbol, price_value, country_code, multiplier=1.0, period=14):
    r = requests.post(f"{BASE}/atr", json={
        "symbols": [symbol], "period": period, "country_code": country_code,
        "price": price_value, "multiplier": multiplier, "direction": "long",
    }, timeout=60)
    r.raise_for_status()
    return r.json()["atr"][symbol]        # -> {"value", "percent", "stop_level", ...}
```

MCP clients use the same four operations as tools: `get_price`, `get_prices`, `get_atr`, `get_bars`,
plus `get_service_status`. `MDS_MCP_TRANSPORT` selects `stdio` (the default) or
`streamable-http`, which serves `/mcp` on `MDS_MCP_HOST`/`MDS_MCP_PORT` (default
`127.0.0.1:8000`). The source's `sse` value is accepted but served as `streamable-http`.
