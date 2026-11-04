# Rust port of McTrader Market Data Service

## Context

`D:\McTrader_Market_Data_Service` is a ~9,500-LOC Python 3.12 service (FastAPI + pyodbc, 252
pytest cases) that serves prices, daily bars, ATR and stop levels to four trading books. It
owns three SQL Server tables (`SymbolPriceCache`, `DailySnapshot`, `IndicatorCache`), reads the
estate's `Exchanges`/`RequestTypes`/`EODHDUsage`/`BarHourly`/`Bar5Min` tables, routes across
EODHD → Yahoo → Binance, runs an in-process collection scheduler, exposes an MCP proxy, and is
hosted by NSSM as the `McTrader_Market_Data_Service` Windows service on `127.0.0.1:8003`.

`D:\McTrader-Market-Data-Service` is an empty git repository (only `.git`, `CLAUDE.md`,
`How_To_Code.md`) with `origin` already configured. The goal is a **full 1:1 Rust
reimplementation** there: same HTTP contract, same database schema and tables, same environment
variables, same provider behavior, same MCP tools, same Windows deployment. At cutover the Rust
service replaces the Python one; the two are not meant to run side by side permanently.

Confirmed scope decisions: full parity, drop-in compatible, install the Rust toolchain during
setup, Windows only.

## Non-goals

- No new product features (screener, streaming, order-book depth, 5-minute bar construction).
- No database migration: `sql/001_schema.sql`, `002_indexes.sql`, `003_index_maintenance.sql`
  are reused verbatim.
- No consumer changes; `INTERFACE.md` is the frozen external contract.
- No Linux/macOS support in this phase.

## Engineering standards (binding)

`How_To_Code.md` governs the implementation:

- Complete replacement, no coexistence/legacy paths for code we own.
- Atomic functions (<50 lines), one responsibility, single-verb names; explicit types;
  composition over monoliths; pure logic separated from I/O; no dead code.
- Every reusable component has tests; test observable behavior, not private internals.
- Validate config/credentials/venue/DB before expensive vendor work.
- One authoritative implementation of each rule (venues, sessions, ATR, tariff, symbols).
- Guard catalogue for gating logic more than a one-liner (`docs/GUARD_CATALOGUE.md`).
- Deferred **coding** items go to `TODO/Loose-Ends.md` separated by `********************************`.

## Architecture

**Single library crate `mmds` + focused binaries.** The Python project is one package with
several entry points; mirroring that keeps the port reviewable and lets the API, daily job,
scheduler, tools and MCP share one implementation of every rule. I/O is isolated behind traits
so domain tests need no database or network.

```
src/
  lib.rs                 module wiring
  config.rs              Settings + env parsing + key resolution
  error.rs               MarketDataError / StoreBusy / ApiError
  domain/                pure: atr, sessions, venues, availability, symbols, asset_classes
  clock.rs               dual market clock (DB Exchanges + data/clock.json fallback)
  store/                 tiberius pool, schema, repositories, SQL builders
  providers/             Provider trait, eodhd, yahoo, binance, router
  products.rs            price/bars/ATR orchestration, caches, bulk routing
  usage.rs               tariff + ledger + caller attribution
  secrets.rs             Windows keyring get/set/delete/mask
  refresh.rs             single-flight bulk refresh policy
  schedule.rs            in-process US/AU collection state machine
  api/                   axum router, DTOs, extractors, error rendering, docs
  mcp.rs                 rmcp proxy tools
  guards/                named guard functions (catalogue entries)
  bin/
    mmds-service.rs      NSSM host: logging, startup validation, axum serve, shutdown
    mmds-daily.rs        bulk pull / seed / raw-close fill / indicator rebuild CLI
    mmds-clock.rs        clock diagnostic
    mmds-mcp.rs          stdio + streamable-http MCP proxy
    mmds-tool.rs         operator subcommands (schema, probes, copy-exchanges, ...)
tests/                   API contract, SQL integration (ignored by default), parity harness
sql/                     copied verbatim from source
data/clock.json          copied verbatim (reference data)
docs/                    README, INTERFACE, WIRING, GUARD_CATALOGUE
scripts/                 ported PowerShell launchers/installers/parity script
```

Dependency direction: `domain` ← `store`/`providers` ← `products`/`refresh`/`schedule` ←
`api`/`mcp`/binaries. `domain` has no tokio/reqwest/tiberius dependency.

## Toolchain and dependencies

- Install `rustup` (stable, `x86_64-pc-windows-msvc`) and verify `link.exe`; install VS 2022
  Build Tools (C++ workload) if missing. Commit `rust-toolchain.toml` (`channel = "stable"`).
- Pin in `Cargo.toml`, resolve with `Cargo.lock`:

| Crate | Version | Features / purpose |
|---|---|---|
| tokio | 1.54 | rt-multi-thread, macros, net, signal, time, sync |
| axum | 0.8 | json, query, http1, tokio |
| tower-http | 0.7 | trace, limit |
| serde / serde_json | 1 | derive; exact contract DTOs |
| reqwest | 0.13 | json, rustls-tls, gzip |
| tiberius | 0.13 | tds73, chrono, rust_decimal, sql-browser-tokio |
| tokio-util | 0.7 | compat (tiberius) |
| bb8 | 0.9 | async connection pool (custom tiberius manager) |
| chrono / chrono-tz | 0.4 / 0.10 | UTC storage, IANA venue clocks |
| clap | 4.6 | derive CLI |
| tracing / tracing-subscriber / tracing-appender | 0.1 / 0.3 / 0.2 | service logging |
| thiserror | 2 | typed errors |
| async-trait | 0.1 | object-safe async traits |
| keyring-core + windows-native-keyring-store | 1.0 / 1.1 | Windows Credential Manager with explicit target |
| rmcp | 3.8 | MCP server (server, macros, transport-io, transport-streamable-http-server) |
| utoipa + utoipa-swagger-ui | 6 / 10 | `/docs` and `/openapi.json` parity |
| rust_decimal | 1.43 | decimal(18,6) read conversion |
| dev: wiremock, insta, pretty_assertions, tempfile, tower(util), http-body-util | | provider/API tests |

## Module mapping

| Python | Rust |
|---|---|
| `data_service/config.py` | `config.rs` |
| `data_service/atr.py` | `domain/atr.rs` |
| `data_service/sessions.py` | `domain/sessions.rs` |
| `data_service/venues.py` | `domain/venues.rs` |
| `data_service/availability.py` | `domain/availability.rs` |
| `data_service/providers/base.py`, `symbols.py` | `domain/asset_class.rs`, `domain/symbols.rs`, `providers/base.rs` |
| `data_service/eodhd.py` | `providers/eodhd.rs` (client + tariff semantics) |
| `data_service/providers/{eodhd,yahoo,binance}_provider.py` | `providers/{eodhd,yahoo,binance}.rs` |
| `data_service/providers/router.py` | `providers/router.rs` |
| `data_service/store.py` | `store/` (repositories + `SqlServerStore`) |
| `data_service/products.py` | `products.rs` |
| `data_service/usage.py` | `usage.rs` |
| `data_service/secrets_service.py` | `secrets.rs` |
| `data_service/refresh.py` | `refresh.rs` |
| `data_service/schedule.py` | `schedule.rs` |
| `data_service/clock.py` | `clock.rs` |
| `data_service/api.py` | `api/` |
| `data_service/service.py`, `__main__.py` | `bin/mmds-service.rs` |
| `data_service/mcp_server.py` | `bin/mmds-mcp.rs` + `mcp.rs` |
| `jobs/daily.py` | `bin/mmds-daily.rs` + `jobs/` |
| `tools/*.py` | `bin/mmds-tool.rs` subcommands |

## Key designs

### Configuration and secrets

- `Settings` is an immutable struct built once at startup. Reproduce every `MDS_*` variable and
  default from `config.py:197-236`, including the exact boolean rule (`"0"`, `"false"`,
  `"False"` mean false; anything else true), invalid int/float fallback, and ATR period parsing.
- Key resolution order: explicit argument → `EODHD_API_KEY` → Windows keyring → `none`; log only
  the source, never the key.
- **Keyring compatibility:** Python `keyring` stores a generic credential with
  `TargetName = "market_data:eodhd:global"` and `UserName = "eodhd:api_key"`. Rust's default
  Windows target is `user.service`, so use `keyring-core` + `windows-native-keyring-store`
  directly:
  `set_default_store(Store::new()?)` then
  `Entry::new_with_modifiers(service, user, &HashMap::from([("target", service)]))`.
  The `target` modifier is used verbatim as `TargetName`, matching Python. Verify by reading a
  credential written by the Python service, and by reading a Rust-written one back through
  Python.
- `MDS_DB_DRIVER` is accepted but unused under TDS; document it. If `MDS_DB_TRUSTED=0`, require
  optional `MDS_DB_USER`/`MDS_DB_PASSWORD` (new, UPPER_CASE) or fail fast with a clear message.

### Store

- `tiberius` over TDS with `AuthMethod::Integrated` (Windows SSPI, no extra feature). Connection
  pool via a small `bb8::ManageConnection`; on each new connection run `SET LOCK_TIMEOUT 15000`.
  Map SQL error 1222 to `StoreBusy` → HTTP 503 `database-busy` + `retryable: true`.
- Apply `sql/001_schema.sql` on startup with a `GO` splitter (regex-equivalent to
  `^\s*GO\s*$`, case-insensitive) using `include_str!`; keep the SQL files in `sql/` for DBAs.
- Rewrite every SQL statement from `?` to tiberius positional `@P1..@Pn`; keep chunk limits
  (prices/indicators 1,000; bar reads 500; stage inserts 20,000) and SQL Server's 2,100-param cap.
- `DailySnapshot` writes keep the staging-table + single `MERGE` pattern and the
  `COALESCE(source.RawClose, target.RawClose)` rule. Prices/indicators use chunked `MERGE`.
- Read `decimal(18,6)` through `rust_decimal::Decimal` and convert to `f64` at the store boundary
  (matching Python); bind `f64` so SQL Server performs the same implicit conversion pyodbc does.
- Split the 900-line `Store` into focused repository traits (`PriceRepository`,
  `DailyBarRepository`, `IndicatorRepository`, `UsageRepository`, `ExchangeRepository`,
  `OpsRepository`) combined by a `MarketDataStore` supertrait. `Products` takes
  `Arc<dyn MarketDataStore>`; tests use one in-memory fake. This keeps business logic free of
  SQL and mirrors `How_To_Code` principles 4, 8 and 10.

### Domain rules

- Port `sessions.py` exactly: venue close map, `SETTLEMENT_HOURS = 3`, venue-local close + 3h
  rule, unknown/crypto → previous UTC date; log unresolvable zones once and fall back
  conservatively.
- Port `venues.py` precedence and refusals verbatim: stated venue/country → uniform symbol
  suffix → `US`; crypto → `CRYPTO`; `unknown-venue`, `venue-conflict`, `mixed-venues` with the
  same sampling of offending symbols.
- Port `atr.py` arithmetic verbatim: gap-inclusive TR by default, `high_low` option, `simple`
  default / `wilder`, percent points, required multiplier, 6-decimal stop rounding. Use a
  Python-compatible round-half-even helper for `stop_level` to avoid off-by-one-ulp parity
  differences.
- Port provider symbol rendering and asset-class aliases from `providers/symbols.py` and
  `providers/base.py`, including the EODHD ETF proxies, index codes, and `None` for
  cannot-serve combinations.
- Port `usage.py` tariff parsing (`free`, `per_symbol`, `flat`, `per_call`), unknown-purpose
  row at zero with a warning, local-day boundary, and ledger-failure isolation.

### Providers

- `#[async_trait] trait Provider { fn name(); fn supports(asset_class) -> bool; async fn quote(...); async fn bars(...); }`
  plus an EODHD-only whole-market capability (not generic).
- Router order: shares/indices/commodities/rates/forex = EODHD → Yahoo; crypto = Binance →
  EODHD → Yahoo. Empty bars is a refusal worth falling through. Record bounded attempts and
  fallback counters.
- EODHD client preserves billing semantics exactly: one transport retry after 500 ms; **429 is
  unbilled and retried up to 6 times honouring Retry-After clamped 0-60 s, else
  min(2^(n-1), 30) s**; other 4xx/5xx are billed once and **never retried**; unparseable 200 is a
  distinct error. Record usage on 200 and on billed failures; never let a ledger failure fail the
  vendor call.
- Yahoo: browser UA, range ladder `730d,2y,1y,6mo,3mo`, 422 means shorter range, drop current UTC
  day and null halted rows, previous close from the second-newest close with >100% move guard.
- Binance: `/api/v3/ticker/24hr` and `/api/v3/klines`, USDT pair fallback, 400 means unknown
  pair, drop in-progress candle.
- Tests inject providers/`wiremock`; no test may read the real EODHD key or spend units.

### Products, caches, routing

- `Products` owns: price read-through cache (300 s TTL, caller may only lower it), in-memory
  whole-market snapshot (120 s), bars refresh memo (3,600 s per symbol/session), indicator-cache
  eligibility (only default method/smoothing, session not ahead of latest completed), and routed
  per-asset-class provider calls.
- Bulk route only when stale count ≥ 100 **and** venue is US; otherwise per-symbol. Missing
  symbols are not cached. Whole-market bulk writes all usable rows and preserves volume.
- Bars cache hit requires enough rows, reaching the latest completed session, and a non-null
  `raw_close` on every served row; otherwise refresh one unit's worth of history per symbol
  (request depth `max(sessions, min_sessions, history_sessions)`; keep all of it). `allow_fetch=false`
  is cache-only and never calls a vendor.
- ATR: indicator cache first only when method/smoothing/default-period/session checks pass;
  otherwise compute from bars and upsert. Insufficient history returns `value: null` with a
  reason. `price` without `multiplier` is `400 incomplete-stop`; multiplier is never defaulted.
- Single process invariant: one service instance per port; the snapshot, refresh lock, scheduler
  state and counters are process-local.

### HTTP API

- `axum` router reproducing every route in `api.py` with identical methods, paths, query/body
  fields, status codes and JSON bodies (parsed-JSON parity; object key order is irrelevant).
- Error rendering: `MarketDataError` → `{"error": reason, "detail": detail}` with its status;
  `StoreBusy` → 503 `{"error":"database-busy","detail":...,"retryable":true}`; add fallbacks so
  unknown routes/methods and validation failures return FastAPI-shaped bodies where practical.
- Preserve `X-MarketData-Caller` attribution (thread it explicitly through request context),
  `X-Admin-Token` guard on secrets writes, `/refresh`, `/schedule/run` when `MDS_ADMIN_TOKEN` is
  set, and `/admin/shutdown` semantics (202, then exit after ~400 ms).
- Keep `/healthz`, `/stats`, `/docs`, `/openapi.json` (utoipa/Swagger UI at `/docs`).
- JSON DTOs use `#[serde(rename = "...")]` for the short bar keys (`d,o,h,l,c,cr,v`) and all
  snake_case contract fields; `Option<T>` renders `null` where Python does.

### Clock, availability, scheduler, refresh

- `MarketClock` loads `dbo.Exchanges` when available and otherwise `data/clock.json`; broker and
  broker→asset-class rules always come from JSON. Port the abbreviation/country→IANA tables
  verbatim, weekend rollover, `close <= open` next-day roll, synthetic 24h clock, and
  `always_open` fallback.
- Scheduler: US/NYSE and AU/ASX; 20-second tick; fire at open + 45 min; one retry +15 min; then
  `manual-intervention` until the next session or manual run. Per-market/per-session in-memory
  state, exposed by `/schedule`, triggerable by `/schedule/run`.
- Collection: `refresh_daily_bars` (single-flight, ledger freshness check per market), then US
  whole-market snapshot or AU top-100 dollar-volume universe via per-symbol routing.
- `refresh.rs` is the single implementation used by `/refresh`, the scheduler and the daily job.

### Daily job and tools

- `mmds-daily` with the same flags: repeatable `--date`, `--since`, `--market`, `--seed-from-barhourly`,
  `--days`, `--fill-raw-close`, `--raw-close-days`, `--rebuild-indicators`, `--skip-indicators`.
- Reuse the same `pull_bulk`, `seed_from_barhourly`, `fill_raw_close`, `rebuild_indicators`
  functions from the library so the job and scheduler cannot drift.
- `mmds-tool` subcommands replace the ten Python tools: apply-schema, bench-upsert,
  check-index-quote, check-mcp, cleanup-sessions, copy-exchanges, probe-bulk-route,
  probe-responsiveness, smoke-api, smoke-providers.

### MCP

- `mmds-mcp` proxies the REST API (no business logic), with tools `get_price`, `get_prices`,
  `get_atr`, `get_bars`, `get_service_status`. Default transport stdio; streamable-http via rmcp.
  **SSE is not provided by rmcp 3.8**; either add a small custom SSE transport or record it as a
  deferred gap. Decision point at implementation.

### Hosting, logging, ops

- Keep NSSM initially (proven service-account/keyring behavior); port `install-service.ps1`
  (install/status/start/stop/restart/remove, `-TakeOver`, `-AsUser`, `-EodhdApiKey`) to point at
  `mmds-service.exe` with the same service name. Native `windows-service` hosting is a deferred
  improvement.
- `mmds-service` validates config/DB before binding, logs to `logs\data_service.log` with
  rotation, logs only the key source, and exits non-zero on fatal startup so NSSM restarts it.
- Port `run.ps1`/`run.cmd`, `start.bat`, `stop.bat`, `refresh.bat`,
  `scripts/Ensure-MarketDataService.ps1`, `scripts/Install-RefreshTask.ps1`,
  `scripts/Refresh-DailyBars.ps1`; keep port 8003 and the same URLs/headers.
- Copy and adapt `README.md`, `INTERFACE.md`, `WIRING.md`; state Rust-specific setup and the
  known differences (driver env var, SSE).

## Guard catalogue

Create `docs/GUARD_CATALOGUE.md` and named functions in `src/guards/` for at least:

| Guard | Gates |
|---|---|
| `resolve_venue` refusals | unknown venue, conflict, mixed venues |
| `latest_completed_session` | serves only settled sessions |
| `bars_cache_complete` | raw-close completeness + session depth |
| `indicator_cache_eligible` | method/smoothing/session/period eligibility |
| `bulk_route_allowed` | US-only + ≥100 stale symbols + snapshot TTL |
| `price_ttl_floor` | caller may only lower the cache age |
| `refresh_single_flight` / `refresh_is_fresh` | no double spend; ledger local-day check |
| `provider_supports` | never ask a provider that cannot serve the class |
| `eodhd_retry_policy` | 429 unbilled retries vs billed no-retry |
| `scheduler_retry_limit` | one retry then manual intervention |
| `admin_token_guard` | sensitive endpoints when configured |
| `stop_multiplier_required` | price without multiplier is 400 |
| `bulk_snapshot_row_lag` | drop rows older than 4 days |

Each entry records name, location, what it gates, trigger, clear, config key, tests, and a
~5-line note. A test parses the catalogue and fails if an entry's location no longer exists.

## Implementation phases

**Phase 0 — Scaffold.** Install rustup + MSVC build tools; create Cargo project, toolchain file,
`.gitignore`/`.gitattributes`, module skeleton, copy `sql/` and `data/clock.json`, port docs.
Acceptance: `cargo build`, `cargo fmt --check`, `cargo clippy -D warnings` on the empty skeleton.

**Phase 1 — Pure domain.** `config`, `error`, `atr`, `sessions`, `venues`, `availability`,
`symbols`, asset classes, usage tariff types. Port the corresponding pytest cases. Acceptance:
all domain tests pass with no tokio/reqwest/tiberius dependency.

**Phase 2 — Providers.** EODHD client with billing/retry semantics, Yahoo, Binance, router,
symbol rendering. Fixture/wiremock tests; no network. Acceptance: retry/billing/fallback tests
match `test_eodhd.py`, `test_providers.py`, `test_usage.py`.

**Phase 3 — Store.** tiberius pool, integrated-auth spike (decimal round-trip, temp table,
MERGE, error 1222), schema apply, repositories. Acceptance: SQL-shape unit tests; optional
integration tests against a disposable database; no writes to production during tests.

**Phase 4 — Products and refresh.** Prices, bars, ATR, cache/routing policy, bulk snapshot,
single-flight refresh. Port `test_products.py`, `test_refresh.py`, `test_bulk_volume.py`.
Acceptance: behavior tests with fakes.

**Phase 5 — HTTP API.** All routes, DTOs, error shapes, admin/caller handling, `/docs`.
Contract tests via `tower::ServiceExt::oneshot`. Acceptance: route-by-route parity table against
`api.py`; validation/error bodies match.

**Phase 6 — Clock, availability, mover, scheduler.** DB/JSON clock, sessions endpoints,
`todays-*`, scheduler state machine. Port `test_market_clock.py`, `test_schedule.py`,
`test_availability.py`, `test_sessions.py`.

**Phase 7 — Service binary and ops.** Logging, startup validation, `/admin/shutdown`, NSSM
scripts. Acceptance: install/start/stop/refresh on a test port; healthz green.

**Phase 8 — Daily CLI and tools.** All flags and subcommands; same SQL paths. Acceptance:
`--seed-from-barhourly`, `--fill-raw-close`, `--rebuild-indicators` verified on a disposable DB.

**Phase 9 — MCP.** Five tools, stdio + streamable-http; decide SSE. Acceptance: a client lists
and calls all five tools against the Rust API.

**Phase 10 — Parity and cutover.** Golden comparison against the Python service; full test run;
stop/retire the Python service; install the Rust service on 8003; update docs; delete the plan;
write `TODO/Loose-Ends.md`.

## Verification

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --all-targets` (unit + contract; SQL integration tests `#[ignore]` unless
  `MDS_TEST_DATABASE` is set)
- `cargo build --release`
- Start `mmds-service.exe` on an alternate port (e.g. 8013) against `Market_Data` and check
  `/healthz`, `/market/clock`, `/market/session`, `/market/exchanges`, `/trading_sessions`,
  `/bars` with `allow_fetch=false`, `/atr` (cache-only), `/docs`.
- **Golden parity harness** (`scripts/parity.ps1`): run Python on 8003 and Rust on 8013 against
  the same DB; compare stable endpoints (`/bars` cache-only, `/atr`, clock/session/exchange
  endpoints, availability) after normalizing timestamps/ages; compare `/prices` shape and venue,
  not live values. Store normalized snapshots for review.
- Do **not** call `/refresh` with `force=true` during verification unless the user approves the
  100-unit spend; use `already-fresh`/cache-only paths.
- Port every pytest case; map the 17 test modules to Rust modules and record any intentionally
  skipped case with the reason in `TODO/Loose-Ends.md`.

## Risks and decisions to watch

1. **tiberius integrated auth + decimal binding**: verify in Phase 0/3 with a tiny spike
   (`SELECT CAST(123.456789 AS decimal(18,6))`, a temp table, a MERGE, error 1222). If it fails,
   switch the store implementation to `odbc-api` (same ODBC Driver 17) before building on it.
2. **Keyring target naming**: must use an explicit target equal to the service; verify both
   directions with the existing Python credential.
3. **Python float/rounding parity**: JSON text formatting differs but parsed values should match;
   `stop_level` needs half-even rounding; avoid `rust_decimal` end-to-end because Python converts
   to float at the DB boundary.
4. **SQL rewrite**: every `?` becomes `@Pn`; chunk sizes must stay under 2,100 parameters.
5. **MCP SSE**: rmcp 3.8 has stdio + streamable-http only; decide custom SSE vs documented gap.
6. **`MDS_DB_DRIVER`/`MDS_DB_TRUSTED`**: driver var is ignored under TDS; non-trusted auth needs
   new optional credentials.
7. **Single-instance invariants**: snapshot, refresh lock, scheduler and counters are in-process;
   run exactly one service instance.
8. **No coexistence**: after cutover, stop and remove the Python NSSM service; keep the Python
   repo only as reference, not as a running alternate.

## Completion housekeeping

- Update `README.md`, `INTERFACE.md`, `WIRING.md`, and `docs/GUARD_CATALOGUE.md`.
- Delete this plan file after implementation.
- Add deferred coding work to `TODO/Loose-Ends.md`, each block separated by a line of
  `********************************`.
- Ensure no temporary scripts or one-off files remain.
