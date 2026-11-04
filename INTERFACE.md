# McTrader_Market_Data_Service — interface reference

Everything needed to call this service. No rationale, no history: request and response shapes,
error codes, and the traps that produce wrong code if ignored.

- **Base URL:** `http://127.0.0.1:8003`
- **Auth:** none. Localhost only.
- **Content type:** `application/json` on every POST.
- **OpenAPI:** `GET /openapi.json`, browsable at `GET /docs`
- **Implementation:** Rust crate `mmds`; binaries `mmds-service`, `mmds-daily`, `mmds-mcp` and
  `mmds-tool`

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
instrument a number belongs to. Crypto has no market: `asset_class: "crypto"` is cached under
`CRYPTO`; the country is still required by the request shape but is ignored, and a stated
`exchange` is ignored with it.

---

## 1. `GET /price/{symbol}`

Latest price for one symbol.

**Query parameters**

| Name | Type | Default | Notes |
|---|---|---|---|
| `country_code` | string | **required** | `US` or `AU`. The market the ticker is in — `CBA` is both an ASX and a US listing. Missing or unknown is a 422 |
| `exchange` | string | none | Optional second spelling of the same fact: `US`, `AU`/`ASX`, `L`/`LSE`, `TO`, `HK`, `F`, `PA`. Must agree with `country_code`. Ignored for crypto |
| `asset_class` | string | `shares` | `shares`, `crypto`, `indices`, `commodities`, `rates`, `forex`. Chooses the provider. |
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
{ "symbols": ["AAPL", "MSFT"], "country_code": "US", "asset_class": null, "max_age_seconds": 300 }
```

`country_code` is required; `exchange` is an optional spelling of the same fact and must agree
with it (§Choosing the market). The market used is echoed as `exchange` in the response.

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
  "asset_class": null,
  "price": 343.83,
  "multiplier": 1.0,
  "direction": "long"
}
```

| Field | Default | Notes |
|---|---|---|
| `country_code` | **required** | `US` or `AU` — the market the series is measured on. See §Choosing the market |
| `exchange` | none | optional second spelling of the same fact; must agree with `country_code` |
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
{ "symbols": ["AAPL"], "sessions": 15, "country_code": "US", "asset_class": null, "allow_fetch": true }
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

**The session in progress is already excluded.** Do not filter it again downstream.

**Errors:** 400 `bad-sessions`, `no-symbols`, `unknown-exchange`, `exchange-conflict`, `mixed-exchanges`;
422 `country_code` missing or unknown; 502 vendor.

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

| `asset_class` | Order |
|---|---|
| `shares` (default) | EODHD → Yahoo |
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
   `400 exchange-conflict`), and a missing country is a `422` rather than a US default. Crypto
   pairs are bare or dashed (`BTC-USD`); the country is required but ignored for crypto.
7. **Crypto needs `asset_class: "crypto"`**, or it is treated as a share and priced from the wrong feed.
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

def price(symbol, country_code, asset_class="shares"):
    r = requests.get(f"{BASE}/price/{symbol}",
                     params={"country_code": country_code, "asset_class": asset_class},
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
