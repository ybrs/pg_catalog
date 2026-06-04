# TODO

## Pre-existing test failures (not caused by the lazy-catalog work)

- **`tests/test_validate_pg_catalog_views.py::test_run_view_query_missing_function`**
  - The test runs `SELECT missing_function_that_should_not_exist()` and expects
    psycopg to raise `psycopg.errors.UndefinedFunction` (SQLSTATE `42883`), which
    would classify the result as `status == "missing_function"`.
  - Instead the server returns a generic error code, so psycopg raises a plain
    `Exception` and the result is classified `status == "error"`.
  - Root cause: the pgwire server (`src/server.rs` / `src/router.rs`) has **no
    SQLSTATE mapping** for an undefined/invalid function — the DataFusion planner
    error (`Invalid function '...'`) is sent without the `42883` code.
  - Fix later: map DataFusion's "Invalid function" / undefined-function planning
    error to SQLSTATE `42883` when building the pgwire `ErrorResponse`, so clients
    can distinguish a missing function from a generic execution error.
  - Scope: server error-code mapping; unrelated to the lazy catalog feature.

## Lazy catalog follow-ups (from task_lazy_catalog_definitions.md, out of scope this round)

- Tier 3: equality-filter pushdown to the source (`relname=`, `nspname=`,
  `datname=`) so very large catalogs aren't fully enumerated per scan.
- `pg_description` / comments support.
- Retrofit the static `register_user_tables` INSERT path onto the shared pure
  row-builders (currently the lazy path uses them; the static path still emits
  SQL `INSERT`s) to prevent drift between the two paths.

## Catalog views still registered as static tables

- Only `pg_views` and `pg_tables` are registered as real views (`VIEW_ONLY_TABLES`
  in `src/session.rs`); a real view derives from `pg_class` and therefore reflects
  lazy/user tables when the lazy source is registered before view creation (see
  `get_base_session_context_with_lazy_catalog`).
- Other table/relation-listing views are still loaded as **static** `MemTable`s
  from their YAML `rows:` and so do NOT reflect lazy tables: `pg_matviews`,
  `pg_indexes`, `pg_sequences`, and the `pg_stat_*` family.
- To make any of them lazy-aware, add `("pg_catalog", "<name>")` to
  `VIEW_ONLY_TABLES` (they already carry `view_sql`) and add a test mirroring
  `test_pg_tables_view_reflects_lazy_tables`. Deferred until a consumer needs it.

## Catalog-UDF sync-over-async runtime: bounded-pool deadlock risk

Some scalar UDFs are synchronous but must run a catalog sub-query
(`pg_get_userbyid` → `pg_authid`, `oid(text)` → `pg_class`). They bridge
sync→async via `run_catalog_query` in `src/user_functions.rs`, which spawns the
sub-query on a dedicated, **bounded** runtime (`CATALOG_QUERY_RT`,
`worker_threads(2)`) and blocks the caller on a std channel:

```rust
static CATALOG_QUERY_RT = multi_thread runtime with worker_threads(2);
fn run_catalog_query(future) -> T {
    CATALOG_QUERY_RT.spawn(future);   // runs on a CATALOG_QUERY_RT worker
    rx.recv()                         // CALLER thread blocks until it finishes
}
```

Because the caller **blocks a worker** (`rx.recv()`) instead of yielding it, the
pool can only absorb nesting up to `worker_threads` deep. If catalog UDFs ever
**compose** — a UDF's sub-query itself evaluates another catalog UDF — every
worker can end up parked in `rx.recv()` waiting on a task that has no free worker
left to run it: a classic pool-exhaustion deadlock (hangs, no timeout).

Worked example (with 2 workers):

1. Outer query calls UDF-A → `run_catalog_query(sub_A)` → `sub_A` runs on worker #1; caller blocks.
2. `sub_A` evaluates UDF-B → `run_catalog_query(sub_B)`; worker #1 is now the caller and parks on `rx.recv()`.
3. `sub_B` runs on worker #2 — OK at depth 2.
4. One level deeper (depth 3), or two sibling nested calls at once, parks both
   workers waiting for a task with no worker to run it → **deadlock**.

Why we are safe **today**: the sub-queries are plain UDF-free scans
(`SELECT oid, rolname FROM pg_authid WHERE oid IN (...)`,
`SELECT oid FROM pg_class WHERE relname = '...'`). Scanning `pg_authid`/`pg_class`
evaluates zero catalog UDFs, so nesting depth is always exactly 1. The whole fix
rests on this one assumption: **catalog sub-queries stay UDF-free.**

The day that stops being true (e.g. a built-in view/UDF whose definition resolves
one catalog UDF via another), the bounded pool is the failure point. Bumping
`worker_threads` only moves the cliff to a fixed depth — not a real fix.

Desired direction (per maintainer): a proper **work-scheduling thread pool** — do
NOT spawn a thread per call (thousands of threads) and do NOT try to reason about
nesting depth. We want to schedule these blocking catalog lookups onto a pool
that won't deadlock under composition (e.g. a pool that can grow/borrow a thread
when a worker blocks, or restructure the UDFs to resolve catalog data without a
nested blocking query at all). Deferred — not in focus right now.
