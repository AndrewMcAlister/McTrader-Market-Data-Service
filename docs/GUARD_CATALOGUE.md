# Guard catalogue

Every rule here decides whether the service may spend vendor units, serve a cached number, or
refuse. Each entry names the one place that makes the decision, so a change can be reviewed
against the reasons it exists instead of against memory.

The `Location` lines are machine-checked by `tests/guard_catalogue.rs`: a guard that moves or is
renamed fails the test suite until this file follows it.

Format: `- **Location:** `path` · `literal snippet``. Keep the snippet short and unique; it is
the proof that the decision still lives in that file.

---

## G-01 · Exchange resolution

- **Location:** `src/domain/exchanges.rs` · `fn resolve_exchange(`
- **Gates:** Every symbol endpoint — which market a ticker names.
- **Trigger:** A missing `country_code` (the HTTP layer answers 422); a stated
  `exchange`/`country_code` that conflicts with the symbol's suffix; a batch mixing suffixes; an
  unknown exchange code.
- **Clear:** Send `country_code` — `US` or `AU` — with bare symbols, or suffix every symbol the
  same way and state the matching country. An internal caller may still state a crypto asset
  class, which resolves to `CRYPTO`; no HTTP request can.
- **Config:** none.
- **Tests:** `src/domain/exchanges.rs` · `exchange_and_country_code_that_disagree_are_refused`

`CBA` is both an ASX listing and a US listing. Serving one for the other returns a plausible
price for the wrong instrument, which is indistinguishable from a correct answer to a caller.
The HTTP layer requires `country_code` (`US`/`AU`) on every symbol-bearing endpoint, so a
missing or unknown one is a 422 before this resolver runs. The precedence is stated market →
symbol suffix → `US`; the last is now reachable only by internal callers. A conflict is a
refusal (`unknown-exchange`, `exchange-conflict`, `mixed-exchanges`), never a guess. Crypto has no
market and ignores any exchange passed with it.

## G-02 · Settled sessions only

- **Location:** `src/domain/sessions.rs` · `fn latest_completed_session(`
- **Gates:** `POST /bars` and `POST /atr` — the newest session that may be served.
- **Trigger:** The exchange's close plus its three-hour settlement window has not passed; an
  unknown or unresolved exchange zone.
- **Clear:** Wait for settlement. The rule is total: an unknown zone answers with the previous
  UTC date and logs, rather than failing or guessing the day in progress.
- **Config:** none.
- **Tests:** `src/domain/sessions.rs` · `the_settlement_boundary_is_exact_to_the_minute`

A partial session has a partial high, low and close. An ATR built on it shrinks exactly when the
morning is quiet, and a stop computed from a shrunk ATR is the wrong stop. Settlement is not the
close itself: the vendor is still writing the session, so the boundary is close + 3 hours. Crypto
never has a session in progress for this purpose.

## G-03 · Bars cache completeness

- **Location:** `src/products.rs` · `fn complete_window(`
- **Gates:** Whether `POST /bars` answers from `DailySnapshot` or calls a vendor.
- **Trigger:** Fewer rows than `sessions`; the series stops before the latest completed session;
  a served row states no `raw_close`.
- **Clear:** One refresh pulls a full unit of history — request depth
  `max(sessions, MDS_MIN_SESSIONS, MDS_HISTORY_SESSIONS)`. `allow_fetch=false` clears to
  cache-only and never calls a vendor.
- **Config:** `MDS_MIN_SESSIONS`, `MDS_HISTORY_SESSIONS`.
- **Tests:** `tests/products.rs` · `a_current_cache_without_raw_closes_is_refreshed_once`

The two closes are two price scales. A series half adjusted and half raw is worse than one that
is wholly adjusted, so a missing raw close on any served row makes the window unusable. Keeping
`allow_fetch=false` utterly passive is what lets a caller inspect the cache without spending.

## G-04 · Bars refresh memo

- **Location:** `src/products.rs` · `BARS_REFRESH_TTL_SECONDS`
- **Gates:** Repeated vendor calls for a symbol that still cannot fill its history.
- **Trigger:** The same `(market, symbol)` was already asked for the same session within 3600
  seconds.
- **Clear:** A new completed session, one hour, or a request that never asked (`allow_fetch`
  false).
- **Config:** none; the interval is a constant.
- **Tests:** `tests/products.rs` · `a_refresh_that_cannot_reach_the_session_is_asked_for_once`

A thin symbol may never have the requested depth. Without the memo every call would ask the
vendor again and pay again for the same answer — the memo makes a hopeless symbol cost one
request an hour, not one per caller.

## G-05 · Indicator cache eligibility

- **Location:** `src/products.rs` · `fn indicator_answers(`
- **Gates:** Whether `POST /atr` may read `IndicatorCache` at all, and per-row hits.
- **Trigger:** A non-default `method` or `smoothing` makes the whole call ineligible; a row is a
  miss when its method/smoothing differ, its `as_of_session` is ahead of the latest completed
  session, or it holds no value for the requested period.
- **Clear:** Compute from bars and upsert the default definition.
- **Config:** `MDS_ATR_METHOD`, `MDS_ATR_SMOOTHING`, `MDS_ATR_PERIOD`.
- **Tests:** `tests/products.rs` · `a_different_smoothing_is_computed_and_not_written_to_the_cache`

The cache stores one definition per symbol and session. Serving a `wilder` request from a
`simple` row would return a number the caller did not ask for under a label that says it did.
A row from a session still in progress is no better: it was computed from a partial bar.

## G-06 · Bulk route

- **Location:** `src/products.rs` · `fn use_bulk(`
- **Gates:** `POST /prices` choosing one whole-market call or per-symbol calls.
- **Trigger:** Stale symbols at or above `MDS_BULK_BREAKEVEN` (100) **and** the market is `US`.
- **Clear:** Smaller batches and every non-US exchange go per-symbol; the response's `route` says
  which way it went.
- **Config:** `MDS_BULK_BREAKEVEN`.
- **Tests:** `tests/products.rs` · `a_large_batch_goes_bulk_because_100_is_the_break_even`

The tariff breaks even at 100: one unit per symbol against 100 flat for the whole market. Below
that a whole-market call overpays; above it, per-symbol calls do. The feed exists only for the
US market, so a 2,800-symbol ASX request must not be routed to it.

## G-07 · Bulk snapshot reuse

- **Location:** `src/products.rs` · `BULK_SNAPSHOT_TTL_SECONDS`
- **Gates:** Whether the in-memory whole-market snapshot answers instead of the vendor.
- **Trigger:** The snapshot is younger than 120 seconds.
- **Clear:** Expiry; `refresh_snapshot` deliberately discards it so the scheduled collection at
  open + 45 is a real collection, not a cache read.
- **Config:** none; the interval is a constant.
- **Tests:** `tests/products.rs` · `the_bulk_snapshot_is_reused_inside_its_own_lifetime`

Two scanners asking seconds apart must not buy the whole market twice. The scheduled collection
is the exception for the opposite reason: its purpose is a record at a known instant, so reusing
a snapshot taken before that instant would defeat it.

## G-08 · Bulk snapshot row lag

- **Location:** `src/products.rs` · `DEFAULT_MAX_ROW_LAG_SECONDS`
- **Gates:** Which whole-market rows may enter the snapshot and cache.
- **Trigger:** A row more than four days (345,600 s) behind the newest observed row, or a price
  at or below zero.
- **Clear:** The row is dropped and the symbol comes back under `missing`, not as a stale price.
- **Config:** none; the bound is a constant.
- **Tests:** `tests/products.rs` · `the_bulk_snapshot_warms_the_cache_for_every_row_not_just_the_asked_ones`

A halted or delisted symbol stays in the whole-market feed at its last traded price. The
absolute age bound catches an ancient feed; the lag behind the newest row catches one symbol
that stopped trading while the market did not. Neither a zero nor a negative price is a price.

## G-09 · Price TTL floor

- **Location:** `src/products.rs` · `fn fresh(`
- **Gates:** A price cache hit versus a vendor fetch.
- **Trigger:** `fetched_at` is older than `min(MDS_PRICE_TTL_SECONDS, caller max_age_seconds)`;
  a missing `fetched_at` is always a miss.
- **Clear:** Fetch and write; the caller may only lower the age, never raise it.
- **Config:** `MDS_PRICE_TTL_SECONDS` (300).
- **Tests:** `src/products.rs` · `a_caller_may_lower_the_price_ttl_but_not_raise_it`

Accepting a longer TTL than the policy would let one caller keep a price alive past the age the
service promises. The floor is one-way on purpose: a caller can demand fresher data, not staler.

## G-10 · Refresh single-flight

- **Location:** `src/refresh.rs` · `fn try_acquire(`
- **Gates:** Concurrent bulk pulls — the double-spend guard.
- **Trigger:** Another refresh holds the process-wide slot.
- **Clear:** The second caller is refused immediately (`already-running`/busy) and never queues.
- **Config:** none.
- **Tests:** `src/refresh.rs` · `the_gate_admits_one_refresh_and_the_second_is_refused_without_waiting`

A bulk pull costs 100 units. Two callers racing must not mean two pulls. The slot is
process-wide and non-blocking: queueing would turn a quick refusal into a slow duplicate.

## G-11 · Refresh freshness

- **Location:** `src/refresh.rs` · `fn is_fresh<S>(`
- **Gates:** Whether a market needs a bulk pull today.
- **Trigger:** The ledger already holds a bulk pull for the market's current local day.
- **Clear:** `force=true` or an explicit `session_date` bypasses the check; an unreadable ledger
  counts as stale (one wasted pull beats silently never refreshing).
- **Config:** none.
- **Tests:** `src/refresh.rs` · `each_market_has_its_own_freshness_row`

`MAX(SnapshotDate)` cannot answer this: single-symbol rows written by `/bars` land in
`DailySnapshot` with today's date while the market-wide pull may not have run for weeks. The
usage ledger records the pull itself, which is the fact the question is about.

## G-12 · Provider support and order

- **Location:** `src/providers/router.rs` · `fn candidates(`
- **Gates:** Which providers may be asked for an asset class, and in what order.
- **Trigger:** A provider that declares `supports(asset_class) == false` is never asked.
- **Clear:** Shares/indices/commodities/rates/forex try EODHD then Yahoo; crypto tries Binance,
  then EODHD, then Yahoo. An empty answer is a refusal worth falling through.
- **Config:** none.
- **Tests:** `src/providers/router.rs` · `a_provider_that_cannot_serve_the_class_is_never_asked`

Asking Binance for an index spends time to receive a guaranteed no. The order reflects the
tariff and the feed's authority, not preference: two providers serving one asset class can
disagree, and the answer names which one served it.

## G-13 · EODHD retry policy

- **Location:** `src/providers/eodhd_client.rs` · `fn get_json(`
- **Gates:** Whether a vendor call is repeated, and whether the repeat is billed.
- **Trigger:** `429` retries up to six times, honouring `Retry-After` clamped to 0–60 s, else
  `min(2^(n-1), 30)` s, and is **unbilled**; a transport error gets one retry after 500 ms;
  every other 4xx/5xx is billed once and never retried.
- **Clear:** Success, the retry cap, or a billed refusal.
- **Config:** none.
- **Tests:** `src/providers/eodhd_client.rs` · `a_persistent_429_stops_at_the_cap_and_is_still_unbilled`

A rate-limit refusal means the vendor did no work, so retrying costs only time. A billed 4xx
means it did the work and rejected it; retrying pays twice for the same mistake. The transport
retry covers a dropped connection, not a vendor answer.

## G-14 · Scheduler retry limit

- **Location:** `src/schedule.rs` · `fn due(`
- **Gates:** When the scheduler may collect a market.
- **Trigger:** First attempt at open + `MDS_SCHEDULE_OFFSET_MINUTES` (45); one retry at fire +
  `MDS_SCHEDULE_RETRY_MINUTES` (15); after that the session is exhausted.
- **Clear:** A new session resets the state; a manual `/schedule/run` does not consume an
  attempt or reset the retry.
- **Config:** `MDS_SCHEDULE_OFFSET_MINUTES`, `MDS_SCHEDULE_RETRY_MINUTES`, `MDS_SCHEDULE_ENABLED`.
- **Tests:** `src/schedule.rs` · `a_failure_is_retried_once_fifteen_minutes_later_and_then_left_alone`

A third try is a person's problem, and `/schedule` says so rather than retrying quietly forever.
The retry still lands inside the session, after the vendor has had time to settle it. A manual
run is an operator's decision and must not edit the scheduler's accounting.

## G-15 · Admin token

- **Location:** `src/api/request.rs` · `fn admin_guard(`
- **Gates:** `POST /secrets/eodhd`, `POST /refresh`, `POST /schedule/run`.
- **Trigger:** `MDS_ADMIN_TOKEN` is set and `X-Admin-Token` is missing or does not match → 401
  `unauthorised`.
- **Clear:** Send the header. With the variable unset the call is allowed, but each one is
  logged as a warning so the gap is never silent.
- **Config:** `MDS_ADMIN_TOKEN`.
- **Tests:** `src/api/tests.rs` · `a_mismatched_admin_token_is_refused_with_401`

These endpoints rewrite the vendor key or spend units. On a single-user localhost a token is
optional — refusing by default would make first install impossible — but an unguarded call must
be visible in the log rather than indistinguishable from an authorised one.

## G-16 · Stop multiplier

- **Location:** `src/products.rs` · `fn requested_stop(`
- **Gates:** Whether an ATR response may carry `stop_level`.
- **Trigger:** `price` and `multiplier` are not both present → 400 `incomplete-stop`; a
  non-positive multiplier → 400 `bad-multiplier`.
- **Clear:** Supply both with a positive multiplier; `direction` defaults to `long`.
- **Config:** none.
- **Tests:** `tests/products.rs` · `stop_level_needs_both_price_and_multiplier`

The books use 1.0, 2.0 and 3.0. Defaulting the multiplier would return a confident stop the
caller never asked for; guessing is worse than refusing because a stop level is acted on.
