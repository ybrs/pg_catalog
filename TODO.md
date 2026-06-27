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
`pg_type`, `pg_database`, `pg_index`, `pg_constraint`, and `pg_attrdef` (the
structural handle; default text is Phase 3), via two interchangeable paths that
share the same row-builders:
- eager / pre-register: `register_user_database`, `register_schema`,
  `register_user_tables` (emits `pg_attrdef` for defaulted columns),
  `register_user_view` (relkind `v`, shares the row-builders with
  `register_user_tables`), `register_user_index`, `register_user_constraint`.
- lazy / callback: a `LazyCatalogSource` implementation + `register_lazy_catalog`
  (a column's `ColumnSpec::with_default` drives its `pg_attrdef` row).

Still seed-only for user objects: `pg_rewrite`.

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
into "add optional definition fields to the registration API" - which is Phase 3
(gated by the Phase 2 pool redesign, since those UDFs do nested catalog lookups).

What stays structural (no deparse, fully ours to compute): a *plain* index's
`CREATE INDEX` text is determined by data we already hold
(`pg_index.indkey/indisunique`, `pg_class`, `pg_am`, `pg_attribute`); only
functional/partial index *expressions* need supplied text.

---

## Phase 1 - Finish structured user-object registration (no deparse)

Goal: a registered user table is fully introspectable from structured data alone.
The structured registration surface is complete: `pg_class`, `pg_type`,
`pg_attribute`, `pg_index`, `pg_constraint` (PK/UNIQUE/FK), and `pg_attrdef` (the
`atthasdef` flag + the `pg_attrdef` handle) are all populated by both the eager and
lazy paths through the shared row-builders.

- Column-fidelity long tail: the row-builders now fill the columns clients commonly
  read - `pg_class` (relam, relnatts, relfilenode, relpages, relchecks,
  relhassubclass, reltablespace, relfrozenxid, relminmxid, ...), `pg_type`
  (typbyval, typalign, typstorage, typinput/output/receive/send, typelem, typarray,
  ...), `pg_attribute` (attlen, attbyval, attalign, attstorage, attcollation,
  attidentity, attgenerated, attinhcount, ...). Remaining NULL columns are
  filled on demand as specific clients turn out to read them; `pg_database`/
  `pg_namespace` were already complete.

## Phase 2 - Engine robustness and scale

Reordered ahead of the deparse pivot (now Phase 3): the pool redesign gated it,
because each definition-text UDF does a nested catalog sub-query the old bounded
pool could not survive (see the catalog-UDF bridge section below).

- DONE - Catalog-UDF sync-over-async deadlock. The bounded 2-worker
  `CATALOG_QUERY_RT` is gone. `run_catalog_query` now spawns the sub-query as a
  task on the current multi-threaded runtime and blocks the caller inside
  `tokio::task::block_in_place`, so a blocked worker is handed back and the runtime
  grows its worker set with the nesting depth instead of parking a fixed pool -
  composed catalog UDFs cannot deadlock at any depth. (`#[tokio::test]` were moved
  to `flavor = "multi_thread"`, since `block_in_place` requires a multi-threaded
  runtime; production was already `#[tokio::main]`.) A depth-3 nesting test on a
  2-worker runtime guards the regression.
- Lazy-source filter pushdown: implement `supports_filters_pushdown` on
  `LazyCatalogTableProvider` and push equality filters (`relname=`, `nspname=`,
  `datname=`) into the source, so a large catalog is not fully re-enumerated per scan.
- Promote the static relation-listing views (`pg_indexes`, `pg_matviews`,
  `pg_sequences`) to live views over the registration where it makes sense -
  `VIEW_ONLY_TABLES` now covers `pg_tables`/`pg_views` plus the four constraint
  views (`table_constraints`, `key_column_usage`, `constraint_column_usage`,
  `referential_constraints`); the remaining relation-listing views are still
  static snapshots that do not reflect registered user objects.

## Phase 3 - Integration-supplied definition text (the deparse pivot)

Goal: views/constraints/defaults become fully introspectable for integrations that
provide the text, without us ever shipping a node-tree deparser. Depends on the
Phase 2 pool redesign - each UDF below reads supplied text via a nested catalog
lookup.

The mechanism is a process-wide resolver callback the integration installs (NOT a
stored row or a table): the relevant UDF resolves the object's identity from the
live catalog at call time, then asks the callback for the text, returning NULL when
no resolver is installed or it declines. The integration constructs whatever SQL it
likes; pg_catalog never deparses.

- DONE - view definitions. `set_view_definition_resolver(resolver)` installs a
  `Fn(&ViewIdentity) -> Option<String>`. `pg_get_viewdef(oid)` resolves each view
  OID to `(schema, name)` from `pg_class` and calls the resolver. Because the live
  `pg_views` view binds the UDF when its plan is created, the resolver-backed UDF is
  registered at session-construction time and reads the resolver slot at call time,
  so installing/changing the resolver later still flows through. Feeds
  `pg_views.definition` and `information_schema.views.view_definition`.
- DONE - functional/partial index expression text.
  `set_index_definition_resolver(resolver)` installs a `Fn(&IndexIdentity) ->
  Option<String>`. `pg_get_indexdef` renders plain indexes structurally as before
  and consults the resolver ONLY for indexes it cannot describe from the catalog
  (functional/partial), feeding the expression portion Phase 1 left NULL. The slot
  machinery is shared with the view resolver via a generic `DefinitionResolverSlot`.
- Still to do, same resolver pattern:
  - `is_updatable` / `is_insertable_into` flags (the `pg_relation_is_updatable` /
    `pg_column_is_updatable` stubs - the integration knows whether its views are
    updatable, so it supplies the flag).
  - check-constraint text and column-default text. Feeds
    `check_constraints.check_clause`, `pg_get_constraintdef`, `pg_get_expr`,
    `information_schema.columns.column_default`. Prerequisites first: `pg_constraint`
    has no `Check` constraint kind yet, and `information_schema.columns` user rows
    are materialized at registration (not a live view), so a call-time resolver only
    surfaces there once that path is promoted to live.
  - (rule definitions -> `pg_rules` / `pg_get_ruledef`: niche, defer.)

## Phase 4 - Session/GUC and SQL-surface compatibility

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
  auto-updatability), but per the Phase 3 pivot it should be integration-supplied;
  niche, leave stubbed with the precise snapshot baseline until then.

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

## Catalog-UDF sync-over-async bridge (deadlock RESOLVED, Phase 2)

Some scalar UDFs are synchronous but must run a catalog sub-query (`pg_get_userbyid`
-> `pg_authid`, `oid(text)` -> `pg_class`, `pg_get_indexdef` -> `pg_index`/...).
They bridge sync->async via `run_catalog_query` in `src/user_functions.rs`.

The old design spawned the sub-query on a dedicated bounded runtime
(`CATALOG_QUERY_RT`, `worker_threads(2)`) and blocked the caller on a std channel.
Because the caller blocked a worker instead of yielding it, the pool absorbed
nesting only `worker_threads` deep: if catalog UDFs composed (a sub-query that
itself evaluates a catalog UDF), every worker could park waiting on a task with no
free worker to run it -> pool-exhaustion deadlock at depth 3 (or two depth-2s).
Phase 3's "read supplied text at call time" plus the now-live constraint views
(which call UDFs) made that composition reachable.

Current design (no fixed pool):

```rust
fn run_catalog_query(future) -> T {
    Handle::current().spawn(async move { tx.send(future.await) });  // sub-query as a task
    tokio::task::block_in_place(move || rx.recv())                  // caller yields its worker
}
```

`block_in_place` transitions the blocked worker into a blocking thread and the
multi-threaded runtime spawns/borrows a replacement, so the scheduler keeps making
progress. Every nested level does the same, so the runtime grows its worker set
with the nesting depth instead of parking a fixed pool - composed catalog UDFs
cannot deadlock at any depth (bounded only by `max_blocking_threads`). Requires a
multi-threaded runtime; production is `#[tokio::main]` and tests use
`#[tokio::test(flavor = "multi_thread")]`. A depth-3 nesting test on a 2-worker
runtime (`test_run_catalog_query_nested_does_not_deadlock`) guards the regression.

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

- Rust: `cargo test` - expect 139 lib + integration bins, 0 failures.
- Python: `.venv/bin/python -m pytest tests/ -q` - expect 54 passed, 1 skipped.
  Do NOT set `RUST_LOG=off` for the full suite: the spawned server inherits it and
  `test_error_logging` greps the server log for `exec_error`. `RUST_LOG=off` is only
  for the snapshot test (`tests/test_view_output_snapshot.py`), to filter log spam.
