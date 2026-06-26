# Plan: turn materialized "views" into real views

136 objects are declared `type: view`. Originally the server registered only 6 with
`CREATE VIEW` and materialized the other 130 as `MemTable` snapshots - tables wearing
a view's name, answering `SELECT` from frozen seed rows and never re-deriving from
their base tables, so broken as views. This plan promotes them to real views, in
importance order, grouped by how hard that is.

**Progress: 36 of 136 are now real views** (Phase 0's 6 + Phase 1's 30 - 35 working
plus `pg_views` partial). Verified by `claude-scripts/audit_catalog_objects.py`
(served-as via `EXPLAIN`, content via `SELECT *` vs the PostgreSQL 17 snapshot, plus
a served-view column-name check) and the full Rust + Python test suites. 6 Phase-1
candidates are skipped on the `alias_N` rewrite bug (see below).

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

## Phase 1 - high value, base-tables-only, body matches snapshot (36) -> `done` (30) / `skip` (6)

Safest promotions: the body reads only base tables (which are live/registrable), so
the result is a genuinely live view, and it already reproduces PostgreSQL exactly.
Ordered by how often real tooling hits them.

**Done (30).** Added to `VIEWS_TO_REGISTER` in `src/session.rs`; the audit confirms
each is served as a view (`EXPLAIN` root `SubqueryAlias`), `SELECT *` reproduces the
snapshot, and the served view has correct column names.

**Skipped (6)** - all blocked by the same `alias_N` rewrite bug: a body that projects
a bare qualified column (`pg_authid.rolname`, `w.fdwoptions`) with no `AS` comes out
named `alias_N` in the served view, exposing wrong column names. `pg_roles` and the
four `_pg_foreign_*` helpers hit it directly; `pg_user_mappings`'s body failed to
plan and safely fell back to a table. Two layers caught this: the audit's served-view
column-name check (the `_pg_foreign_*` views are empty, so their row content matched
despite the bad names) and the `srf_views` Rust test. Unblocking them needs the
view-creation rewrite to preserve a bare `tbl.col`'s name - a follow-up engine task.

| View | Importance | Status |
|---|---|---|
| `pg_catalog.pg_indexes` | critical (psql `\d`, ORMs) | done |
| `pg_catalog.pg_roles` | critical (auth, `\du`) | skip - served view exposes `alias_N` columns (rewrite drops unqualified names for bare `tbl.col` refs); needs the view-creation rewrite to preserve column names |
| `pg_catalog.pg_matviews` | high | done |
| `pg_catalog.pg_shadow` | high (auth) | done |
| `pg_catalog.pg_user_mappings` | medium (FDW) | skip - listed but its `CREATE VIEW` body failed to plan, so it fell back to a table (safely, no crash); needs the same view-creation investigation as `pg_roles` |
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
| `information_schema._pg_foreign_data_wrappers` | low (internal helper) | skip - alias_N column bug (bare `w.col` projections); breaks consumers like foreign_data_wrapper_options |
| `information_schema._pg_foreign_servers` | low (internal helper) | skip - alias_N column bug |
| `information_schema._pg_foreign_table_columns` | low (internal helper) | skip - alias_N column bug |
| `information_schema._pg_foreign_tables` | low (internal helper) | skip - alias_N column bug |

---

## Phase 2 - reads other views, body matches snapshot (31) -> `done`

Same content correctness, but the body references other declared views, so it can
only become live once those resolve. The fixpoint retry in the mechanism handles the
ordering; the `pg_stat[io]_{sys,user}_*` family stays correct because their parents
(`pg_stat[io]_all_*`) are empty in a static catalog (0 rows in, 0 rows out).

| View | Importance | Depends on | Status |
|---|---|---|---|
| `information_schema.foreign_tables` | medium (FDW) | `_pg_foreign_tables` | pending |
| `information_schema.foreign_servers` | medium (FDW) | `_pg_foreign_servers` | pending |
| `information_schema.foreign_data_wrappers` | medium (FDW) | `_pg_foreign_data_wrappers` | pending |
| `information_schema.user_mappings` | medium (FDW) | `_pg_user_mappings` | pending |
| `information_schema.element_types` | high (array columns) | `data_type_privileges` | pending |
| `information_schema.data_type_privileges` | medium | attributes, columns, domains, parameters, routines | pending |
| `information_schema.foreign_table_options` | low | `_pg_foreign_tables` | pending |
| `information_schema.foreign_server_options` | low | `_pg_foreign_servers` | pending |
| `information_schema.foreign_data_wrapper_options` | low | `_pg_foreign_data_wrappers` | pending |
| `information_schema.user_mapping_options` | low | `_pg_user_mappings` | pending |
| `information_schema.column_options` | low | `_pg_foreign_table_columns` | pending |
| `information_schema._pg_user_mappings` | low (internal) | `_pg_foreign_servers` | pending |
| `information_schema.administrable_role_authorizations` | low | `applicable_roles` | pending |
| `information_schema.role_column_grants` | low | column_privileges, enabled_roles | pending |
| `information_schema.role_routine_grants` | low | routine_privileges, enabled_roles | pending |
| `information_schema.role_table_grants` | low | table_privileges, enabled_roles | pending |
| `information_schema.role_udt_grants` | low | udt_privileges, enabled_roles | pending |
| `information_schema.role_usage_grants` | low | usage_privileges, enabled_roles | pending |
| `pg_catalog.pg_user` | high (auth, `\du`) | `pg_shadow` | pending |
| `pg_catalog.pg_stat_sys_tables` | medium (monitoring) | `pg_stat_all_tables` | pending |
| `pg_catalog.pg_stat_user_tables` | medium | `pg_stat_all_tables` | pending |
| `pg_catalog.pg_stat_sys_indexes` | medium | `pg_stat_all_indexes` | pending |
| `pg_catalog.pg_stat_user_indexes` | medium | `pg_stat_all_indexes` | pending |
| `pg_catalog.pg_stat_xact_sys_tables` | low | `pg_stat_xact_all_tables` | pending |
| `pg_catalog.pg_stat_xact_user_tables` | low | `pg_stat_xact_all_tables` | pending |
| `pg_catalog.pg_statio_sys_tables` | low | `pg_statio_all_tables` | pending |
| `pg_catalog.pg_statio_user_tables` | low | `pg_statio_all_tables` | pending |
| `pg_catalog.pg_statio_sys_indexes` | low | `pg_statio_all_indexes` | pending |
| `pg_catalog.pg_statio_user_indexes` | low | `pg_statio_all_indexes` | pending |
| `pg_catalog.pg_statio_sys_sequences` | low | `pg_statio_all_sequences` | pending |
| `pg_catalog.pg_statio_user_sequences` | low | `pg_statio_all_sequences` | pending |

---

## Phase 3 - promote as a `partial` real view, body diverges in a known column (10) -> `done`

The body plans (so it becomes a real view), but one column diverges because of a
documented stub. Promoting them is still correct - they go from "fake table" to
"real view, one known-partial column" - and the divergence is the same stub gap
tracked elsewhere.

| View | Importance | Diverging column / reason | Status |
|---|---|---|---|
| `information_schema.views` | critical | `view_definition`/`is_updatable`/`is_insertable_into` (`pg_get_viewdef` stub) | pending |
| `pg_catalog.pg_rules` | medium | `definition` (`pg_get_ruledef` stub) | pending |
| `information_schema.check_constraints` | high | `check_clause` (`pg_get_constraintdef` stub) | pending |
| `information_schema.parameters` | high | `parameter_default` (`pg_get_function_arg_default` stub) | pending |
| `information_schema.applicable_roles` | medium | row count (role membership not fully modeled) | pending |
| `information_schema.column_privileges` | medium | empty (GRANTs not modeled) | pending |
| `information_schema.routine_privileges` | low | empty (GRANTs not modeled) | pending |
| `information_schema.table_privileges` | medium | empty (GRANTs not modeled) | pending |
| `information_schema.udt_privileges` | low | empty (GRANTs not modeled) | pending |
| `information_schema.usage_privileges` | low | partial (GRANTs not modeled) | pending |

---

## Phase 4 - blocked: `view_sql` errors on a fixable engine/UDF gap (9) -> `skip`

These cannot become real views until the planner/UDF gap is closed (registering
them now would abort startup). They stay materialized tables. Each is a separate
engine task; doing them here would balloon scope.

| View | Importance | Blocker | Status |
|---|---|---|---|
| `pg_catalog.pg_sequences` | critical | `pg_sequence_last_value()` not implemented | skip (needs UDF) |
| `pg_catalog.pg_stats` | high | `row_security_active()` not implemented | skip (needs UDF) |
| `pg_catalog.pg_policies` | medium | "Unsupported SQL type name" while planning | skip (planner gap) |
| `pg_catalog.pg_seclabels` | low | `pg_table_is_visible()` not implemented | skip (needs UDF) |
| `pg_catalog.pg_group` | medium | `pg_authid.oid` unresolved after subquery flattening | skip (rewrite bug - good small win later) |
| `pg_catalog.pg_available_extension_versions` | low | `GROUP BY` wildcard not planned (function-backed) | skip (planner gap) |
| `pg_catalog.pg_publication_tables` | low | sqlparser error on `ARRAY` literal | skip (parser gap) |
| `pg_catalog.pg_stats_ext` | low | `s.stxkeys` unresolved (column scoping) | skip (rewrite bug) |
| `pg_catalog.pg_stats_ext_exprs` | low | `pg_get_statisticsobjdef_expressions()` not implemented | skip (needs UDF) |

---

## Phase 5 - blocked: needs live server-runtime functions (41) -> `skip`

These read live process/IO/WAL/lock/progress state via server-runtime table
functions we do not host. Even as real views they would error or be permanently
empty, so they stay materialized tables (and most have an empty snapshot anyway).
Out of scope until those runtime functions exist. Full list with the missing
function per view: `claude-scripts/catalog_audit.md` (`runtime-function` rows). They
are: the `pg_stat_*` activity/db/wal/io/slru/subscription/replication/archiver/
bgwriter/checkpointer views, the six `pg_stat_progress_*`, the `pg_stat[io]_all_*`
parents, `pg_locks`, `pg_cursors`, `pg_prepared_statements`, `pg_prepared_xacts`,
`pg_file_settings`, `pg_wait_events`, `pg_backend_memory_contexts`,
`pg_shmem_allocations`, `pg_replication_slots`, `pg_replication_origin_status`.

---

## Phase 6 - blocked: merge append targets (3) -> `skip`

`register_user_relation` writes user objects into these with `append_catalog_row`;
if they were views that `INSERT` would fail and user-table registration would break.
Promoting them requires first redirecting registration to write only the base tables
(`pg_class`/`pg_attribute`/`pg_namespace`) and letting these derive - a separate,
larger change.

| View | Importance | Status |
|---|---|---|
| `information_schema.columns` | critical | skip (rework registration first) |
| `information_schema.tables` | critical | skip (rework registration first) |
| `information_schema.schemata` | high | skip (rework registration first) |
