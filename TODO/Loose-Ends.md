# Loose ends

Open items from the Rust port: deferred coding, decisions owed, and verification still owed.
Each block is separated by a line of `*`.
Resolved items are removed rather than kept as history — the tests are the record.

********************************

## Phase 4 — `test_refresh.py::test_broken_calendar_does_not_crash_the_refresh`

**Source case:** the Python refresh caught an exception from `latest_completed_session` and
returned `expected_session: null` with the pull still reported.

**Why it is not ported:** the Rust session rule is total. An unknown or unresolved venue zone
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
distinguishes "the venue is closed" from "the vendor failed". The collector documents this as
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
