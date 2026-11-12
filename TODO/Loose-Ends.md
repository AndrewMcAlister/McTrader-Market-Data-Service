# Loose ends

Open items from the Rust port: deferred coding, decisions owed, and verification still owed.
Each block is separated by a line of `*`.
Resolved items are removed rather than kept as history — the tests are the record.

********************************

## Security — a literal EODHD API key sat in McTrader's test files

`D:/McTrader/tests/test_analysis_screener_live.py` and the Postman `EODHD` collection held a
literal EODHD API key. The value is deliberately not reproduced here.

**The fix is an operator step, outside HTTP:** delete both files and rotate the key in the EODHD
account. Rotation means editing the service account's `market_data:eodhd:global` /
`eodhd:api_key` entry in Windows Credential Manager and restarting the service —
`POST /secrets/eodhd` is set-once and refuses with 409 `key-already-set`, so it cannot replace a
key. No keyring re-point is needed once McTrader loses the key entirely, which the egress work
above is for.

**Status:** owed. Until both files are deleted, the key is in the working tree and in whatever
history and backups contain them.

********************************

## Phase 4 — `test_refresh.py::test_broken_calendar_does_not_crash_the_refresh`

**Source case:** the Python refresh caught an exception from `latest_completed_session` and
returned `expected_session: null` with the pull still reported.

**Why it is not ported:** the Rust session rule is total. An unknown or unresolved exchange zone
falls back to the previous UTC date and logs a warning, so the calendar cannot fail and a
refresh always reports a session. There is no error path to exercise, and inventing one would
add a failure mode the production code does not have.

**Consequence:** `RefreshOutcome.expected_session` is never `Presence::Null` today. The third
state is still needed by the ATR `reason` field; if the session rule ever becomes fallible, the
refresh must switch to `Presence::from_option` and this test should be restored.

********************************

## Phase 9 — MCP `sse` transport value

**Source behaviour:** `MDS_MCP_TRANSPORT=sse` served MCP over Server-Sent Events and the Python
launcher could connect with the SSE transport.

**Why it is not ported:** rmcp 3.8 provides `stdio` and streamable HTTP only; the MCP
specification superseded SSE with streamable HTTP. `mmds-mcp` accepts `sse`, warns, and serves
streamable HTTP at `/mcp`. Rejecting the value would break an existing launcher, and silently
doing nothing would be worse than saying what happened.

**Deferred coding:** add a custom SSE server transport over the same `ToolRouter` if a client
needs it, or remove `sse` from the accepted values at the next breaking configuration change.
The transport parsing and its default live in `src/bin/mmds-mcp.rs`; no test currently pins the
`/sse` path because it does not exist.

********************************

## Phase 5 — Extractor-level refusals are not FastAPI-shaped

**Source behaviour:** FastAPI validated every query parameter and JSON body before the handler
and answered 422 with `{"detail":[{"type", "loc", "msg", "input"}]}`.

**Port difference:** DTO-level refusals (`/bars`, `/prices`, `/atr`) reproduce that 422 shape
through `ApiError::validation`. Failures inside axum's extractors do not: a malformed JSON body
or a mistyped query value (`broker_id=abc`, a non-boolean flag) is refused
with axum's own 400 and a plain-text body before any handler runs. `session_date` is also typed
as a date, so an unparseable value is a 422 here while Python accepted the string and failed
later.

**Deferred coding:** add rejection handlers for `JsonRejection` and `QueryRejection` that render
`ApiError::validation`, so every refusal carries the same shape and status; then decide whether
`session_date` should stay typed (safer) or accept any string for byte parity. No test currently
pins the plain 400 bodies, and `README.md` records the difference.

********************************

## Phase 5 — `/schedule/run` reports a provider failure as 409

**Source behaviour:** `/schedule/run` answered 409 only for the scheduler's own refusals
(`unknown-market`, `closed`). A vendor exception escaped `refresh_daily_bars`, so FastAPI
answered 500.

**Port difference:** `Collector::run_market` catches every refresh failure and returns
`MarketRun { ok: false, status: "error", ... }`, which the endpoint renders as 409. The failure
is still reported in the payload and recorded by the scheduler, but the status code no longer
distinguishes "the exchange is closed" from "the vendor failed". The collector documents this as
deliberate: a manual run answers rather than raises.

**Decision owed:** keep the 409 and accept it as the one shape for every non-refresh outcome, or
map `status == "error"` to 500/502 so a client can branch on a transport failure. The
202/409/200 mapping for ordinary outcomes must not change. `README.md` records the difference.

********************************

## Phase 8 — the daily write jobs are still unverified

**Owed work:** `mmds-daily`'s write paths — `--seed-from-barhourly`, `--fill-raw-close` and
`--rebuild-indicators` — have not run against a disposable database. The read-only paths and the
service itself are live, but the seed, raw-close backfill and indicator rebuild have only been
seen to compile.

**Before running them:** back up `Market_Data`, run each command against a disposable database
first, and never point a write command at `Market_Data` until it is confirmed safe on a copy.
`--fill-raw-close` against a full symbol set is a large spend — size it before committing to it.
Do not call `/refresh` with `force=true` (100 units) without approval.

**Consequence:** the test suite is green and the service is live under NSSM, but the bulk write
jobs are verification debt, not a code gap. They must be proven on a copy before they are run
against production.

********************************

## EODHD egress — probes P1–P8 are still owed

The four egress routes (`/bars/intraday`, `/bars/bulk`, `/screener`, `/profile`) and the optional
`asset_class` field are implemented and tested, but the plan's eight live probes have not run:
they need `Market_Data` and, for P6, the deployed binary. None of them is a code change.

| # | What | Expected / what it decides |
|---|---|---|
| P1 | `SELECT RequestType, CostBasis, CostEach FROM dbo.RequestTypes WHERE RequestType IN (N'/screener', N'/intraday/', N'/eod-bulk-last-day/US', N'/eod-bulk-last-day/AU')` | one row each: `/screener` 5 `flat`, `/intraday/` 5 `per_call`, both bulk rows 100 `flat`. The guarded INSERT seeds `/screener` and leaves a different existing row alone; if the live row disagrees with the literal 5, the row wins |
| P2 | one `/screener` call with `[["code","in",["WDS","BHP"]],["exchange","=","AU"]]` (5 units) | two rows — which would make a batched `/profile` safe; until then the per-symbol fan-out is final |
| P3 | one `/screener` call pinned to `code`/`exchange` for `WDS` | records `currency_symbol`'s live value form; the three-uppercase-letter rule holds either way |
| P4 | `SELECT COUNT(*), MAX(BarTimeUtc) FROM dbo.Bar5Min` and `BarHourly` | expected stale (newest ~2026-10-30 / ~2026-10-14), confirming §5.2's refusal to read the estate tape |
| P5 | two `/bars/bulk` calls for the same date | the second is `provider: "cache"` and the ledger is unchanged; a two-date miss costs 200 |
| P6 | each new route against the live service on a temporary `MDS_PORT` | the INTERFACE.md example shapes, `provider` correct |
| P7 | grep every live broker row, watchlist and stored `Screener_Settings` for a venue outside US/AU | none. If one exists, the US/AU scope decision has to be reopened before `/screener` is relied on |
| P8 | `SELECT COUNT(*) FROM dbo.DailySnapshot WHERE SnapshotDate = <latest session>` | records how partial the latest session is, which is the evidence the `BulkSnapshotPull` marker exists for |

**Also owed before S1/S2 go live:** a `Market_Data` backup, because both slices add a table. The
DDL is guarded and idempotent, and the previous binary runs against the new schema unchanged.

**Deployment itself is an operator step, not a coding one:** stop/start the NSSM service **with
elevation** (it keeps the old binary otherwise), after proving the build on a temporary `MDS_PORT`
with `run.ps1`. Rollback is `git revert` + rebuild + an elevated restart; the two new tables are
left in place and are harmless.

********************************

## EODHD egress — the profile memo cap is not exercised to its bound

`PROFILE_CACHE_MAX_ENTRIES` (4096) and its one-time warning are implemented in
`Products::remember_profiles`, and the guard is catalogued as G-18. The test suite pins the TTL,
the miss-not-cached rule, the dedupe and the per-symbol failure isolation, but **no test drives
the memo to its cap**: doing so needs 4,097 distinct symbols and about 20,000 units of simulated
spend.

**Deferred coding:** if the cap ever matters in practice, make it a `Settings` value
(`MDS_PROFILE_CACHE_MAX_ENTRIES`) and test it at a small bound. The rest of the guard — prune on
insert, never memoise a miss, never memoise a failure — is already pinned.

