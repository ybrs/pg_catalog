# pg_catalog reference - tables, views, registration & status

Single source of truth for **what lives in the catalog**, **what each object is
for**, **how it is populated** (seed vs. runtime registration), and **whether it
actually works today**. Verified 2026-06-26.

- **75 base tables** (71 `pg_catalog` + 4 `information_schema`) - all served as
  tables and queryable (**75 working**).
- **136 objects declared `type: view`** - **126 are now actually served as views**
  (92 working + 34 partial); only **10 are still materialized as tables**, every one
  blocked on an engine/parser gap rather than a missing function. The stat /
  live-state views became real (empty) views once their runtime functions were
  registered as integration-installable resolvers; every missing function (111 total)
  is now wired - see [`update-views-plan.md`](update-views-plan.md) and
  `src/runtime_function_resolvers.rs`.

Every status here is machine-checked, not asserted. The numbers come from
`claude-scripts/audit_catalog_objects.py`, which enumerates every object, reads
from the live server how each is **served** (real view vs materialized table, via
`EXPLAIN`), runs each view's `view_sql`, and compares the output (count + content)
to the captured PostgreSQL 17 snapshot. Regenerate with (server on port 5444):
```bash
.venv/bin/python -m claude-scripts.audit_catalog_objects \
    --conn "host=127.0.0.1 port=5444 dbname=pgtry user=dbuser password=pencil sslmode=disable"
```
The exhaustive per-object table lives in `claude-scripts/catalog_audit.md`; this
file is the curated summary.

### Status legend
| status | meaning |
|---|---|
| working | A real view (or base table) that executes and reproduces PostgreSQL output. |
| partial | A real view that executes, but a named column diverges by design. |
| broken | The object does not work as declared. For a `type: view` this includes being **served as a materialized table** (it does not re-derive from base tables), as well as a defining SQL that does not execute. |

---

## How objects are populated

There are two layers, and for the dynamic tables, two interchangeable runtime paths.

### 1. Seed (built-in rows)
Every base table is loaded at startup from `pg_catalog_data/pg_schema/*.yaml`,
dumped from a real PostgreSQL 17.4. For the immutable system catalogs (`pg_type`,
`pg_proc`, `pg_operator`, `pg_am`, ...) this seed **is** the data. Declared views are
also seeded with materialized `rows:` used as test fixtures.

### 2. Runtime registration (a live database's own objects)
When something fronts a real database (e.g. `riffq`), the user's tables/columns/
indexes are injected into the catalog at runtime. Two paths write the **same**
catalog tables; pick per embedding:

| Path | Entry points | How it works |
|---|---|---|
| **Eager / pre-register** | `register_user_database`, `register_schema`, `register_user_tables`, `register_user_index`, `register_user_constraint` | Imperatively writes rows now (INSERT / batch-append). Use when objects are known up front. |
| **Lazy / callback** | `register_lazy_catalog(source, opts)` + `register_user_database_with_callback` | The embedder implements `LazyCatalogSource`; on **every scan** the catalog calls back (`databases()`/`schemas()`/`relations()`/`columns()`/`indexes()`/`constraints()`/`config()`/`settings()`), builds rows, and merges them over the seed. Use when the live set changes. |

Both paths share the same pure row-builders (`build_pg_class_row`,
`build_pg_index_row`, ...) so the two stay in sync.

### How a declared view is actually served (the core defect)

A YAML object with `type: view` is **only a real view if the server registers it
with `CREATE VIEW`** - `src/session.rs:1060-1090` does this for exactly the six
names in `VIEW_ONLY_TABLES` and **materializes every other declared view as a
`ScanRecordingMemTable`** (a table) seeded from its YAML `rows:`. So a "view" not in
that list is a **frozen snapshot - a view in name only**: it is queryable but holds
seed/fixture rows and does **not** re-derive from its base tables. This is read
empirically here from each object's query plan (`EXPLAIN SELECT *`): a real view's
plan root is `SubqueryAlias: <name>` over the base tables; a materialized one's is
`TableScan: <name>`.

By that test, of the 136 declared views **only 6 are served as views**; the other
**130 are served as tables** (or fail to scan) and are counted **broken** below - a
view that is physically a table is broken, however cleanly it answers a `SELECT`.

- **view (real)** - the six in `VIEW_ONLY_TABLES`: `pg_tables`, `pg_views`, and the
  four constraint views (`table_constraints`, `key_column_usage`,
  `constraint_column_usage`, `referential_constraints`, over the registrable
  `pg_constraint`). Re-derive from base tables on every query; reflect runtime
  user objects.
- **table (materialized) - BROKEN as a view** - everything else declared a view. A
  `MemTable` snapshot. A few are also lazy/eager **merge** targets
  (`information_schema.tables`/`columns`/`schemata`) that registration appends user
  rows into, so they reflect user objects - but they are still tables, not views.

The **Served as** column in the view tables below is `view` or `table`.

---

## Base tables (75)

**Population**: **seed-only** (built-in rows; no user-object injection yet) or
**eager + lazy** / **lazy** (also populated at runtime with the live database's
objects, via the paths above). All 75 are served as tables and are queryable
(working - each verified with `SELECT count(*)`).

### Core relation catalog
| Table | Purpose | Population |
|---|---|---|
| `pg_class` | Relations: tables, views, indexes, sequences, composite types. | **eager + lazy** (tables via `register_user_tables`; indexes via `register_user_index`; lazy `relations()`/`indexes()`) |
| `pg_attribute` | Columns of every relation. | **eager + lazy** (`register_user_tables` / lazy `columns()`) |
| `pg_namespace` | Schemas. | **eager + lazy** (`register_schema` / lazy `schemas()`) |
| `pg_type` | Data types incl. each relation's composite rowtype. | **eager + lazy** (rowtype written with the relation) |
| `pg_index` | Index structure: target table, key columns, unique/primary flags. | **eager + lazy** (`register_user_index` / lazy `indexes()`) |
| `pg_attrdef` | Column defaults (node-tree `adbin`). | **eager + lazy** (one row per defaulted column via `register_user_tables` / lazy `columns()`) |
| `pg_constraint` | Check/PK/FK/unique/exclusion constraints. | **eager + lazy** (`register_user_constraint` / lazy `constraints()`) - backs the four real constraint views |
| `pg_inherits` | Table inheritance / partition parent links. | seed-only |
| `pg_partitioned_table` | Partition-key metadata for partitioned tables. | seed-only |
| `pg_sequence` | Sequence parameters (start, increment, ...). | seed-only |

### Functions, operators, access methods
| Table | Purpose | Population |
|---|---|---|
| `pg_proc` | Functions and procedures. | seed-only |
| `pg_aggregate` | Aggregate-function metadata. | seed-only |
| `pg_operator` | Operators. | seed-only |
| `pg_cast` | Cast rules between types. | seed-only |
| `pg_language` | Function languages (sql, c, plpgsql, ...). | seed-only |
| `pg_transform` | Type/language transform functions. | seed-only |
| `pg_am` | Access methods (btree, hash, gin, ...). | seed-only |
| `pg_amop` | Operators belonging to an access-method opclass. | seed-only |
| `pg_amproc` | Support procedures for an access-method opclass. | seed-only |
| `pg_opclass` | Operator classes. | seed-only |
| `pg_opfamily` | Operator families. | seed-only |

### Types
| Table | Purpose | Population |
|---|---|---|
| `pg_enum` | Enum-label values. | seed-only |
| `pg_range` | Range-type definitions. | seed-only |
| `pg_collation` | Collations. | seed-only |
| `pg_conversion` | Encoding conversions. | seed-only |

### Roles, databases, tablespaces
| Table | Purpose | Population |
|---|---|---|
| `pg_authid` | Roles/users (with auth attributes). | seed-only |
| `pg_auth_members` | Role-membership edges. | seed-only |
| `pg_database` | Databases. | **eager + lazy** (`register_user_database` / `register_user_database_with_callback` / lazy `databases()`) |
| `pg_db_role_setting` | Per-database/role GUC overrides. | seed-only |
| `pg_tablespace` | Tablespaces. | seed-only |

### Dependencies, descriptions, security labels, ACLs
| Table | Purpose | Population |
|---|---|---|
| `pg_depend` | Dependencies between objects. | seed-only |
| `pg_shdepend` | Dependencies on shared (cluster-wide) objects. | seed-only |
| `pg_description` | Comments on objects. | seed-only |
| `pg_shdescription` | Comments on shared objects. | seed-only |
| `pg_seclabel` | Security labels. | seed-only |
| `pg_shseclabel` | Security labels on shared objects. | seed-only |
| `pg_init_privs` | Initial privileges (extension-owned objects). | seed-only |
| `pg_default_acl` | Default privileges by object type. | seed-only |
| `pg_parameter_acl` | ACLs on configuration parameters. | seed-only |

### Rules, triggers, policies, events
| Table | Purpose | Population |
|---|---|---|
| `pg_rewrite` | Rewrite rules incl. view bodies (node-tree `ev_action`). | seed-only |
| `pg_trigger` | Triggers. | seed-only |
| `pg_policy` | Row-level-security policies. | seed-only |
| `pg_event_trigger` | Event triggers. | seed-only |

### Statistics
| Table | Purpose | Population |
|---|---|---|
| `pg_statistic` | Per-column planner statistics. | seed-only |
| `pg_statistic_ext` | Extended-statistics objects. | seed-only |
| `pg_statistic_ext_data` | Extended-statistics data. | seed-only |

### Extensions, FDW, replication, text search, large objects
| Table | Purpose | Population |
|---|---|---|
| `pg_extension` | Installed extensions. | seed-only |
| `pg_available_extensions` | Extensions available to install. | seed-only |
| `pg_foreign_data_wrapper` | Foreign-data wrappers. | seed-only |
| `pg_foreign_server` | Foreign servers. | seed-only |
| `pg_foreign_table` | Foreign tables. | seed-only |
| `pg_user_mapping` | User mappings for foreign servers. | seed-only |
| `pg_publication` | Logical-replication publications. | seed-only |
| `pg_publication_namespace` | Schemas in a publication. | seed-only |
| `pg_publication_rel` | Tables in a publication. | seed-only |
| `pg_subscription` | Logical-replication subscriptions. | seed-only |
| `pg_subscription_rel` | Per-table subscription state. | seed-only |
| `pg_replication_origin` | Replication-origin registrations. | seed-only |
| `pg_ts_config` | Text-search configurations. | seed-only |
| `pg_ts_config_map` | Text-search config token->dict maps. | seed-only |
| `pg_ts_dict` | Text-search dictionaries. | seed-only |
| `pg_ts_parser` | Text-search parsers. | seed-only |
| `pg_ts_template` | Text-search templates. | seed-only |
| `pg_largeobject` | Large-object data chunks. | seed-only |
| `pg_largeobject_metadata` | Large-object ownership/ACL. | seed-only |

### Configuration & runtime-info tables
| Table | Purpose | Population |
|---|---|---|
| `pg_config` | Compile/install settings (the `pg_config` CLI values). | **lazy** (`LazyCatalogSource::config()`) |
| `pg_settings` | Runtime GUC parameters. | **lazy** (`LazyCatalogSource::settings()`) |
| `pg_timezone_names` | Known time-zone names. | seed-only |
| `pg_timezone_abbrevs` | Time-zone abbreviations. | seed-only |
| `pg_hba_file_rules` | Parsed `pg_hba.conf` rules. | seed-only |
| `pg_ident_file_mappings` | Parsed `pg_ident.conf` mappings. | seed-only |

### information_schema base tables
| Table | Purpose | Population |
|---|---|---|
| `sql_features` | SQL-standard feature conformance list. | seed-only |
| `sql_implementation_info` | Implementation-defined limits/info. | seed-only |
| `sql_parts` | SQL-standard parts conformance. | seed-only |
| `sql_sizing` | Sizing limits (identifier length, ...). | seed-only |

---

## Views: the 6 actually served as views

These are the only objects declared `type: view` that the server serves as real
views (re-derived from base tables every query). `view_sql exec`/`content` are from
running the definition against the PostgreSQL 17 snapshot.

| View | Schema | Status | Diverging column | Reason |
|---|---|---|---|---|
| `pg_tables` | pg_catalog | working | - | - |
| `pg_views` | pg_catalog | partial | `definition` | `pg_get_viewdef` reads an integration-supplied resolver (`set_view_definition_resolver`); NULL when none is installed. We never deparse node trees. |
| `table_constraints` | information_schema | working | - | - |
| `key_column_usage` | information_schema | working | - | - |
| `constraint_column_usage` | information_schema | working | - | - |
| `referential_constraints` | information_schema | working | - | - |

---

## Views: the 130 declared but served as tables (broken)

These carry `type: view` in the YAML but the server materializes each as a
`MemTable` (or, for one, fails to scan). They answer `SELECT` from frozen
seed/snapshot rows and do **not** re-derive from their base tables, so they are
broken as views. Promoting them to real views means adding them to `VIEW_ONLY_TABLES`
(and ensuring their `view_sql` plans) - tracked in `TODO.md`.

The secondary `view_sql` column below records whether the defining SQL would even
run if promoted: **80 would run** (68 of those reproduce the snapshot exactly, 12
diverge in a named column), **50 error today**. Per-object detail is in
`claude-scripts/catalog_audit.md`; the meaningful groupings:

### Their `view_sql` runs and matches the snapshot (would be working if promoted)
Materialized today but the definition is correct - the cheapest to make real views.
Includes `pg_indexes`, `pg_matviews`, `pg_roles`, `pg_shadow`, `pg_user`,
`pg_user_mappings`, all `pg_stat[io]_{sys,user}_*` snapshot views, and most
`information_schema` views (`routines`, `columns`-adjacent helpers, `domains`,
`sequences`, `triggers`, the `role_*_grants`, the `foreign_*`, `element_types`,
`attributes`, ...). Full list: rows with `view_sql exec = ok`, `content = match` in
`catalog_audit.md`.

### Their `view_sql` runs but a column diverges (would be partial if promoted)
| View | Schema | Diverging | Reason |
|---|---|---|---|
| `pg_rules` | pg_catalog | `definition` | `pg_get_ruledef` not implemented. |
| `applicable_roles` | information_schema | (row count) | role membership (`pg_auth_members`) not fully modeled - one synthetic row. |
| `check_constraints` | information_schema | `check_clause` | raw node-tree; `pg_get_constraintdef` not reproduced. |
| `columns` | information_schema | `is_updatable` | `pg_relation_is_updatable` stub returns 0. |
| `parameters` | information_schema | `parameter_default` | `pg_get_function_arg_default` stub. |
| `tables` | information_schema | `is_insertable_into` | not reproduced for views. |
| `views` | information_schema | `view_definition`, `is_updatable`, `is_insertable_into` | `pg_get_viewdef` not reproduced. |
| `column_privileges`, `routine_privileges`, `table_privileges`, `udt_privileges`, `usage_privileges` | information_schema | (row count -> 0/partial) | GRANTs not modeled, so privilege views are empty. |

### Their `view_sql` errors - fixable engine/UDF gaps (9)
| View | Reads from | Blocker |
|---|---|---|
| `pg_group` | pg_auth_members, pg_authid | `pg_authid.oid` unresolved after subquery flattening (rewrite-pipeline bug). **Good small win.** |
| `pg_available_extension_versions` | pg_available_extensions, pg_extension | Spurious `GROUP BY` wildcard not planned. (Function-backed - even `SELECT *` fails to scan.) |
| `pg_policies` | pg_policy, pg_class, pg_namespace, pg_authid | "Unsupported SQL type name" while planning. |
| `pg_publication_tables` | pg_publication, pg_namespace, pg_attribute | sqlparser parse error on `ARRAY` literal. |
| `pg_seclabels` | pg_seclabel, pg_class, pg_namespace | `pg_table_is_visible()` not implemented. |
| `pg_sequences` | pg_sequence, pg_class, pg_namespace | `pg_sequence_last_value()` not implemented. |
| `pg_stats` | pg_statistic, pg_class, pg_attribute | `row_security_active()` not implemented. |
| `pg_stats_ext` | pg_statistic_ext, pg_class | `s.stxkeys` unresolved (column scoping after rewrite). |
| `pg_stats_ext_exprs` | pg_statistic_ext, pg_statistic_ext_data, pg_class | `pg_get_statisticsobjdef_expressions()` not implemented. |

### Their `view_sql` errors - need live server-runtime functions (41, all pg_catalog)
Report live process/IO/WAL/lock/progress state via server-runtime table functions we
don't host (inherently empty in a static catalog even if promoted). The missing
function per view, from `catalog_audit.md`:

`pg_stat_activity`, `pg_stat_replication`, `pg_stat_gssapi`, `pg_stat_ssl`
(`pg_stat_get_activity`); `pg_stat_all_tables`, `pg_stat_all_indexes`
(`pg_stat_get_numscans`); `pg_statio_all_tables`, `pg_statio_all_indexes`,
`pg_statio_all_sequences` (`pg_stat_get_blocks_fetched`); `pg_stat_xact_all_tables`
(`pg_stat_get_xact_numscans`); `pg_stat_user_functions`
(`pg_stat_get_function_calls`); `pg_stat_xact_user_functions`
(`pg_stat_get_xact_function_calls`); `pg_stat_archiver`, `pg_stat_bgwriter`,
`pg_stat_checkpointer`, `pg_stat_database`, `pg_stat_database_conflicts`,
`pg_stat_io`, `pg_stat_recovery_prefetch`, `pg_stat_replication_slots`,
`pg_stat_slru`, `pg_stat_subscription`, `pg_stat_subscription_stats`, `pg_stat_wal`,
`pg_stat_wal_receiver` (`pg_stat_get_*`); the six `pg_stat_progress_*`
(`pg_stat_get_progress_info`); `pg_locks` (`pg_lock_status`); `pg_cursors`
(`pg_cursor`); `pg_prepared_statements` (`pg_prepared_statement`);
`pg_prepared_xacts` (`pg_prepared_xact`); `pg_file_settings`
(`pg_show_all_file_settings`); `pg_wait_events` (`pg_get_wait_events`);
`pg_backend_memory_contexts` (`pg_get_backend_memory_contexts`);
`pg_shmem_allocations` (`pg_get_shmem_allocations`);
`pg_replication_origin_status` (`pg_show_replication_origin_status`);
`pg_replication_slots` (`pg_get_replication_slots`).

> The `pg_stat[io]_{sys,user}_*` views are in the "would-be-working" group above
> because they select from the **materialized** `pg_stat[io]_all_*` snapshot tables;
> the `*_all_*` parents' SQL is what needs these runtime functions.

---

## Summary

| Scope | Declared | Served as view | working | partial | broken (served as table) |
|---|---|---|---|---|---|
| `pg_catalog` views | 71 | 2 | 1 | 1 | 69 |
| `information_schema` views | 65 | 4 | 4 | 0 | 61 |
| **views total** | **136** | **6** | **5** | **1** | **130** |
| base tables | 75 | n/a (all tables) | 75 | 0 | 0 |

Of the 130 views served as tables: **80** have a `view_sql` that runs (68 reproduce
the snapshot, 12 diverge in a named column) and would become working/partial real
views if registered as views; **50** error today (9 fixable engine/UDF gaps + 41
needing live server-runtime functions). One of the 50, `pg_available_extension_versions`,
is function-backed and fails even a plain scan.

All 75 base tables are queryable; 13 also accept runtime user objects
(`pg_class`, `pg_attribute`, `pg_attrdef`, `pg_namespace`, `pg_type`, `pg_index`,
`pg_constraint`, `pg_database` eager+lazy; `pg_config`, `pg_settings` lazy; and
`information_schema.tables`/`columns`/`schemata` as merge targets), the rest are
seed-only.

---

## How status is determined

This file's per-object verdicts are produced by one script and confirmed by the
regression test; they are not hand-maintained.

1. **Audit (served-as + execution + content in one pass)** -
   `claude-scripts/audit_catalog_objects.py` enumerates every object from the YAML,
   asks the live server how each is served (`EXPLAIN`: real view vs materialized
   table), queries every base table, runs each view's `view_sql`, and compares row
   count and content to the PostgreSQL 17 snapshot (with the same database-name
   canonicalization the regression test uses). It writes
   `claude-scripts/catalog_audit.md` and `catalog_audit.json`. Start the server on
   the embedded catalog (the empty first argument selects the shipped Arrow IPC
   artifact), then run it:
   ```bash
   cargo build --release --bin datafusion_pg_catalog
   RUST_LOG=off ./target/release/datafusion_pg_catalog "" \
       --default-catalog pgtry --default-schema public --host 127.0.0.1 --port 5444 &
   .venv/bin/python -m claude-scripts.audit_catalog_objects \
       --conn "host=127.0.0.1 port=5444 dbname=pgtry user=dbuser password=pencil sslmode=disable"
   ```
2. **Regression test (content gate)** - `tests/test_view_output_snapshot.py` runs each
   view's `view_sql` and fails on any drift versus the snapshot. Its
   `KNOWN_CONTENT_MISMATCHES` / `KNOWN_COUNT_MISMATCHES` / `KNOWN_EXEC_FAILURES`
   baselines are the authoritative list of accepted `view_sql` divergences. Note this
   test checks the **definition** (`view_sql`), not whether the object is served as a
   real view - that distinction is what the audit's served-as check adds.
   ```bash
   RUST_LOG=off .venv/bin/python -m pytest tests/test_view_output_snapshot.py -q
   ```

See [`TODO.md`](TODO.md) for the prioritized work to promote materialized views to
real views and to close the engine/registration backlog.

---

## Appendix: notable view-engine fixes

History of the rewrite passes / UDFs that made specific `view_sql` definitions plan
and match, kept for context when touching the planner pipeline
(`src/logical_plan_rules.rs`, `src/replace*.rs`, `src/user_functions.rs`).

- **`constraint_column_usage`** - its derived table projects two columns named
  `nspname`, tripping a DataFusion optimizer assertion. `disambiguate_duplicate_columns`
  aliases duplicate names in *nested* projections (`nspname`, `nspname_2`); the top
  level is handled by `alias_unnamed_columns`.
- **`user_mapping_options`** - `LATERAL pg_options_to_table(um.umoptions) opts(...)`
  in `FROM` rewritten to the projection form `(pg_options_to_table(...)).option_name`,
  which the SRF->unnest pass handles (semantically identical in PostgreSQL).
- **`element_types`** - its 4-column `(...) IN (SELECT ... FROM data_type_privileges)`
  row-constructor subquery (unplannable) rewritten to a correlated `EXISTS`
  (`rewrite_tuple_in_subquery_to_exists`), which DataFusion 54 decorrelates.
- **`table_constraints`** - (1) a wire-layer bug emitted one result set per Arrow
  `RecordBatch`, truncating every `UNION` view / >8192-row result; now all batches are
  concatenated (`server.rs`). (2) Its `nulls_distinct` correlated scalar subquery
  (undecorrelatable) replaced with the constant `'YES'`. The wire fix silently
  corrected *every* truncated `UNION` view.
- **`parameters` / `element_types`** - `proargtypes` (`oidvector`) / `proallargtypes`
  (`_oid`) load as `List<Int64>`, so `COALESCE(proallargtypes, proargtypes::oid[])`
  type-matches and the `::oid[]` cast drops (`drop_oid_array_cast`).
- **`key_column_usage`, `check_constraints`** - the dot->subscript pass wrongly turned
  `tbl.arraycol[i]` into `tbl['arraycol'][i]`; now only parenthesized `(expr).field`
  roots convert. `check_constraints` also needed `format()` (`register_format`).
- **`columns`, `attributes`** - `_pg_truetypid(a.*, t.*)` / `_pg_truetypmod(a.*, t.*)`
  whole-row composite args expanded to scalar columns
  (`rewrite_pg_truetypid_composite_args`); `pg_column_is_updatable` stub added; a
  spurious `GROUP BY` fabricated from `= ANY(ARRAY[...])` predicates fixed.
- **`tables`, `views`** - non-literal `::regclass` / `::oid` casts dropped
  (`rewrite_remaining_oid_regclass_casts`); EXISTS/IN subquery tables qualified.
