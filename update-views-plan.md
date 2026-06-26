# Plan: turn materialized "views" into real views

136 objects are declared `type: view`. Originally the server registered only 6 with
`CREATE VIEW` and materialized the other 130 as `MemTable` snapshots - tables wearing
a view's name, answering `SELECT` from frozen seed rows and never re-deriving from
their base tables, so broken as views. This plan promotes them to real views.

Phases are ordered by priority: lower number = do sooner. Phase 1 is done; Phase 2
(the critical introspection views) and Phase 3 (remaining critical/high) are done.

**Progress: 126 of 136 are now real views** - 92 working plus 34 partial. The goal is
structural: every declared view should be served as a view, even when it derives 0
rows because the underlying tables/functions are not populated yet (that data is a
separate future iteration). The stat / live-state views (`pg_stat_*`, `pg_locks`,
`pg_cursors`, the `pg_stat_progress_*`, ...) are now real, empty-by-default views: the
runtime functions they call are registered as integration-installable resolvers
(`src/runtime_function_resolvers.rs`). **Every missing function is wired** - all 111
(92 scalar + 19 set-returning), each with its own explicit typed `set_<fn>_resolver`.
The full contract - every function, signature, and (for table functions) output schema
- is in `claude-scripts/missing_functions.md`. Verified by
`claude-scripts/audit_catalog_objects.py` and the full Rust + Python suites.

The `alias_N` rewrite bug is fixed: `restore_aliased_column_names`
(`src/clean_duplicate_columns.rs`) restores a view body's real column names before
`CREATE VIEW`.

**Remaining 10 not-yet-views - all blocked on engine/parser gaps, not functions:**
- `pg_user_mappings`, `information_schema.user_mapping_options` - `CREATE VIEW` fails.
- `pg_available_extension_versions` - `GROUP BY` wildcard expansion.
- `pg_group` - `pg_authid.oid` not exposed through the `ARRAY(subquery)` rewrite.
- `pg_policies` - unsupported SQL type name (`oid[]` cast).
- `pg_publication_tables` - `ARRAY` parser gap.
- `pg_statio_all_tables` - correlated `OuterReferenceColumn` has no physical plan
  (pulled back to a table on purpose: as a view it plans but errors at execution).
- `pg_stats` - `anyarray` column type.
- `pg_stats_ext`, `pg_stats_ext_exprs` - `int2vector` unnest, `anyarray`, composite
  field access. Their `pg_get_statisticsobjdef_expressions` / `pg_mcv_list_items`
  functions are wired; the views still need these engine features.

Source of the per-view facts: `claude-scripts/plan_view_promotion.py` (feasibility:
`view_sql` status, view-on-view dependencies, merge-target) and
`claude-scripts/catalog_audit.md` (served-as / status). Re-derive both after each
change.

## Mechanism

Promotion is driven by the `VIEWS_TO_REGISTER` allow-list in `src/session.rs`, grown
phase by phase. Each listed view is **attempted as a real `CREATE VIEW`, and falls
back to its materialized `MemTable` if the body fails to plan** - retried to a
fixpoint so view-on-view ordering resolves itself. The fallback means listing a view
can only upgrade it to a real view; a non-plannable body never crashes startup or
drops the object (it just stays a table). Promoting = "add it to the list and confirm
the audit shows it served as a view whose `SELECT *` matches the snapshot".

Why an allow-list rather than "attempt every declared view": attempting all 130
made startup re-plan ~50 unplannable bodies every boot (minutes of wasted work), and
the info_schema view-on-view bodies do not resolve under the pg_catalog-only view-body
schema anyway. The list keeps startup fast and promotion deliberate.

## Status legend (per view)
- `done` - now served as a real view (EXPLAIN root `SubqueryAlias`) and content
  verified against the PostgreSQL 17 snapshot (`working` or, where a known stub
  column diverges, `partial`).
- `pending` - eligible, not yet promoted / not yet verified.
- `skip` - left as a materialized table on purpose; reason given. The status is
  still correct (it is a table), it is just not worth or not possible to fix now.

---

## Phase 1 - high value, base-tables-only, body matches snapshot (36) -> `done` (35) / `skip` (1)

Safest promotions: the body reads only base tables (which are live/registrable), so
the result is a genuinely live view, and it already reproduces PostgreSQL exactly.
Ordered by how often real tooling hits them.

**Done (35).** Added to `VIEWS_TO_REGISTER` in `src/session.rs`; the audit confirms
each is served as a view (`EXPLAIN` root `SubqueryAlias`), `SELECT *` reproduces the
snapshot, and the served view has correct column names. This includes `pg_roles` and
the four `_pg_foreign_*` helpers, which the `alias_N` rewrite fix
(`restore_aliased_column_names`) unblocked.

**Skipped (1).** `pg_user_mappings` - its `CREATE VIEW` body does not plan (a separate
gap from `alias_N`), so it falls back to a materialized table.

| View | Importance | Status |
|---|---|---|
| `pg_catalog.pg_indexes` | critical (psql `\d`, ORMs) | done |
| `pg_catalog.pg_roles` | critical (auth, `\du`) | done (unblocked by the `alias_N` rewrite fix) |
| `pg_catalog.pg_matviews` | high | done |
| `pg_catalog.pg_shadow` | high (auth) | done |
| `pg_catalog.pg_user_mappings` | medium (FDW) | skip - `CREATE VIEW` body does not plan (separate gap from `alias_N`); stays a table |
| `information_schema.routines` | critical (ORMs, function introspection) | done |
| `information_schema.sequences` | critical (identity/serial) | done |
| `information_schema.domains` | high | done |
| `information_schema.attributes` | high (composite types) | done |
| `information_schema.triggers` | high | done |
| `information_schema.user_defined_types` | high | done |
| `information_schema.column_udt_usage` | high (type introspection) | done |
| `information_schema.column_domain_usage` | medium | done |
| `information_schema.constraint_table_usage` | medium | done |
| `information_schema.character_sets` | medium | done |
| `information_schema.collations` | medium | done |
| `information_schema.collation_character_set_applicability` | low | done |
| `information_schema.check_constraint_routine_usage` | low | done |
| `information_schema.column_column_usage` | low | done |
| `information_schema.domain_constraints` | low | done |
| `information_schema.domain_udt_usage` | low | done |
| `information_schema.enabled_roles` | low | done |
| `information_schema.information_schema_catalog_name` | low | done |
| `information_schema.routine_column_usage` | low | done |
| `information_schema.routine_routine_usage` | low | done |
| `information_schema.routine_sequence_usage` | low | done |
| `information_schema.routine_table_usage` | low | done |
| `information_schema.triggered_update_columns` | low | done |
| `information_schema.transforms` | low | done |
| `information_schema.view_column_usage` | low | done |
| `information_schema.view_routine_usage` | low | done |
| `information_schema.view_table_usage` | low | done |
| `information_schema._pg_foreign_data_wrappers` | low (internal helper) | done (unblocked by the `alias_N` rewrite fix) |
| `information_schema._pg_foreign_servers` | low (internal helper) | done |
| `information_schema._pg_foreign_table_columns` | low (internal helper) | done |
| `information_schema._pg_foreign_tables` | low (internal helper) | done |

---

## Phase 2 - merge-append targets (3) -> `done`

`information_schema.tables`, `information_schema.columns`, and
`information_schema.schemata` are the most-queried introspection views (ORMs, BI
tools, migration frameworks, `psql`). They are now real views deriving from
`pg_class`/`pg_attribute`/`pg_namespace`/`pg_authid`.

What it took:
- **Removed the dual-write** so nothing materializes them as tables: the eager
  `append_catalog_row` to `information_schema.tables`/`.columns` in
  `register_user_relation` (`src/pg_catalog_helpers.rs`), and the three
  `CatalogTable::InformationSchema*` entries from `LazyCatalogOptions::all()`
  (`src/lazy_catalog.rs`). Added the three to `VIEWS_TO_REGISTER` (`src/session.rs`).
- **Re-bind after lazy registration.** A DataFusion `ViewTable` stores the logical
  plan it was planned from, capturing the base-table providers present at
  `CREATE VIEW` time - it is not re-resolved per query. `register_lazy_catalog`
  swaps each base table's provider *after* the views are built, so the views kept
  reading the pre-swap (empty) providers and returned 0 rows for lazily-registered
  relations. Fix: `create_registered_views` records each `CREATE VIEW` statement in a
  per-session registry, and `replan_registered_views_against_current_providers`
  replays them after the provider swap so the views re-bind to the lazy providers.

Verified: audit shows all three `served_as=view` with correct column names (no
`alias_N`); `schemata` working, `tables`/`columns` partial only on the known stub
columns `is_insertable_into` / `is_updatable`; `lazy_pg_catalog` (27), the full Rust
suite, and the Python suite (53 passed, 1 skipped) all green.

| View | Importance | Status |
|---|---|---|
| `information_schema.columns` | critical (ORMs, BI, migrations) | done (partial: `is_updatable` stub) |
| `information_schema.tables` | critical (ORMs, BI, migrations) | done (partial: `is_insertable_into` stub) |
| `information_schema.schemata` | high | done |

---

## Phase 3 - remaining critical and high views -> `done` (6)

The most important not-yet-real views, attempted as `CREATE VIEW`, critical first then
high. Result of actually adding them:

**Critical**
| View | Result | Detail |
|---|---|---|
| `information_schema.views` | done (partial) | served as a view; diverges on `view_definition`/`is_updatable`/`is_insertable_into` (`pg_get_viewdef` stub) |
| `pg_catalog.pg_sequences` | done (working) | unblocked by registering `pg_sequence_last_value` (stub NULL default, installable resolver `set_pg_sequence_last_value_resolver`) |

**High**
| View | Result | Detail |
|---|---|---|
| `information_schema.element_types` | done (working) | served as a view; reproduces the snapshot exactly |
| `information_schema.check_constraints` | done (partial) | served as a view; diverges on `check_clause` (`pg_get_constraintdef` stub) |
| `information_schema.parameters` | done (partial) | served as a view; diverges on `parameter_default` (`pg_get_function_arg_default` stub) |
| `pg_catalog.pg_user` | done (working) | unblocked by the `alias_N` rewrite fix |

All 6 are in `VIEWS_TO_REGISTER`, verified by the audit (served as views, correct
column names) and the full Rust + Python suites. (`pg_stats`, also high-importance, is
in the lowest-priority `pg_stat*`/`pg_statio*` group in Phase 4 - it needs `anyarray`
type support.)

---

## Phase 4 - remaining medium and low views

Everything else, grouped by what blocks it. None is critical or high.

### Done - promoted (whose `view_sql` plans)
All views whose body planned are now served as views: the `foreign_*` family
(`foreign_tables`/`servers`/`data_wrappers` + their `_options`, `user_mappings`,
`_pg_user_mappings`), `data_type_privileges`, `administrable_role_authorizations`,
`column_options`, `pg_rules`, the `*_privileges` views, the `role_*_grants`, and the
`pg_stat[io]_{sys,user}_*` / `pg_stat_xact_{sys,user}_*` derivations. The privilege
views and the `role_*_grants` over them derive empty (GRANTs not modeled) - correct
for a real view; baselined in the snapshot test.

**Fell back (2)** - `view_sql` plans standalone but `CREATE VIEW` does not:
`pg_catalog.pg_user_mappings`, `information_schema.user_mapping_options`. They stay
tables until that is diagnosed.

### Blocked - fixable engine/UDF gap
Cannot be served as views until the planner/UDF gap is closed; each is a separate
engine task. They stay materialized tables.

| View | Importance | Blocker | Status |
|---|---|---|---|
| `pg_catalog.pg_policies` | medium | "Unsupported SQL type name" while planning | skip (planner gap) |
| `pg_catalog.pg_group` | medium | `pg_authid.oid` unresolved after subquery flattening | skip (rewrite bug - good small win later) |
| `pg_catalog.pg_seclabels` | low | `pg_table_is_visible()` not implemented | skip (needs UDF) |
| `pg_catalog.pg_available_extension_versions` | low | `GROUP BY` wildcard not planned (function-backed) | skip (planner gap) |
| `pg_catalog.pg_publication_tables` | low | sqlparser error on `ARRAY` literal | skip (parser gap) |

### Blocked - other live-state views
Report live process/lock/cursor/WAL state through set-returning server-runtime
functions we do not host. Inherently empty in a static catalog.

| View | Blocker |
|---|---|
| `pg_locks` | `pg_lock_status` |
| `pg_cursors` | `pg_cursor` |
| `pg_prepared_statements` | `pg_prepared_statement` |
| `pg_prepared_xacts` | `pg_prepared_xact` |
| `pg_file_settings` | `pg_show_all_file_settings` |
| `pg_wait_events` | `pg_get_wait_events` |
| `pg_backend_memory_contexts` | `pg_get_backend_memory_contexts` |
| `pg_shmem_allocations` | `pg_get_shmem_allocations` |
| `pg_replication_slots` | `pg_get_replication_slots` |
| `pg_replication_origin_status` | `pg_show_replication_origin_status` |

### Lowest priority - monitoring & statistics (`pg_stat*` / `pg_statio*`)
- **Done (empty derivations).** The `pg_stat[io]_{sys,user}_{tables,indexes,sequences}`
  and `pg_stat_xact_{sys,user}_*` views read the `pg_stat[io]_all_*` parents (empty
  MemTables), so their `view_sql` plans - they are now served as views returning 0
  rows. Correct: empty now, real data when the parents are.
- **Need set-returning runtime functions** (`pg_stat_get_*`, ...): the activity / db /
  wal / io / slru / subscription / replication / archiver / bgwriter / checkpointer
  views, the six `pg_stat_progress_*`, and the `pg_stat[io]_all_*` parents themselves.
  Same class as the live-state views above - their `view_sql` errors until those
  functions exist.
- **Planner statistics** needing engine work: `pg_stats` (`anyarray` type),
  `pg_stats_ext` (`s.stxkeys` column scoping), `pg_stats_ext_exprs`
  (`pg_get_statisticsobjdef_expressions()` missing).

Full per-view list with the exact blocker: `claude-scripts/catalog_audit.md`.
