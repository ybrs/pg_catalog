# Roadmap - phased backlog

Object/view inventory and live status (working / partial / broken + reasons) live
in [`CATALOG-REFERENCE.md`](CATALOG-REFERENCE.md). This file is the consolidated,
phased plan: what to build next and why. Completed work is removed, not archived.

## Project mental model (read first)

pg_catalog is a live PostgreSQL catalog compatibility layer in front of DataFusion.
When something (e.g. `riffq`) fronts a real database, the user's tables/columns/
indexes are materialized into the catalog at runtime so psql / ORMs / BI tools can
introspect them. The catalog is mutable and reflects the live database; it is NOT a
frozen photo of some PostgreSQL.

The static YAML dump (`pg_catalog_data/pg_schema/*.yaml`, from real PostgreSQL 17.4)
is a seed for the immutable built-ins plus test fixtures, NOT the source of truth.
Do NOT bake per-database/dynamic answers (indexes, views, constraints, defaults) out
of it by oid lookup - a user's runtime object is not in the dump.

Runtime registration today populates `pg_class`, `pg_attribute`, `pg_namespace`,
`pg_type`, `pg_database`, and `pg_index`, via two interchangeable paths that share
the same row-builders:
- eager / pre-register: `register_user_database`, `register_schema`,
  `register_user_tables`, `register_user_index`.
- lazy / callback: a `LazyCatalogSource` implementation + `register_lazy_catalog`.

Still seed-only for user objects: `pg_constraint`, `pg_rewrite`, `pg_attrdef`.

## The strategic pivot: the integration supplies definitions, we never deparse

PostgreSQL stores views, rules, check constraints, column defaults, and index
expressions as compiled **node trees** (`pg_rewrite.ev_action`,
`pg_constraint.conbin`, `pg_attrdef.adbin`, `pg_index.indexprs/indpred`), and
regenerates SQL from them with `ruleutils.c` (~12k lines). We never stored those
trees, so we cannot reconstruct the SQL - and reimplementing `ruleutils.c` is out of
scope and would never match byte-for-byte.

Decision: **we do not deparse.** Because pg_catalog always fronts a *custom*
database, the integration that owns those objects already knows their definition
text and updatability. So instead of reverse-engineering SQL, we extend the
registration contract (the same eager + lazy callback used for tables/indexes) to
let the integration **supply** the human-facing strings and flags. When a definition
is not supplied, the column stays NULL (today's behavior) - it is purely opt-in.

This reframes the single biggest item on the old backlog ("reimplement the deparser")
into "add optional definition fields to the registration API" - which is Phase 2.

What stays structural (no deparse, fully ours to compute): a *plain* index's
`CREATE INDEX` text is determined by data we already hold
(`pg_index.indkey/indisunique`, `pg_class`, `pg_am`, `pg_attribute`); only
functional/partial index *expressions* need supplied text.

---

## Phase 1 - Finish structured user-object registration (no deparse)

Goal: a registered user table is fully introspectable from structured data alone.

- `pg_get_indexdef` for plain indexes via templating, from live catalog rows at call
  time (read whatever `pg_index` holds, seeded + user). Build
  `CREATE [UNIQUE] INDEX name ON schema.tbl USING am (col, ...)` from
  `pg_class`/`pg_index`/`pg_am`/`pg_attribute`. Leave functional/partial expression
  text to Phase 2. When done: drop `pg_catalog.pg_indexes` from
  `KNOWN_CONTENT_MISMATCHES` and confirm the snapshot test goes green.
- `pg_constraint` registration (structured): PK / FK / UNIQUE described by their key
  columns and referenced relation. Feeds `table_constraints`, `key_column_usage`,
  `constraint_column_usage`, `referential_constraints` for user tables. Mirror the
  `IndexDef` shape (one `ConstraintDef` -> the `pg_constraint` row(s); FK target
  resolved by oid).
- `pg_attrdef` registration (structured handle + supplied text): the column-default
  text is integration-supplied (Phase 2 territory), but the `pg_attrdef` row and
  `pg_attribute.atthasdef` flag are structural and belong here.
- Column-fidelity audit: the lazy/eager row-builders set only a subset of each
  catalog table's columns; the rest are NULL though real PostgreSQL has many NOT
  NULL. Fill the columns clients actually read. Known-NULL today: `pg_class` (relam,
  relfilenode, reltablespace, relpages, relnatts, relchecks, relhassubclass, ...),
  `pg_type` (typbyval, typelem, typarray, typinput/output/..., typalign, ...),
  `pg_attribute` (attlen, attbyval, attalign, attstorage, attidentity, attinhcount,
  attcollation, ...). `pg_database`/`pg_namespace` are already complete.
- Retrofit `register_user_tables` onto the shared `build_*_row` helpers +
  `append_catalog_row` (the lazy path and `register_user_index` already do this;
  `register_user_tables` still emits raw SQL `INSERT`s - the last drift source).

## Phase 2 - Integration-supplied definition text (the deparse pivot)

Goal: views/constraints/defaults become fully introspectable for integrations that
provide the text, without us ever shipping a node-tree deparser.

- Extend the registration contract with optional definition fields:
  - view definition SQL + updatability flags on a `ViewDef`/`RelationDef`
    (relkind `v`/`m`). Feeds `pg_views.definition`,
    `information_schema.views.view_definition`, `pg_get_viewdef`, and the
    `is_updatable` / `is_insertable_into` columns (which today read the
    `pg_relation_is_updatable` / `pg_column_is_updatable` stubs - the integration
    knows whether its views are updatable, so it supplies the flag).
  - check-constraint text and column-default text on `ConstraintDef` / the column
    default. Feeds `check_constraints.check_clause`, `pg_get_constraintdef`,
    `pg_get_expr`, `information_schema.columns.column_default`.
  - functional/partial index expression text on `IndexDef`. Feeds the expression
    portion of `pg_get_indexdef` that Phase 1 left unrendered.
  - (rule definitions -> `pg_rules` / `pg_get_ruledef`: niche, defer.)
- Implementation pattern: the relevant UDFs (`pg_get_viewdef`, `pg_get_constraintdef`,
  `pg_get_expr`, `pg_relation_is_updatable`, ...) read the supplied text/flag from the
  live catalog at call time (the runtime-catalog-lookup pattern used by
  `pg_get_userbyid`), returning NULL / the safe default when nothing was supplied.

## Phase 3 - Session/GUC and SQL-surface compatibility

Goal: behave like PostgreSQL for the session-control surface tools rely on, and clear
the self-contained "broken view" gaps.

Session/GUC (SET is already per-session and reflected in `pg_settings`):
- `SET` should return the `SET` command tag on the wire (currently returns an empty
  result, so JDBC/psql don't see the ack).
- `SET LOCAL ...` (currently errors "LOCAL is not supported").
- `SET TIME ZONE '...'` (currently errors "Unsupported SQL statement").
- `current_setting(name)` and `current_setting(name, missing_ok)`.
- `current_schema()` returns only `public` (DataFusion has one schema level) - document
  or model multiple schemas.

Self-contained broken views (see `CATALOG-REFERENCE.md` "fixable" list):
- Missing scalar functions that unblock a real view each: `pg_table_is_visible`
  (`pg_seclabels`), `pg_sequence_last_value` (`pg_sequences`), `row_security_active`
  (`pg_stats`), `pg_get_statisticsobjdef_expressions` (`pg_stats_ext_exprs`).
- Planner/rewrite fixes: `pg_group` (`pg_authid.oid` unresolved after subquery
  flattening - good small win), `pg_available_extension_versions` (spurious GROUP BY
  wildcard), `pg_policies` (unsupported SQL type name), `pg_publication_tables`
  (sqlparser `ARRAY` parse error), `pg_stats_ext` (`s.stxkeys` scoping).
- `is_updatable` (4 cols) / `is_insertable_into` (2 views): the
  `pg_relation_is_updatable` stub returns 0. Real value is structural (view
  auto-updatability), but per the Phase 2 pivot it should be integration-supplied;
  niche, leave stubbed with the precise snapshot baseline until then.

## Phase 4 - Engine robustness and scale

- Catalog-UDF sync-over-async pool redesign (see Hazard below). Replace the bounded
  `CATALOG_QUERY_RT` + blocking `rx.recv()` with a scheme that cannot deadlock under
  UDF composition (a pool that grows/borrows a worker when one blocks, or restructure
  the UDFs to avoid a nested blocking catalog query).
- Lazy-source filter pushdown: implement `supports_filters_pushdown` on
  `LazyCatalogTableProvider` and push equality filters (`relname=`, `nspname=`,
  `datname=`) into the source, so a large catalog is not fully re-enumerated per scan.
- Promote the static relation-listing views (`pg_indexes`, `pg_matviews`,
  `pg_sequences`) to live views over the registration where it makes sense - today
  only `pg_tables`/`pg_views` are live (`VIEW_ONLY_TABLES`); the rest are static
  snapshots that do not reflect registered user objects.

## Phase 5 - Runtime/monitoring views (optional, integration-driven)

The ~41 `pg_stat_*` / `pg_locks` / `pg_cursors` / `pg_replication_*` views need live
server-runtime state via table functions we don't have (`pg_stat_get_activity`,
`pg_lock_status`, `pg_stat_get_numscans`, ...). Default: leave them empty (most tools
tolerate empty stat views). Optional: a runtime-stats callback on the registration
contract (same pattern as everything else) so an integration can supply live rows.
Lowest priority.

## Cross-cutting (ongoing)

- Per-column pinning of `KNOWN_CONTENT_MISMATCHES` in
  `tests/test_view_output_snapshot.py`: today an entry exempts a view's *whole*
  content; tighten it to exempt only the named columns so an unrelated column drift
  still fails. (The count side is already precise - see the demo-table strip.)
- Test-coverage gaps: `session.rename_columns`, `server.batch_to_field_info`,
  `ObservableMemTable.scan` logging, the `build_table` helper.
- `pg_description` / comments support (small, structural).

---

## Hazard: catalog-UDF sync-over-async bounded-pool deadlock risk

Some scalar UDFs are synchronous but must run a catalog sub-query (`pg_get_userbyid`
-> `pg_authid`, `oid(text)` -> `pg_class`). They bridge sync->async via
`run_catalog_query` in `src/user_functions.rs`, which spawns the sub-query on a
dedicated bounded runtime (`CATALOG_QUERY_RT`, `worker_threads(2)`) and blocks the
caller on a std channel:

```rust
static CATALOG_QUERY_RT = multi_thread runtime with worker_threads(2);
fn run_catalog_query(future) -> T {
    CATALOG_QUERY_RT.spawn(future);   // runs on a CATALOG_QUERY_RT worker
    rx.recv()                         // CALLER thread blocks until it finishes
}
```

Because the caller blocks a worker (`rx.recv()`) instead of yielding it, the pool
absorbs nesting only `worker_threads` deep. If catalog UDFs ever compose (a sub-query
itself evaluates another catalog UDF), every worker can park in `rx.recv()` waiting on
a task with no free worker to run it -> pool-exhaustion deadlock (hangs, no timeout).
At depth 3, or two sibling nested calls at once with 2 workers, it deadlocks.

Why we are safe today: the sub-queries are plain UDF-free scans
(`SELECT ... FROM pg_authid WHERE oid IN (...)`, `SELECT oid FROM pg_class WHERE
relname = '...'`), so nesting depth is always exactly 1. The whole thing rests on this
one assumption: catalog sub-queries stay UDF-free. The day a built-in view/UDF
resolves one catalog UDF via another, the bounded pool is the failure point; bumping
`worker_threads` only moves the cliff. The fix is Phase 4.

Note: the Phase 2 pivot (UDFs reading supplied definition text from the catalog at
call time) ADDS catalog sub-queries inside UDFs - so land the Phase 4 pool redesign
before, or alongside, any Phase 2 UDF that does a nested catalog lookup.

## Operational gotchas

- cargo isn't on PATH:
  `export PATH="/home/node/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin:$PATH"`
- Incremental-build corruption can cause a phantom `SIGSEGV` at process exit and
  `unknown relocation against stacker::grow` / `_Unwind_Resume` linker errors - NOT a
  code bug. Fix: `rm -rf target/debug/incremental && cargo clean -p datafusion_pg_catalog`,
  then rebuild. If tests segfault at exit but all report `ok`, suspect this first.
- `regenerate-catalog.sh` only rebuilds `gen_schema_ipc`, not the server binary. After
  code changes, `cargo build --release --bin datafusion_pg_catalog` yourself, or the
  standalone server / analyzer runs against a stale binary. (Tests use `cargo run`
  debug, so they're fine.)
- Start a server manually: build a zip from the schema dir, then run the binary:
  `.venv/bin/python -c "import shutil; shutil.make_archive('/tmp/s','zip','pg_catalog_data/pg_schema')"`
  then `./target/debug/datafusion_pg_catalog /tmp/s.zip --default-catalog pgtry --default-schema public --host 127.0.0.1 --port <PORT>`.
  Conn: `host=127.0.0.1 port=<PORT> dbname=pgtry user=dbuser password=pencil sslmode=disable`.
  Use the harness background mechanism (a bare `&` may not survive).
- The analyzer (`analyze_catalog_views.py`) regenerates `catalog_views_report.md` on
  demand against a running server; it is a build artifact, not committed.

## Validate ("all green" - CLAUDE.md: can't call it done without all tests)

- Rust: `cargo test` - expect 136 lib + integration bins, 0 failures.
- Python: `.venv/bin/python -m pytest tests/ -q` - expect 54 passed, 1 skipped.
  Do NOT set `RUST_LOG=off` for the full suite: the spawned server inherits it and
  `test_error_logging` greps the server log for `exec_error`. `RUST_LOG=off` is only
  for the snapshot test (`tests/test_view_output_snapshot.py`), to filter log spam.
