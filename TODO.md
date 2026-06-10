# TODO

## ⚠️ HIGHEST PRIORITY — column type mapping silently turns values into NULL

`map_pg_type` (`src/db_table.rs`) has **no arm for floating-point types**
(`float`, `float4`, `real`, `double`, `float8`) — they fall through to the
`_ => DataType::Utf8` default. A row builder then writes a JSON *number* into
that `Utf8` column, and `rows_to_record_batch` converts it via `v.as_str()`,
which returns `None` for a number → the value **silently becomes NULL**. No
error, no warning. Silence is the dangerous part: a column looks present but its
data quietly vanishes.

Concrete example — `pg_class.reltuples` is declared `float4` in the YAML schema:

```
schema: reltuples: float4
build_pg_class_row writes  row["reltuples"] = json!(0)      // JSON number
map_pg_type("float4")      => DataType::Utf8                // no float arm -> default
rows_to_record_batch       Utf8 column: json_number.as_str() => None => NULL
result:                    SELECT reltuples FROM pg_class    => NULL for every row
```

This hits **both** built-in YAML rows (`reltuples: 410.0`) and lazy rows
identically, so it is not a merge hazard and not a regression — but any client
reading `reltuples` (or any future float column) gets NULL instead of a number.

Fix (in a dedicated type-mapping branch): add a float arm to `map_pg_type`
(`"real" | "float4" | "float" | "double precision" | "float8" => DataType::Float64`),
a `DataType::Float64` arm to `rows_to_record_batch`, and have the float row
builders write `json!(0.0)`. Then add a round-trip test asserting a float column
survives (a test that would catch this class of bug going forward).

This is the umbrella issue for "column types": see also teleduck's
`duckdb_type_to_oid` mismatches (tracked in `../riffq/teleduck/TODO.md`).

## Lazy catalog follow-ups (from task_lazy_catalog_definitions.md, out of scope this round)

- **Column-fidelity parity check (catalog rows are sparse).** The lazy
  row-builders set only a subset of each catalog table's columns; the rest are
  emitted as NULL. This matches the existing eager `register_user_tables` path
  (same 7 `pg_class` columns; the lazy path sets *more* `pg_type` columns than the
  eager one), so it is parity — not a regression — but real PostgreSQL has many of
  these NOT NULL. Audit which NULL columns clients actually read and fill them.
  Currently NULL:
  - `pg_class` (26): reloftype, relowner, relam, relfilenode, reltablespace,
    relpages, relallvisible, reltoastrelid, relhasindex, relisshared,
    relpersistence, relnatts, relchecks, relhasrules, relhastriggers,
    relhassubclass, relrowsecurity, relforcerowsecurity, relispopulated,
    relreplident, relrewrite, relfrozenxid, relminmxid, relacl, reloptions,
    relpartbound.
  - `pg_type` (25): typowner, typbyval, typispreferred, typisdefined, typdelim,
    typsubscript, typelem, typarray, typinput, typoutput, typreceive, typsend,
    typmodin, typmodout, typanalyze, typalign, typstorage, typnotnull,
    typbasetype, typtypmod, typndims, typcollation, typdefaultbin, typdefault,
    typacl.
  - `pg_attribute` (19): attlen, attcacheoff, attndims, attbyval, attalign,
    attstorage, attcompression, atthasdef, atthasmissing, attidentity,
    attgenerated, attislocal, attinhcount, attcollation, attstattarget, attacl,
    attoptions, attfdwoptions, attmissingval.
  - `pg_database` / `pg_namespace`: complete (0 NULL).
- **Per-scan full re-walk + no filter pushdown (perf, deliberately deferred).**
  Each catalog scan re-invokes the whole source hierarchy
  (`databases()`→`schemas()`→`relations()`→`columns()`), and
  `LazyCatalogTableProvider` does not implement `supports_filters_pushdown`, so a
  single introspection query re-walks the source several times and filters only
  after materializing every row. Correctness is prioritized over this; a consumer
  with a large catalog should cache inside its own source. See Tier 3 below.
- Tier 3: equality-filter pushdown to the source (`relname=`, `nspname=`,
  `datname=`) so very large catalogs aren't fully enumerated per scan.
- `pg_description` / comments support.
- Retrofit the static `register_user_tables` INSERT path onto the shared pure
  row-builders (currently the lazy path uses them; the static path still emits
  SQL `INSERT`s) to prevent drift between the two paths.

## Fixed

- **Undefined-function SQLSTATE mapping** — `SELECT <missing_function>()` now
  returns SQLSTATE `42883` (`undefined_function`) with a `... does not exist`
  message instead of a generic `XX000`. Implemented as `into_pgwire_error` /
  `unknown_function_name` in `src/server.rs`, routed through all four query
  error sites. Verified by
  `tests/test_validate_pg_catalog_views.py::test_run_view_query_missing_function`.

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
