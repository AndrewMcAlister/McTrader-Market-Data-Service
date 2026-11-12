# Wiring McTrader to McTrader_Market_Data_Service

A checklist for the next step. Read §1 first: it is the one instruction that is easy to get wrong
and expensive to get wrong.

---

## 1. Delete McTrader's cache. All of it.

**Caching belongs in this service.** Once McTrader's external leg points at `:8003`, every cache
left behind in McTrader is a *second* cache for the same data with its own TTL, and the two will
disagree at exactly the moment it matters.

| In McTrader | What happens to it |
|---|---|
| `services/quote_cache.py` (`QuoteCache`) | **delete** — its job is now `SymbolPriceCache` |
| `services/eodhd_price_service.py` (`EodhdPriceService`) | **delete** — its job is `POST /prices` |
| the 90-second bar memo in `market_data_service` | **delete** — `DailySnapshot` is keyed on the completed session, which is a better key than a clock |
| the 600-second negative price memo | **delete** — `missing` symbols are not cached here either, so a retry is already free |
| `BULK_PRICE_ENABLED`, `services/bulk_price_route.py` | **retire** — `POST /prices` picks bulk vs per-symbol itself from the list length |

What must stay in McTrader: `MARKET_DATA_PROVIDER_PRIORITY` (the broker-first policy), the broker
leg, and `EodhdUsage` *reads* (the ledger is still the accounting source).

Why this matters concretely: a 300-second TTL here and a 300-second TTL there do not expire
together. The first request after a restart gets one answer from each, and a stop computed from a
stale price is indistinguishable from a stop computed from a fresh one.

### The EODHD legs that move to this service

Once these four routes are wired, McTrader holds no EODHD key and reaches no EODHD host. The
capability each route carries, and the leg in McTrader that it retires:

| Capability | mMds route | McTrader leg deleted |
|---|---|---|
| Intraday 5m/15m/30m/1h/4h bars | `POST /bars/intraday` | `EODHDClient.get_historical_bars` + `_aggregate_by_count` |
| Whole-market closes for several sessions | `POST /bars/bulk` | `EODHDClient.get_eod_bulk_last_day`, the direct REST pull in `EODHDBulkCloseService` |
| Raw screener rows | `POST /screener` | `EODHDClient.get_screener`, the tracked-query REST leg in `mc_trader_controller`, the hosted MCP screener tools |
| Company profile | `POST /profile` | `EODHDClient.get_screener_profile` (and the `get_fundamentals` call that answers 403) |
| An index/commodity/rate price, bar or ATR | optional `asset_class` on `/price`, `/prices`, `/atr`, `/bars` | the market picture's share-folding workaround |

Rules that come with them, and that the adapter must honour:

- **One request is one market.** `country_code` is required on every symbol route; `POST /bars/bulk`
  names `market` instead. A body naming two markets is refused, never half-served.
- **Send `X-MarketData-Caller: mctrader`.** All four routes bill through the ledger under it, so
  "the screener spend moved" is a row you can read rather than a claim.
- **`POST /profile` caps at 25 symbols** and memoises for an hour in-process. A 25-symbol pass is at
  most 125 units cold and free inside the hour; its worst-case latency is about 300 s.
- **`/screener` is 5 units flat per call.** There is no cache: a screen is a question about now.
- **`/bars/intraday` is 5 units per fetched symbol**, free inside `MDS_INTRADAY_TTL_SECONDS` (300).
  Use `allow_fetch: false` wherever a cached answer is enough — a refetch storm is the largest
  tariff risk in this estate.

---

## 2. The adapter

McTrader cannot run an ABC over HTTP. The pattern is a **6-method proxy** implementing
`IMarketDataProvider`, which forwards to the four endpoints:

```python
class HttpMarketDataProvider(IMarketDataProvider):
    # get_current_price  -> GET  /price/{symbol}
    # get_latest_trade    -> GET  /price/{symbol}
    # get_latest_quote    -> GET  /price/{symbol}
    # get_bars            -> POST /bars
    # is_market_open      -> answered locally (no vendor call, no network hop)
    # get_market_clock    -> answered locally
```

`is_market_open` and `get_market_clock` are calendar logic — do **not** proxy them. Three of the
six methods are answerable without a round trip.

Slot it into `FallbackMarketDataAdapter` as the external leg, replacing the direct `EODHDClient`
calls. `MarketDataService`, `IBrokerAdapter` and `AlpacaMarketDataAdapter` are untouched.

---

## 3. Caller attribution — do not skip this

Every request should carry who is asking:

```
X-MarketData-Caller: mctrader
```

Without it the ledger cannot attribute spend, and R6 in `requirements.md` fails while looking
perfectly fine. The service reports unattributed spend as `mktdata.unattributed` and counts it, so
the gap is visible rather than absorbed — but visible is not the same as correct.

---

## 4. The four endpoints

### `GET /price/{symbol}?country_code=US&exchange=&max_age_seconds=`

```json
{ "symbol": "AAPL", "exchange": "US", "price": 343.83, "previous_close": 347.10, "change_pct": -0.9421,
  "bid": null, "ask": null,
  "provider": "eodhd", "fetched_at": "2026-10-29T04:34:16", "age_seconds": 0.4,
  "as_of": "2026-10-28T20:29:00", "as_of_age_seconds": 25424.3 }
```

404 `{"error": "unpriced", ...}` when nothing can price it.

**Send the country — it is not optional.** `country_code` (US, AU) is required on every
symbol-bearing call. `exchange` (US, AU/ASX, L, TO, HK, F, PA) is an optional second spelling and
must agree with it; a suffixed symbol (`CBA.AU`) is a third and must agree too. A ticker is
not unique across markets — `CBA` is an ASX listing and a US one — and a request that names no
country is refused with a 422 rather than answered from the US tape. A contradiction is 400
`exchange-conflict` rather than a preference for one.

### `POST /prices`

```json
{ "symbols": ["AAPL","MSFT"], "country_code": "US", "max_age_seconds": 300 }
```

Answers with `quotes`, `exchange`, `fetched`, `missing`, `deferred`, `route`, `complete`. One
request is one market: a list whose symbols carry two different suffixes is 400 `mixed-exchanges`,
so send one country per call.

**`route` is the important field.** The service chooses bulk or per-symbol from the list length
because the tariff breaks even at 100 symbols. A 2,757-name scan costs 100 units; six positions
cost 6. Neither caller has to know.

### `POST /atr`

```json
{ "symbols": ["AAPL"], "period": 14, "method": "tr", "smoothing": "simple",
  "country_code": "US",
  "price": 343.83, "multiplier": 1.0, "direction": "long" }
```

Answers with `value`, `percent`, `reference_price`, `period`, `method`, `smoothing`,
`sessions_used`, `as_of_session`, `source`, and `stop_level` when price **and** multiplier are
supplied.

- **`multiplier` is never defaulted.** It comes from `STOP_RULES.ATR_STOP_MULTIPLIER` in the broker
  config, which currently reads `1.0` on both broker rows. Pass it explicitly.
- 400 `{"error": "incomplete-stop"}` if `price` is given without `multiplier`.
- `value: null` with a `reason` when there is too little history — never a guess.

### `POST /bars`

```json
{ "symbols": ["AAPL"], "sessions": 15, "country_code": "US", "allow_fetch": true }
```

Answers with `as_of_session`, `sessions`, `adjusted`, `provider`, `rows[]`.

**The session in progress is already excluded.** Do not filter it again downstream; filtering twice
is harmless but it hides the fact that the guarantee lives in the service.

### `POST /bars/intraday`

```json
{ "symbols": ["TTD"], "country_code": "US", "timeframe": "4Hour", "limit": 48, "allow_fetch": true }
```

Answers in `POST /bars`'s series shape, so one decoder serves both. `timeframe` is `5Min`,
`15Min`, `30Min`, `1Hour` or `4Hour` (aliases `5m`…`4h`); `limit` defaults to 78 buckets.
`fetched` names the symbols that cost 5 units each; a cache hit inside 300 s costs nothing.

Two differences worth wiring deliberately: **`d` is an exchange-local ISO timestamp, not a session
date** (an intraday bar is a moment), and **`adjusted` is always `false`** with `cr: null`,
because the vendor's intraday tape is unadjusted. The buckets are anchored to the session's first
bar, so a 4H bar cannot straddle the overnight gap and is stable across requests — this is a
deliberate correction of the current `_aggregate_by_count`, which chunks from `now - window_days`
and therefore changes shape as the clock advances.

### `POST /bars/bulk`

```json
{ "market": "US", "dates": ["latest", "2026-08-13"], "allow_fetch": true }
```

Whole-market closes for one to ten sessions. `c` is the **adjusted** close. `"latest"` resolves
through the same completed-session rule `/bars` uses. A session is served from the cache only when
a completed pull is recorded for it; a pull far below the previous session's row count comes back
with its key in `partial` and is never served as complete afterwards. `allow_fetch: false` on an
unmarked session is `404 not-pulled`. Cost: 100 units per dated pull, free on a marked session.

Dated pulls include delisted listings (~50,270 codes against ~27,714), so intersect a dated set
with the live listing set before computing a market-wide statistic — the trap the local
`ReturnWindowService` already handles.

### `POST /screener`

```json
{ "country_code": "US",
  "filters": [["market_capitalization", ">", 300000000], ["refund_5d_p", ">", 5]],
  "sort": "refund_5d_p.desc", "limit": 50 }
```

Raw vendor rows for one market, **untouched**: no normalisation, no filtering, no `symbol` added.
An empty result is a 200 with `count: 0`. `filters` are validated against the vendor's 14 fields
before a unit is spent, so a typo is a local 422 rather than a vendor round-trip.

The venue travels inside `filters`: the service lifts any agreeing `exchange` filter and appends
exactly one `["exchange", "=", venue]` triple, **using the caller's spelling**, so a per-venue
`NASDAQ` screen stays per-venue rather than becoming a US-wide one. Keep the venue resolution
policy on your side — the service refuses a contradiction but does not choose a venue for you.
Cost: 5 units flat.

### `POST /profile`

```json
{ "symbols": ["WDS", "BHP", "CBA"], "country_code": "AU", "exchange": "ASX" }
```

Up to 25 listings. 5 units per **uncached** symbol, memoised in-process for an hour. A symbol with
no listing is `found: false` and appears in `missing`; a single failed call is that symbol's
`error`, never the batch's. `country`, `gic_sector`, `gic_industry` and `description` have no
entitled source and are always `null`. Worst case for a 25-symbol pass is about 300 s, so the
client timeout must exceed it (330 s is safe).

---

## 5. Error handling

| Status | Meaning | What to do |
|---|---|---|
| 400 | bad request (incomplete stop, bad method, no symbols) | fix the call — it will not succeed on retry |
| 400 `unknown-exchange` / `exchange-conflict` / `mixed-exchanges` | the market could not be resolved | fix the call: one country per request, and no suffix that contradicts it |
| 422 | `country_code` missing or not `US`/`AU` | fix the call — the country is required, there is no default |
| 404 | that symbol cannot be priced | mark it missing, move on |
| 502 | the vendor refused | retry later, or fall back to the broker |
| 503 `database-busy` | the daily load held a lock | **retry** — it is transient and short |

`503 database-busy` is the one worth wiring deliberately: a read that could not get a lock in 15
seconds returns immediately instead of hanging. Treat it as retryable rather than as an outage.

---

## 6. Gotchas that will bite

1. **The ATR method is `tr` + `simple`** — gap-inclusive true range, mean of the last N. That is
   the agreed standard and the service default; `method`/`smoothing` are echoed back on every
   response. Do not assume Wilder.
2. **`provider` may differ between two calls for the same symbol.** EODHD is primary, Yahoo is the
   fallback. A quote from Yahoo and a bar from EODHD is allowed, but it must be visible — that is
   what the field is for. Do not compute a change percentage by mixing providers.
3. **`as_of_age_seconds` is not `age_seconds`.** The first is when the vendor observed the price
   (measured hours behind for EODHD's US feed); the second is when the service fetched it. Gate
   freshness on `age_seconds`, report the other.
4. **`POST /prices` with very long lists returns a large payload** (~12,700 rows for the whole
   market, ~2 MB). If McTrader only needs the top N movers, sort locally rather than asking for a
   narrower list — a narrower list may drop below the 100-symbol break-even and become *more*
   expensive.
5. **`asset_class` is optional and absent means shares.** State it for an index, commodity ETF
   proxy or rate: `indices`, `commodities`, `rates`, `forex`, `crypto` (plus the aliases the
   service lists). An index priced as a share is a *different instrument* and usually comes back
   `404 unpriced`; a spelling the service does not know is `422 unknown-asset-class` rather than a
   silent share.
6. **A country code is mandatory.** `CBA`, `BHP`, `BXB` are ASX tickers *and* US ones. Pass
   `country_code` (`US` or `AU`) on every symbol-bearing call; omitting it is a 422, not a US
   default. A suffix (`CBA.AU`) corroborates the country but does not replace it. A
   contradiction is 400 `exchange-conflict`, never a silent pick.

---

## 7. Checklist

- [ ] `HttpMarketDataProvider` implements the 6 methods; calendar methods answered locally
- [ ] `X-MarketData-Caller: mctrader` on every request
- [ ] `QuoteCache` deleted
- [ ] `EodhdPriceService` deleted
- [ ] 90-second bar memo deleted
- [ ] `BulkPriceRoute` retired
- [ ] `ATR_STOP_MULTIPLIER` read from broker config and passed explicitly
- [ ] 503 `database-busy` treated as retryable
- [ ] Intraday, bulk-close, screener and profile calls re-pointed at the four new routes
- [ ] The EODHD key removed from McTrader's environment and keyring; no EODHD host reachable
- [ ] `asset_class` sent for the market picture's indices, commodity proxies and the rate
- [ ] Kill switch: `D:\market-data\NO-MKTDATA` present → direct vendor (per the `NO-BULK` precedent,
      which treats an *unreadable* switch as "off")
- [ ] Verified: a McTrader cycle prices and stops with `:8003` stopped and the kill switch set
