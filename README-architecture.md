# pg_catalog Architecture

This document describes the components of `pg_catalog` and, in detail, who does
what. It is meant to be read top-to-bottom by someone new to the codebase, and
also used as a reference map when changing a specific layer.

## 1. What this project is

`pg_catalog` is a PostgreSQL-compatibility layer built on top of Apache
DataFusion. It does two things:

1. It emulates PostgreSQL's system catalogs (`pg_catalog.*`) and
   `information_schema.*` as queryable tables/views backed by a captured
   snapshot of a real PostgreSQL instance.
2. It speaks the PostgreSQL wire protocol, so real PostgreSQL clients and tools
   (psql, DBeaver, IntelliJ, the JDBC/psycopg drivers, etc.) can connect and run
   their introspection queries unmodified.

The hard part is the gap between what those clients send and what DataFusion can
execute: PostgreSQL-specific syntax, casts (`::regclass`, `::oid`), set-returning
functions, correlated subqueries, and ~100 catalog functions DataFusion does not
have. The crate closes that gap with a SQL-rewriting layer, a set of emulated
UDFs, and an on-demand ("lazy") catalog.

It is consumed as a library by the sibling projects (`pg_catalog` is used by
`riffq` for Python pgwire compatibility) and depends on `corr-subq-udf-rs` for
correlated-subquery support inside DataFusion.

## 2. High-level architecture

```
   PostgreSQL client (psql / JDBC / psycopg / IntelliJ ...)
        |
        |  PostgreSQL wire protocol (TCP)
        v
  +-------------------------------------------------------------+
  |  server.rs        pgwire protocol, auth, type encoding      |
  |                   Simple + Extended query handlers          |
  +-------------------------------------------------------------+
        |  dispatch_query(ctx, sql, ...)
        v
  +-------------------------------------------------------------+
  |  router.rs        catalog-vs-user routing                   |
  |                   (does this query touch pg_catalog /        |
  |                    information_schema?)                      |
  +-------------------------------------------------------------+
        |  execute_sql(ctx, sql, ...)
        v
  +-------------------------------------------------------------+
  |  session.rs       session/context, parameter binding,       |
  |                   the SQL-rewrite pipeline, result shaping   |
  +------+-----------------------------+------------------------+
         |                             |
         | rewrite_filters(...)        | ctx.sql(...).collect()
         v                             v
  +--------------------+      +-----------------------------------+
  |  SQL rewriting     |      |  DataFusion engine                |
  |  replace.rs        |      |  + emulated catalog tables/views  |
  |  scalar_to_cte.rs  |      |  + emulated UDFs                  |
  |  clean_duplicate_  |      |  + optimizer rules                |
  |    columns.rs      |      +-----------------------------------+
  |  replace_any_      |             |               |
  |    group_by.rs     |             v               v
  |  logical_plan_     |   +------------------+  +-------------------------+
  |    rules.rs        |   | catalog data     |  | emulated functions      |
  +--------------------+   | lazy_catalog.rs  |  | user_functions.rs       |
                          | pg_catalog_      |  | runtime_function_       |
                          |   helpers.rs     |  |   resolvers.rs          |
                          | db_table.rs      |  +-------------------------+
                          | register_table.rs|
                          +------------------+
                                   ^
                                   | embedded at compile time
                          +------------------+
                          | Arrow IPC catalog |  (built offline from a
                          | snapshot artifact |   real PostgreSQL by the
                          +------------------+    Python data pipeline)
```

The crate is split into four layers, described in sections 4-7:

- Entry points and wire protocol (`main.rs`, `lib.rs`, `server.rs`, `router.rs`).
- The session/execution core (`session.rs`).
- The SQL-rewriting layer (`replace.rs`, `scalar_to_cte.rs`,
  `clean_duplicate_columns.rs`, `replace_any_group_by.rs`,
  `logical_plan_rules.rs`).
- Catalog data and emulated functions (`lazy_catalog.rs`,
  `lazy_pg_catalog_helpers.rs`, `pg_catalog_helpers.rs`, `db_table.rs`,
  `register_table.rs`, `user_functions.rs`, `runtime_function_resolvers.rs`).

## 3. The lifecycle of one query

1. A client connects. `server.rs` (`start_server`) accepts the TCP connection,
   runs the pgwire startup/auth handshake, and binds the connection to the shared
   `SessionContext`. The connecting user/database is recorded and a
   connection-scoped `current_database()` UDF is registered.
2. The client sends SQL, either via the Simple query protocol or the Extended
   (parse/bind/execute, i.e. prepared-statement) protocol. Both land in
   `DatafusionBackend::do_query`.
3. `server.rs` first handles "special" statements itself without touching
   DataFusion: `BEGIN`/`COMMIT`/`ROLLBACK`/`DISCARD` (no-ops in a single-catalog
   world), `SET` (stored on the session), and `SHOW` (read back from the session
   or answered with a fixed value such as `transaction_isolation = read
   committed`).
4. For everything else, `server.rs` calls `dispatch_query` (`router.rs`),
   passing a closure that runs the user-table path. `router.rs` parses the SQL
   and decides whether it references catalog objects (`pg_catalog.*`,
   `information_schema.*`, or a registered catalog function). Catalog queries are
   qualified (unqualified catalog tables get their schema prepended) and executed
   via `execute_sql`; non-catalog queries are handed to the caller's closure.
5. `execute_sql` (`session.rs`) calls `rewrite_and_execute_sql`, which: binds any
   prepared-statement parameters, runs the SQL-rewrite pipeline
   (`rewrite_filters` plus the SRF and EXISTS passes), plans and executes the
   rewritten SQL through DataFusion's `ctx.sql(...).collect()`, then renames
   columns back to their PostgreSQL names and hides virtual system columns
   (`xmin`, `ctid`, ...).
6. DataFusion resolves catalog tables against the emulated providers
   (`lazy_catalog.rs` / the registered MemTables) and catalog functions against
   the emulated UDFs (`user_functions.rs`, `runtime_function_resolvers.rs`), and
   applies the registered optimizer rule (`StripPgGetOne`).
7. The resulting Arrow `RecordBatch`es flow back to `server.rs`, which maps Arrow
   types to PostgreSQL type OIDs (`arrow_to_pg_type`) and streams the rows to the
   client as pgwire `DataRow`s. If query capture is enabled, the query and its
   result are also written to a YAML file.

## 4. Entry points and wire protocol

### main.rs
The binary entry point. `run()` parses CLI flags
(`--schema-directory`, `--default-catalog`, `--default-schema`, `--host`,
`--port`, `--capture`), builds the session via `get_base_session_context`,
registers a demo `public.users` table, and starts the server with
`start_server`. This file is also the smallest worked example of how to embed the
library.

### lib.rs
The public API surface. It re-exports the modules and the three primary entry
points a library consumer needs:

- `start_server` (from `server`) - run the pgwire server over a context.
- `dispatch_query` (from `router`) - route+execute a single SQL string.
- `get_base_session_context` / `get_base_session_context_with_lazy_catalog` /
  `build_ipc_artifact` (from `session`) - build a ready-to-use context, and build
  the embedded artifact.

It also re-exports the catalog registration helpers (`pg_catalog_helpers`,
`lazy_catalog`, `lazy_pg_catalog_helpers`), the UDF registrations
(`user_functions`), and the runtime resolvers (`runtime_function_resolvers`).

### server.rs
Owns the PostgreSQL wire protocol. It wraps the `pgwire` crate and implements its
handler traits.

- `DatafusionBackend` is the per-server handler. It holds the shared
  `SessionContext` and an optional capture store, and implements both
  `SimpleQueryHandler` and `ExtendedQueryHandler` (`do_query`,
  `do_describe_statement`, `do_describe_portal`).
- `start_server` binds the TCP listener and serves connections; a
  `DatafusionBackendFactory` produces handlers and a dummy auth source (all users
  authenticate with a fixed password; this is a read-only compatibility shim, not
  a security boundary).
- Result encoding: `arrow_to_pg_type` maps Arrow `DataType` to a PostgreSQL
  `Type`; `batch_to_field_info` builds the row description; `batch_to_row_stream`
  encodes each `RecordBatch` into a stream of pgwire `DataRow`s. `decode_parameters`
  turns Extended-protocol binary parameter bytes into DataFusion `ScalarValue`s.
- `CaptureStore` optionally records every query, its parameters, and its result
  rows (as JSON) to a YAML file (`--capture`), which the Python test suite replays
  and asserts against. `batches_to_json_rows` is the capture-side encoder.
- Special-command handling for transaction control and `SET`/`SHOW` lives here so
  these never reach the planner.

### router.rs
Decides, per statement, whether a query is a catalog query or a user query.

- `dispatch_query` is the entry point. It parses the SQL, and if any statement
  references a catalog object it qualifies unqualified catalog table names and
  runs the query through `execute_sql`; otherwise it awaits the user-supplied
  handler closure.
- Detection helpers walk the parsed AST: `resolve_schema` resolves a table name
  against the `search_path` (always treating `pg_catalog` as present),
  `object_is_catalog` / `function_is_catalog` decide whether a name belongs to a
  catalog schema, and the `*_has_catalog` family recurses through joins, CTEs, and
  subqueries. `schema_is_catalog` and `function_registered` are the shared
  predicates these use.

## 5. The session and execution core: session.rs

`session.rs` is the largest and most central module. It builds the session,
loads the catalog, owns the rewrite pipeline, and shapes results. It splits into
two concerns: setup and per-query execution.

### Session setup
- `get_base_session_context` (and the lazy variant) build a DataFusion
  `SessionContext`, configured with a `ClientOpts` config extension (per-session
  `application_name`, `datestyle`, `search_path`).
- `parse_schema` loads the catalog. With no schema path it uses the fast path:
  `SCHEMA_IPC` is an Arrow IPC zip embedded into the binary at compile time via
  `include_bytes!`, parsed by `parse_schema_ipc_bytes` (~tens of ms versus
  ~seconds for parsing YAML). A YAML file/dir/zip path is also accepted for
  development.
- `register_catalogs_from_schemas` populates DataFusion's catalog/schema tree,
  installing each catalog table as a `ScanRecordingMemTable` and collecting the
  list of views to create.
- `create_registered_views` turns declared views into live `CREATE VIEW`s where
  the engine can plan their bodies, and falls back to materializing the captured
  snapshot rows as a table where it cannot (`register_view_as_table`). It runs as
  a fixpoint retry loop to satisfy view-to-view dependencies. `VIEWS_TO_REGISTER`
  lists the views attempted as live views; `SIMPLIFIED_VIEW_BODIES` supplies
  engine-friendly bodies for a few views whose real definitions the engine cannot
  yet plan.
- All the emulated UDFs are registered here (the many `register_*` calls into
  `user_functions` and `runtime_function_resolvers`), and `StripPgGetOne` is
  added as an optimizer rule.
- `build_ipc_artifact` is the inverse of the load path (YAML zip -> Arrow IPC
  zip); it is what `bin/gen_schema_ipc.rs` calls to regenerate the embedded
  artifact.

### Per-query execution
- `execute_sql` is the entry point used by the router and the server fallback. It
  delegates to `rewrite_and_execute_sql` and logs (never swallows) errors.
- `rewrite_and_execute_sql` binds parameters, runs the rewrite pipeline, plans and
  collects the query, then calls `plan_collect_and_rename` and
  `remove_virtual_system_columns` to restore PostgreSQL column names and hide
  system columns unless explicitly selected.
- `rewrite_filters` is the ordered SQL-rewrite pipeline (see section 6.6).
- `rows_to_record_batch` materializes JSON catalog rows into Arrow batches; it is
  the bridge used by both the lazy catalog and the eager registration helpers.

## 6. The SQL-rewriting layer

PostgreSQL clients send SQL that DataFusion cannot execute verbatim. These
modules rewrite the parsed AST (using `sqlparser`) or add DataFusion planner
rules. Each rewriter solves one narrow problem; they are chained in a fixed order
by `session.rs`.

### 6.1 replace.rs
The bulk of the rewriters (~30 functions). They share one skeleton,
`rewrite_each_expression(sql, |expr| ...)`, which parses, walks every expression
in every statement, applies a transformation in place, and re-renders. Grouped by
theme:

- regclass / oid casts: `replace_regclass` (turns `'x'::regclass` and
  `::regclass::oid` into function calls), `rewrite_oid_cast`,
  `drop_redundant_oid_and_regclass_casts`, `drop_oid_array_cast`.
- custom type casts to a DataFusion type: `rewrite_custom_type_cast_target` is the
  data-driven core; `rewrite_regtype_cast`, `rewrite_char_cast`,
  `rewrite_xid_cast`, `rewrite_name_cast`, `rewrite_regoper_cast`,
  `rewrite_regoperator_cast`, `rewrite_regproc_cast`, `rewrite_regprocedure_cast`
  are thin callers; `rewrite_text_backed_type_casts` handles `anyarray`/`name[]`.
- schema-qualified types: `rewrite_schema_qualified_text`,
  `rewrite_schema_qualified_custom_types`, `rewrite_schema_qualified_udtfs`.
- information_schema domain types: `rewrite_information_schema_casts`.
- EXISTS / subquery shape: `rewrite_exists_to_count` (rewrites `EXISTS(subq)` to a
  counted scalar subquery so DataFusion can decorrelate it),
  `rewrite_tuple_in_subquery_to_exists`, `rewrite_array_subquery`.
- set-returning functions: `rewrite_srf_to_unnest` (rewrites `(srf(x)).field`
  projections to an `unnest(...)` form).
- operators / literals / function-specific: `rewrite_pg_custom_operator`,
  `rewrite_array_agg_varchar_cast`, `rewrite_brace_array_literal`,
  `rewrite_tuple_equality`, `rewrite_pg_truetypid_composite_args`,
  `decorrelate_lateral_aggregate`.
- administrative: `replace_set_command_with_namespace`, `rewrite_time_zone_utc`,
  `strip_default_collate`, `alias_subquery_tables`.
- `object_name_matches` is the shared helper that decides whether an AST object
  name matches a type name (bare or `pg_catalog`-qualified).

### 6.2 scalar_to_cte.rs
Converts correlated scalar subqueries in the projection into `WITH` CTEs that are
`LEFT JOIN`ed back to the outer query, removing the correlation barrier so
DataFusion's optimizer can see the join. `rewrite_scalar_subqueries_to_ctes` is
the entry point (`rewrite_subquery_as_cte` is the convenience wrapper used by the
pipeline). Internally a read-only `ScalarFinder` collects scalar subqueries in the
projection, then a mutating `ScalarToCte` rewriter extracts correlation
predicates, builds the CTE (auto-injecting `GROUP BY` when it mixes aggregates and
grouping columns), and replaces each projection expression with a reference to the
CTE column.

### 6.3 clean_duplicate_columns.rs
Ensures projection column names are unique so the DataFusion optimizer does not
trip on name collisions. `disambiguate_duplicate_columns` renames duplicate names
inside nested selects; `alias_unnamed_columns` gives every unnamed top-level
projection a stable `alias_N` name and returns a map; `restore_aliased_column_names`
puts the real PostgreSQL names back (used when registering a view body and when
shaping results).

### 6.4 replace_any_group_by.rs
`rewrite_group_by_for_any` adds the column referenced inside a `literal = ANY(col)`
predicate to the `GROUP BY` when the query already groups, so such queries pass
semantic analysis.

### 6.5 logical_plan_rules.rs
`StripPgGetOne` is a DataFusion `OptimizerRule` (applied bottom-up) that unwraps
`pggetone(<expr>)` to `<expr>`. `pggetone` is a marker the rewrites use to force a
scalar context; this rule removes it after planning so the planner never sees the
wrapper. It is registered in `session.rs` setup.

### 6.6 Rewrite order
Order matters and is fixed in `session.rs`:

1. `rewrite_srf_to_unnest` runs first (before `rewrite_filters`) so the later
   `GROUP BY` heuristic can see the unnest markers.
2. `rewrite_filters` then applies the cast/operator/schema/alias rewriters and
   finally the scalar-subquery-to-CTE rewrite, in sequence.
3. A post pass applies `rewrite_exists_to_count` before
   `rewrite_tuple_in_subquery_to_exists` (the IN rewrite depends on the EXISTS
   form existing).
4. `rewrite_group_by_for_any` runs near the end.
5. `StripPgGetOne` runs later still, during DataFusion planning, not on SQL text.

## 7. Catalog data and emulated functions

### 7.1 lazy_catalog.rs
The on-demand catalog. Instead of materializing every catalog row up front, a
backend can supply a `LazyCatalogSource` whose callbacks (`databases`, `schemas`,
`relations`, `columns`, `indexes`, `constraints`, `config`, `settings`) are
invoked on every scan. `LazyCatalogTableProvider` implements DataFusion's
`TableProvider`: on each scan it pulls fresh rows from the source, builds Arrow
rows, and merges them with the immutable built-in rows captured at registration,
with user rows winning by a per-table merge key (for example `pg_class` keys on
`relnamespace` + `relname`).

The data model is a set of typed row definitions - `DatabaseDef`, `SchemaDef`,
`RelationDef` (`RelationKind` = Table/View/MaterializedView), `IndexDef`,
`ConstraintDef`, `ColumnSpec` - and the `build_pg_*_row` builders
(`build_pg_class_row`, `build_pg_namespace_row`, `build_pg_attribute_rows`,
`build_pg_index_row`, `build_pg_constraint_row`, the `information_schema`
builders, ...) that turn each definition into the columns of the corresponding
catalog table. `build_rows_for` walks the source hierarchy and dispatches to those
builders per `CatalogTable`. OIDs are always supplied by the source and written
verbatim - the catalog never invents or caches them. `register_lazy_catalog`
installs the providers (capturing the current built-in rows first) and replans the
views so they bind to the lazy providers; `LazyCatalogOptions` chooses which
catalog tables are made lazy.

### 7.2 lazy_pg_catalog_helpers.rs
A thin compatibility wrapper for the database-only case. `LazyDatabaseRow` models
all `pg_database` columns (mandatory `oid`/`datname`/`datdba`, the rest optional
with PostgreSQL defaults). `DatabaseOnlySource` adapts a simple
`Fn() -> Vec<LazyDatabaseRow>` callback to the full `LazyCatalogSource` trait, and
`register_user_database_with_callback` registers just `pg_database` lazily, merging
user databases with the built-in `postgres`/`template0`/`template1`.

### 7.3 pg_catalog_helpers.rs
The eager (DDL-like) registration path: synchronously insert user databases,
schemas, tables, views, indexes, and constraints into the in-memory catalog
MemTables. It owns OID allocation (a process-global `NEXT_OID` atomic) and the
`DATABASE_SCHEMAS` registry that tracks which schemas belong to which database for
cleanup. Key entry points: `register_user_database`, `register_schema`,
`register_user_tables` / `register_user_view` (via `register_user_relation`, which
writes the matching `pg_class` + `pg_type` + `pg_attribute` + `pg_attrdef` rows),
`register_user_index`, and `register_user_constraint`; each has an `unregister_*`
inverse. Rows are inserted with `append_catalog_row` (stage as a one-off MemTable,
`INSERT ... SELECT`, drop) because literal `INSERT VALUES` cannot express complex
columns such as `pg_index.indkey`. Lookups (`get_schema_oid`, `get_table_oid`,
`first_oid_cell`, `collect_string_column`) and the type<->OID maps
(`map_type_to_oid`, `oid_to_type_names`) round it out. Schemas are scoped by OID,
not name, so the same schema name can exist under different databases in the
flattened catalog.

### 7.4 db_table.rs
`map_pg_type` maps a PostgreSQL type name to an Arrow `DataType` (for example
`oidvector` -> `List<Int64>`, `_int4` -> `List<Int32>`, `float4` -> `Float32`,
`int2`/`int4` -> `Int32`), keeping integer arrays integer-typed so predicates like
`attnum = ANY(conkey)` compare like with like. `ScanRecordingMemTable` wraps a
DataFusion `MemTable` and records every scan (table, projection, filters) into a
shared `ScanTrace` log for test introspection; `log_scan_traces` serializes it.

### 7.5 register_table.rs
`register_table` is the small helper that creates a catalog/schema if missing and
registers a new empty `MemTable` for application data (the table user `INSERT`s
then populate), as opposed to the system catalog tables.

### 7.6 user_functions.rs
The library of emulated PostgreSQL scalar and table functions (the largest single
file). Grouped by category:

- session identity: `current_user` / `session_user` / `current_role` read a
  mutable `SESSION_USER` slot at call time (`set_session_user`,
  `register_session_identity`), so eagerly-planned views capture the UDF, not a
  frozen value.
- relation/OID lookup: `oid(text)` resolves a name to a `pg_class` OID
  (`register_scalar_regclass_oid`); `pg_get_userbyid` resolves role OIDs to names
  in a single batched catalog query (`fetch_users_by_oids`).
- definition functions: `pg_get_indexdef`, `pg_get_viewdef`, `pg_get_expr`,
  `pg_get_partkeydef`. The index/view definition functions read a process-global
  definition-resolver slot (`set_index_definition_resolver`,
  `set_view_definition_resolver`) at call time, so integrations can supply the
  real DDL text; with none installed they return a stub.
- type introspection: `format_type`, the `information_schema._pg_*` helpers
  (`_pg_char_max_length`, `_pg_numeric_precision`, `_pg_truetypid`/`_pg_truetypmod`,
  ...), which are pure functions of an OID + typmod.
- string formatting: `format(fmt, ...)` implementing `%s`/`%I`/`%L`/`%%`.
- privilege / visibility / membership stubs that always succeed for the emulated
  single superuser: the `has_*_privilege` family
  (`register_has_privilege_family`, plus `register_always_true_object_privilege`
  shared by `has_database_privilege`/`has_schema_privilege`), `pg_has_role`,
  `pg_table_is_visible` / `pg_type_is_visible`.
- assorted compatibility stubs: `pg_is_other_temp_schema` (false),
  `pg_my_temp_schema` (0), `getdatabaseencoding` (UTF8),
  `pg_tablespace_location` (NULL), `pg_relation_size` / `pg_total_relation_size`
  (0 via `register_zero_relation_size`), `pg_relation_is_updatable`,
  `pg_column_is_updatable`, `pg_relation_is_publishable`.

`run_catalog_query` is the shared bridge that lets a synchronous UDF body run a
catalog query (a future) to completion without deadlocking, on either a
multi-thread or current-thread runtime.

### 7.7 runtime_function_resolvers.rs
A process-global resolver-slot mechanism for functions whose values are live
runtime state rather than static catalog data. Each function has a `ResolverSlot`;
integration code installs a callback with `set_<name>_resolver(...)`, and with no
callback installed the function returns NULL (scalar) or no rows (table). Two
macro-generated families keep the per-function plumbing to one declarative line
each:

- `scalar_resolvers!` generates the visibility predicates and the many
  `pg_stat_get_*` accessors (one boxed `DynScalarUdf` per function).
- `table_resolvers!` generates the activity/lock/replication/IO set-returning
  views (`pg_stat_get_activity`, `pg_lock_status`, `pg_get_replication_slots`,
  `pg_stat_get_io`, ...), each backed by a `DynTableUdf` that materializes the
  resolver's rows into a `MemTable` on each call.

`register_all_scalar_resolvers` and `register_all_table_resolvers` wire them into
the context. See `docs/runtime-functions-reference.md`, which is generated from
this file.

## 8. The catalog data pipeline (offline)

The catalog the server serves is a captured snapshot of a real PostgreSQL
instance, processed into an embedded Arrow IPC artifact. The pipeline is offline
(run when refreshing the snapshot), orchestrated by `regenerate-catalog.sh` and
the `Makefile`:

```
  live PostgreSQL 17 (download_postgresql.sh, run-postgres.sh; port 5434)
     |   schema.py generate
     v
  per-object YAML  (pg_catalog_data/pg_schema/*.yaml, ~211 files)
     |   patch_views.py   (rewrite views the extractor cannot express)
     v
  postgres-schema-nightly.zip      (human-editable YAML source)
     |   cargo run --bin gen_schema_ipc  ->  build_ipc_artifact()
     v
  postgres-schema-nightly-ipc.zip  (Arrow IPC; embedded via include_bytes!)
     |   build_snapshot_db.py
     v
  view_snapshots.duckdb            (precomputed view rows, for fast tests)
```

- `download_postgresql.sh` fetches a pinned PostgreSQL 17 build;
  `run-postgres.sh` initializes it deterministically (fixed encoding and bootstrap
  superuser, so ownership/ACLs do not depend on the host account).
- `schema.py` connects to that instance and writes one YAML file per catalog
  object under `pg_catalog_data/pg_schema/`. Each file nests
  `catalog -> schema -> object -> {type, schema, pg_types, rows, view_sql}`. It maps
  PostgreSQL types to a simplified set (keeping `float4`/`float8` distinct to
  preserve wire OIDs 700/701) and emits empty `rows` for volatile runtime tables
  so the snapshot is deterministic.
- `patch_views.py` idempotently rewrites views the raw extraction cannot model
  (unsupported SRFs flipped to base tables, a correlated CASE subquery in
  `information_schema.table_constraints` collapsed to a constant, a LATERAL SRF in
  `information_schema.user_mapping_options` rewritten to projection form).
- `bin/gen_schema_ipc.rs` calls `build_ipc_artifact` to convert the YAML zip into
  the Arrow IPC zip that `session.rs` embeds and fast-loads.

### Data locations
- `pg_catalog_data/pg_schema/*.yaml` - the per-object snapshot (the source of
  truth, human-readable).
- `pg_catalog_data/postgres-schema-nightly.zip` - zipped YAML.
- `pg_catalog_data/postgres-schema-nightly-ipc.zip` - the embedded Arrow IPC
  artifact.
- `pg_catalog_data/view_snapshots.duckdb` - precomputed view rows, loaded by the
  snapshot test for speed (gitignored, rebuilt on regen).
- `queries.yaml`, `captures/*.yaml` - real client queries (from PostgreSQL logs
  and from server-side capture) used by the test suites.

## 9. Python tooling

`yaml_loader.py` is the shared library every tool imports: `load_yaml` (libyaml
fast path), `walk_catalog_objects` (yield `(schema, name, node)` for each catalog
object), and `find_in_doc`. The tools (run from the repo root) are:

- `schema.py` - extract the live catalog to YAML (pipeline stage 3).
- `patch_views.py` - post-process views (pipeline stage 4).
- `build_snapshot_db.py` - precompute the DuckDB view snapshot (pipeline stage 7).
- `validate_pg_catalog_views.py` - run every view's SQL against a live server and
  report pass/missing-function/error. Its `ViewDefinition` / `run_view_query`
  helpers are also imported by a unit test.
- `analyze_catalog_views.py` - the same, with finer error classification, emitting
  `catalog_views_report.md`.
- `gen_failing_views_doc.py` - group failing views by blocking symbol into
  `catalog-failing-views.md`.
- `extract_queries_from_postgres_logs.py` - parse PostgreSQL logs into a normalized,
  deduplicated `queries.yaml`.
- `claude-scripts/` (run as `python -m claude-scripts.<name>` from the root):
  `audit_catalog_objects.py` (machine-checked inventory: executes + compares to the
  snapshot, mirroring the snapshot test), `plan_view_promotion.py` (facts for
  promoting materialized views to live views), `missing_functions_report.py`
  (functions still blocking views), `generate_runtime_function_reference.py`
  (generate the resolver reference doc from `runtime_function_resolvers.rs`).

## 10. Test suites

### Rust (`cargo test`)
Unit tests live next to the code in `src/` (notably the registration tests in
`pg_catalog_helpers.rs` and the rewriter tests in `replace.rs` /
`scalar_to_cte.rs`). Integration tests under `tests/` drive the library directly
through a shared `base_ctx()` helper:

- `lazy_pg_catalog.rs` - lazy catalog registration, metadata callbacks, and
  runtime relation appending.
- `scalar_catalog_udfs.rs` - UDFs that run nested catalog queries from a sync body,
  on both runtime flavors (the `run_catalog_query` no-deadlock guarantee).
- `public_api.rs` - `dispatch_query` / `get_base_session_context` and result types
  (for example that `pg_class.reltuples` stays `float4`).
- `has_privilege_family.rs`, `pg_get_userbyid.rs`, `pg_has_role.rs`,
  `pg_is_other_temp_schema.rs`, `scalar_stubs.rs`, `srf_views.rs` - focused tests
  for the corresponding emulated functions and SRF views.

### Python (`pytest`)
`tests/conftest.py` owns the server-spawn machinery: it starts the server through
the shipped fast-load path (`cargo run -- ""` selects the embedded IPC artifact)
on `127.0.0.1`, shares one session-scoped server for read-only tests, and exposes
`pg_server(...)` for tests that need their own process (capture, log piping).
`conn_str(port)` centralizes the DSN. The suites:

- `test_view_output_snapshot.py` - the main regression test: run every view's SQL
  and compare execution, row count, and row content against the captured snapshot,
  with explicit baselines of known divergences (a new divergence, or a baselined
  view that starts passing, fails the test).
- `test_functional.py` - end-to-end behavior over pgwire (types, arrays,
  parameterized queries, error logging, capture).
- `test_captures.py` - replay captured queries (currently skipped).
- `test_datacl_capture.py` - assert the shape of captured `pg_database.datacl`
  privilege arrays.
- `test_validate_pg_catalog_views.py` - unit tests for the view-validation helpers.

Because the Python tests launch the server via `cargo run`, `cargo` must be on
`PATH` when running them.

## 11. Cross-cutting design decisions

- Single source of truth is a real PostgreSQL snapshot. Behavior is captured, not
  hand-written, and regression-tested row-for-row against that capture.
- Never fail silently. Unparseable SQL and unexpected types return errors to the
  client rather than falling back to a guess; the test suites fail loudly on a new
  divergence.
- OID stability. OIDs come from the captured data or the caller and are written
  verbatim; the catalog does not invent or cache them.
- Fix the gap in SQL, not in the engine where possible. Most compatibility lives
  in the `sqlparser`-level rewrite passes (small, independently testable
  functions) rather than in DataFusion internals; only `StripPgGetOne` is a
  planner rule.
- Read at call time, not registration time. Session identity and runtime functions
  read mutable slots when invoked, so views planned once still reflect the current
  connection and live state.
- Lazy and eager catalog paths coexist. The lazy path re-derives rows from a source
  on every scan (no caching); the eager path inserts rows once. Integrations pick
  per catalog table.
- Fast startup. The catalog is embedded as Arrow IPC and loaded in-process, so a
  cold start does not parse YAML.
</content>
</invoke>
