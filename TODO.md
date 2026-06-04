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
