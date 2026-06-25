# pg_catalog reference - tables, views, registration & status

Single source of truth for **what lives in the catalog**, **what each object is
for**, **how it is populated** (seed vs. runtime registration, and what it derives
from), and **whether it works today**. Verified 2026-06-25.

- **75 base tables** (71 `pg_catalog` + 4 `information_schema`).
- **136 views** (71 `pg_catalog` + 65 `information_schema`).

Status comes from two checks (see [How status is determined](#how-status-is-determined)):
- **execution** - does the view's `view_sql` plan and run? (`analyze_catalog_views.py` -> `catalog_views_report.md`)
- **content** - does the output match a real PostgreSQL 17 snapshot? (`tests/test_view_output_snapshot.py`)

### Status legend
| | Meaning |
|---|---|
| working | Executes and reproduces PostgreSQL output. |
| partial | Executes, but a named column diverges, or the row set is empty/extra **by design**. |
| broken | The view's defining SQL does not execute; the blocker is named. (It is still queryable as a static snapshot - see below.) |

---

## How objects are populated

There are two layers, and for the dynamic tables, two interchangeable runtime paths.

### 1. Seed (built-in rows)
Every base table is loaded at startup from `pg_catalog_data/pg_schema/*.yaml`,
dumped from a real PostgreSQL 17.4. For the immutable system catalogs (`pg_type`,
`pg_proc`, `pg_operator`, `pg_am`, ...) this seed **is** the data. Views are also
seeded with materialized `rows:` used as test fixtures.

### 2. Runtime registration (a live database's own objects)
When something fronts a real database (e.g. `riffq`), the user's tables/columns/
indexes are injected into the catalog at runtime. Two paths write the **same**
catalog tables; pick per embedding:

| Path | Entry points | How it works |
|---|---|---|
| **Eager / pre-register** | `register_user_database`, `register_schema`, `register_user_tables`, `register_user_index` | Imperatively writes rows now (INSERT / batch-append). Use when objects are known up front. |
| **Lazy / callback** | `register_lazy_catalog(source, opts)` + `register_user_database_with_callback` | The embedder implements `LazyCatalogSource`; on **every scan** the catalog calls back (`databases()`/`schemas()`/`relations()`/`columns()`/`indexes()`/`config()`/`settings()`), builds rows, and merges them over the seed. Use when the live set changes. |

Both paths share the same pure row-builders (`build_pg_class_row`,
`build_pg_index_row`, ...) so the two stay in sync.

### How that reaches the **views**
Views are not registered directly - they derive from base tables. Three behaviours:

- **live** - re-derived from its base tables on every query, so it **reflects
  runtime-registered user objects immediately**. `VIEW_ONLY_TABLES` in
  `src/session.rs`: `pg_tables`, `pg_views`, and the four constraint views
  (`table_constraints`, `key_column_usage`, `constraint_column_usage`,
  `referential_constraints`, derived from the registrable `pg_constraint`).
- **merge** - the lazy/eager registration target itself: `information_schema.tables`,
  `information_schema.columns`, `information_schema.schemata` are wrapped by the
  lazy provider (and written by `register_user_tables`), so they **reflect user
  objects** too.
- **static** - every other view is served as a materialized **snapshot**
  (`MemTable` from its YAML `rows:`). It is queryable but holds **seed/fixture data
  only**; it does **not** reflect runtime-registered user objects. (So today a
  registered index lands in `pg_index`/`pg_class` but `pg_indexes` - a static view -
  does not yet show it; promoting it is tracked in `TODO.md`.)

The **Reflects** column in the view tables below is one of `live` / `merge` / `static`.

---

## Base tables (75)

**Population**: **seed-only** (built-in rows; no user-object injection yet) or
**eager + lazy** (also populated at runtime with the live database's objects, via
both paths above). All base tables are queryable (working).

### Core relation catalog
| Table | Purpose | Population |
|---|---|---|
| `pg_class` | Relations: tables, views, indexes, sequences, composite types. | **eager + lazy** (tables via `register_user_tables`; indexes via `register_user_index`; lazy `relations()`/`indexes()`) |
| `pg_attribute` | Columns of every relation. | **eager + lazy** (`register_user_tables` / lazy `columns()`) |
| `pg_namespace` | Schemas. | **eager + lazy** (`register_schema` / lazy `schemas()`) |
| `pg_type` | Data types incl. each relation's composite rowtype. | **eager + lazy** (rowtype written with the relation) |
| `pg_index` | Index structure: target table, key columns, unique/primary flags. | **eager + lazy** (`register_user_index` / lazy `indexes()`) - *added this session* |
| `pg_attrdef` | Column defaults (node-tree `adbin`). | seed-only |
| `pg_constraint` | Check/PK/FK/unique/exclusion constraints. | seed-only |
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

## pg_catalog views (71)

`Reflects` = does it show runtime-registered user objects (`live`/`merge`/`static`,
see above). `Reads from` = the main catalog tables the view's SQL joins.

### Working (19)
| View | Purpose | Reads from | Reflects |
|---|---|---|---|
| `pg_tables` | All tables, one row per `relkind` r/p. | pg_class, pg_namespace, pg_tablespace | **live** |
| `pg_indexes` | All indexes + their `CREATE INDEX` text (`pg_get_indexdef`; functional/partial expression text is Phase 3). | pg_index, pg_class, pg_namespace, pg_tablespace | static |
| `pg_matviews` | Materialized views and their owners. | pg_class, pg_namespace | static |
| `pg_roles` | Roles (password redacted). | pg_authid, pg_db_role_setting | static |
| `pg_shadow` | Roles with password hashes (superuser view). | pg_authid | static |
| `pg_user` | Login-capable users. | pg_shadow | static |
| `pg_user_mappings` | FDW user mappings (ACL-aware). | pg_user_mapping, pg_authid, pg_foreign_server | static |
| `pg_stat_sys_tables` | Access stats for system tables. | pg_stat_all_tables | static |
| `pg_stat_user_tables` | Access stats for user tables. | pg_stat_all_tables | static |
| `pg_stat_sys_indexes` | Access stats for system indexes. | pg_stat_all_indexes | static |
| `pg_stat_user_indexes` | Access stats for user indexes. | pg_stat_all_indexes | static |
| `pg_stat_xact_sys_tables` | Per-transaction access stats, system tables. | pg_stat_xact_all_tables | static |
| `pg_stat_xact_user_tables` | Per-transaction access stats, user tables. | pg_stat_xact_all_tables | static |
| `pg_statio_sys_tables` | Block I/O per system table. | pg_statio_all_tables | static |
| `pg_statio_user_tables` | Block I/O per user table. | pg_statio_all_tables | static |
| `pg_statio_sys_indexes` | Block I/O per system index. | pg_statio_all_indexes | static |
| `pg_statio_user_indexes` | Block I/O per user index. | pg_statio_all_indexes | static |
| `pg_statio_sys_sequences` | Block I/O per system sequence. | pg_statio_all_sequences | static |
| `pg_statio_user_sequences` | Block I/O per user sequence. | pg_statio_all_sequences | static |

> The `pg_stat[io]_{sys,user}_*` views above work because they select from the
> **materialized** `pg_stat[io]_all_*` snapshot tables; the `*_all_*` parents'
> *live* SQL is what's broken (needs runtime stat functions - below).

### Partial - executes but a column diverges (2)
| View | Purpose | Reads from | Reflects | Diverging | Reason |
|---|---|---|---|---|---|
| `pg_views` | All views + their defining SQL. | pg_class, pg_namespace | **live** | `definition` | `pg_get_viewdef` not implemented (node-tree deparse). |
| `pg_rules` | Rewrite rules + their defining SQL. | pg_rewrite, pg_class, pg_namespace | static | `definition` | `pg_get_ruledef` not implemented. |

### Broken - fixable engine/UDF gaps (9)
| View | Purpose | Reads from | Blocker |
|---|---|---|---|
| `pg_group` | Legacy role groups + members. | pg_auth_members, pg_authid | `pg_authid.oid` unresolved after subquery flattening (rewrite-pipeline bug). **Good small win.** |
| `pg_available_extension_versions` | Installable extension versions. | pg_available_extension_versions, pg_extension | Spurious `GROUP BY` wildcard not planned. |
| `pg_policies` | Row-level-security policies, readable form. | pg_policy, pg_class, pg_namespace, pg_authid | "Unsupported SQL type name" while planning. |
| `pg_publication_tables` | Tables in each publication. | pg_publication, pg_namespace, pg_attribute | sqlparser parse error on `ARRAY` literal. |
| `pg_seclabels` | Security labels on objects. | pg_seclabel, pg_class, pg_namespace | `pg_table_is_visible()` not implemented. |
| `pg_sequences` | Sequences + parameters/last value. | pg_sequence, pg_class, pg_namespace | `pg_sequence_last_value()` not implemented. |
| `pg_stats` | Per-column planner statistics, readable. | pg_statistic, pg_class, pg_attribute | `row_security_active()` not implemented. |
| `pg_stats_ext` | Extended (multi-column) statistics. | pg_statistic_ext, pg_class | `s.stxkeys` unresolved (column scoping after rewrite). |
| `pg_stats_ext_exprs` | Per-expression extended statistics. | pg_statistic_ext, pg_statistic_ext_data, pg_class | `pg_get_statisticsobjdef_expressions()` not implemented. |

### Broken - need live server-runtime functions (41)
These report live process/IO/WAL/lock/progress state via server-runtime **table
functions** we don't have. In a static catalog they'd be empty/zero stubs at best.
All `static`.

| View | Purpose | Missing runtime function |
|---|---|---|
| `pg_stat_activity` | Current sessions/queries. | `pg_stat_get_activity` |
| `pg_stat_replication` | Live replication connections (sender). | `pg_stat_get_activity` |
| `pg_stat_gssapi` | Per-connection GSSAPI info. | `pg_stat_get_activity` |
| `pg_stat_ssl` | Per-connection SSL info. | `pg_stat_get_activity` |
| `pg_stat_all_tables` | Access stats, all tables. | `pg_stat_get_numscans` |
| `pg_stat_all_indexes` | Access stats, all indexes. | `pg_stat_get_numscans` |
| `pg_statio_all_tables` | Block I/O, all tables. | `pg_stat_get_blocks_fetched` |
| `pg_statio_all_indexes` | Block I/O, all indexes. | `pg_stat_get_blocks_fetched` |
| `pg_statio_all_sequences` | Block I/O, all sequences. | `pg_stat_get_blocks_fetched` |
| `pg_stat_xact_all_tables` | Per-transaction access stats, all tables. | `pg_stat_get_xact_numscans` |
| `pg_stat_user_functions` | Function call counts/timing. | `pg_stat_get_function_calls` |
| `pg_stat_xact_user_functions` | Per-transaction function stats. | `pg_stat_get_xact_function_calls` |
| `pg_stat_archiver` | WAL archiver activity. | `pg_stat_get_archiver` |
| `pg_stat_bgwriter` | Background-writer activity. | `pg_stat_get_bgwriter_*` |
| `pg_stat_checkpointer` | Checkpointer activity. | `pg_stat_get_checkpointer_*` |
| `pg_stat_database` | Per-database activity/stats. | `pg_stat_get_db_numbackends` |
| `pg_stat_database_conflicts` | Recovery-conflict counts per DB. | `pg_stat_get_db_conflict_*` |
| `pg_stat_io` | I/O stats by backend type/context. | `pg_stat_get_io` |
| `pg_stat_recovery_prefetch` | WAL prefetch during recovery. | `pg_stat_get_recovery_prefetch` |
| `pg_stat_replication_slots` | Per-slot replication stats. | `pg_stat_get_replication_slot` |
| `pg_stat_slru` | SLRU cache stats. | `pg_stat_get_slru` |
| `pg_stat_subscription` | Subscription worker status. | `pg_stat_get_subscription` |
| `pg_stat_subscription_stats` | Subscription error counts. | `pg_stat_get_subscription_stats` |
| `pg_stat_wal` | WAL generation activity. | `pg_stat_get_wal` |
| `pg_stat_wal_receiver` | WAL receiver status. | `pg_stat_get_wal_receiver` |
| `pg_stat_progress_analyze` | Running `ANALYZE` progress. | `pg_stat_get_progress_info` |
| `pg_stat_progress_basebackup` | Running base-backup progress. | `pg_stat_get_progress_info` |
| `pg_stat_progress_cluster` | Running `CLUSTER`/`VACUUM FULL` progress. | `pg_stat_get_progress_info` |
| `pg_stat_progress_copy` | Running `COPY` progress. | `pg_stat_get_progress_info` |
| `pg_stat_progress_create_index` | Running `CREATE INDEX` progress. | `pg_stat_get_progress_info` |
| `pg_stat_progress_vacuum` | Running `VACUUM` progress. | `pg_stat_get_progress_info` |
| `pg_locks` | Currently held/awaited locks. | `pg_lock_status` |
| `pg_cursors` | Open cursors in the session. | `pg_cursor` |
| `pg_prepared_statements` | Prepared statements in the session. | `pg_prepared_statement` |
| `pg_prepared_xacts` | Prepared (two-phase) transactions. | `pg_prepared_xact` |
| `pg_file_settings` | Settings as parsed from config files. | `pg_show_all_file_settings` |
| `pg_wait_events` | Catalog of wait events. | `pg_get_wait_events` |
| `pg_backend_memory_contexts` | Backend memory-context tree. | `pg_get_backend_memory_contexts` |
| `pg_shmem_allocations` | Shared-memory allocations. | `pg_get_shmem_allocations` |
| `pg_replication_origin_status` | Replication-origin replay progress. | `pg_show_replication_origin_status` |
| `pg_replication_slots` | Replication slots. | `pg_get_replication_slots` |

---

## information_schema views (65)

The SQL-standard introspection layer; each is a thin, portable projection over
`pg_catalog`. **All 65 execute** (54 working, 11 partial). `tables`/`columns`/
`schemata` are lazy/eager registration targets (`merge` - reflect user objects);
the four constraint views (`table_constraints`, `key_column_usage`,
`constraint_column_usage`, `referential_constraints`) are `live` over
`pg_constraint`; the rest are `static` snapshots.

| View | Purpose | Reads from | Reflects | Status / reason |
|---|---|---|---|---|
| `_pg_foreign_data_wrappers` | Internal helper: FDWs visible to the user. | pg_foreign_data_wrapper | static | working |
| `_pg_foreign_servers` | Internal helper: foreign servers. | pg_foreign_server | static | working |
| `_pg_foreign_table_columns` | Internal helper: foreign-table columns. | pg_foreign_table | static | working |
| `_pg_foreign_tables` | Internal helper: foreign tables. | pg_foreign_table | static | working |
| `_pg_user_mappings` | Internal helper: user mappings. | pg_user_mapping, pg_authid | static | working |
| `administrable_role_authorizations` | Roles the current user can administer. | applicable_roles | static | working |
| `applicable_roles` | Roles whose privileges the user inherits. | pg_auth_members, pg_authid | static | partial - off-by-one: synthetic `pg_database_owner` membership row. |
| `attributes` | Attributes (fields) of composite types. | pg_attribute, pg_type, pg_namespace | static | working |
| `character_sets` | Available character sets. | pg_database, pg_namespace | static | working |
| `check_constraint_routine_usage` | Routines used in check constraints. | pg_constraint, pg_proc | static | working |
| `check_constraints` | Check constraints + their clause. | pg_constraint, pg_class | static | partial - `check_clause`: raw node-tree (pg_get_constraintdef not reproduced). |
| `collation_character_set_applicability` | Which charsets a collation applies to. | pg_collation, pg_database | static | working |
| `collations` | Available collations. | pg_collation, pg_database | static | working |
| `column_column_usage` | Generated-column -> source-column deps. | pg_attribute, pg_attrdef | static | working |
| `column_domain_usage` | Columns that use a domain. | pg_type, pg_attribute | static | working |
| `column_options` | Per-column FDW options. | (foreign-column helper) | static | working |
| `column_privileges` | Column-level privileges. | pg_class, pg_attribute, pg_authid | static | partial - empty: GRANTs not modeled. |
| `column_udt_usage` | Columns and their underlying type. | pg_attribute, pg_namespace | static | working |
| `columns` | All table/view columns. | pg_attribute, pg_namespace, pg_attrdef | **merge** | partial - is_updatable NULL on 4 columns (pg_relation_is_updatable stub). |
| `constraint_column_usage` | Columns referenced by constraints. | pg_constraint, pg_attribute | live | working |
| `constraint_table_usage` | Tables referenced by constraints. | pg_constraint, pg_class | static | working |
| `data_type_privileges` | Type-usage privilege scopes. | attributes, columns, domains, parameters | static | working |
| `domain_constraints` | Constraints attached to domains. | pg_constraint, pg_type | static | working |
| `domain_udt_usage` | Domains and their base type. | pg_type, pg_namespace | static | working |
| `domains` | Domain types + facets. | pg_type, pg_namespace | static | working |
| `element_types` | Element type of array columns/params. | pg_class, pg_type, pg_proc, pg_attribute | static | working |
| `enabled_roles` | Roles enabled in the current session. | pg_authid | static | working |
| `foreign_data_wrapper_options` | FDW option key/value pairs. | pg_foreign_data_wrapper | static | working |
| `foreign_data_wrappers` | Foreign-data wrappers. | pg_foreign_data_wrapper | static | working |
| `foreign_server_options` | Foreign-server option pairs. | pg_foreign_server | static | working |
| `foreign_servers` | Foreign servers. | pg_foreign_server | static | working |
| `foreign_table_options` | Foreign-table option pairs. | pg_foreign_table | static | working |
| `foreign_tables` | Foreign tables. | pg_foreign_table | static | working |
| `information_schema_catalog_name` | Name of the current database. | (constant) | static | working |
| `key_column_usage` | Columns in PK/UNIQUE/FK constraints. | pg_constraint, pg_attribute | live | working |
| `parameters` | Function/procedure parameters. | pg_proc, pg_type, pg_namespace | static | partial - `parameter_default` NULL (`pg_get_function_arg_default` stub). |
| `referential_constraints` | Foreign-key constraint metadata. | pg_constraint, pg_class, pg_depend | live | working |
| `role_column_grants` | Column grants to enabled roles. | column_privileges, enabled_roles | static | working |
| `role_routine_grants` | Routine (EXECUTE) grants to enabled roles. | routine_privileges, enabled_roles | static | working |
| `role_table_grants` | Table grants to enabled roles. | table_privileges, enabled_roles | static | working |
| `role_udt_grants` | Type grants to enabled roles. | udt_privileges, enabled_roles | static | working |
| `role_usage_grants` | USAGE grants to enabled roles. | usage_privileges, enabled_roles | static | working |
| `routine_column_usage` | Columns used by a routine. | pg_proc, pg_depend | static | working |
| `routine_privileges` | EXECUTE privileges on routines. | pg_proc, pg_authid | static | partial - empty: GRANTs not modeled. |
| `routine_routine_usage` | Routines called by a routine. | pg_proc, pg_depend | static | working |
| `routine_sequence_usage` | Sequences used by a routine. | pg_proc, pg_depend | static | working |
| `routine_table_usage` | Tables used by a routine. | pg_proc, pg_depend | static | working |
| `routines` | Functions and procedures. | pg_proc, pg_namespace, pg_language | static | working |
| `schemata` | Schemas in the database. | pg_namespace | **merge** | working |
| `sequences` | Sequences + type/limits. | pg_sequence, pg_class, pg_namespace | static | working |
| `table_constraints` | All table constraints. | pg_constraint, pg_class, pg_namespace | live | working |
| `table_privileges` | Table-level privileges. | pg_class, pg_authid | static | partial - empty: GRANTs not modeled. |
| `tables` | All tables and views. | pg_class, pg_namespace | **merge** | partial - is_insertable_into not reproduced for a couple of views. |
| `transforms` | Type/language transform functions. | pg_type, pg_transform, pg_language, pg_proc | static | working |
| `triggered_update_columns` | Columns an `UPDATE OF` trigger watches. | pg_trigger, pg_attribute | static | working |
| `triggers` | Triggers. | pg_trigger, pg_class, pg_namespace | static | working |
| `udt_privileges` | User-defined-type (UDT) privileges. | pg_type, pg_authid | static | partial - empty: GRANTs not modeled. |
| `usage_privileges` | USAGE privileges (domains, charsets, ...). | pg_authid, pg_type, pg_foreign_data_wrapper | static | partial - GRANTs not modeled. |
| `user_defined_types` | Composite/base user-defined types. | pg_type, pg_namespace | static | working |
| `user_mapping_options` | User-mapping option pairs. | pg_user_mapping, pg_authid | static | working |
| `user_mappings` | User mappings (option-free). | pg_user_mapping | static | working |
| `view_column_usage` | Columns a view reads. | pg_rewrite, pg_depend | static | working |
| `view_routine_usage` | Routines a view calls. | pg_depend, pg_proc | static | working |
| `view_table_usage` | Tables a view reads. | pg_rewrite, pg_depend | static | working |
| `views` | All views + their definition. | pg_class, pg_namespace, pg_rewrite | static | partial - `view_definition`/`is_updatable`: `pg_get_viewdef` not reproduced. |

---

## Summary

| | Count | working | partial | broken |
|---|---|---|---|---|
| `pg_catalog` views | 71 | 18 | 3 | 50 |
| `information_schema` views | 65 | 54 | 11 | 0 |
| **views total** | **136** | **72** | **14** | **50** |

The 50 broken views are all `pg_catalog`: **9 fixable** engine/UDF gaps and **41**
needing live server-runtime table functions (inherently empty in a static catalog).

---

## How status is determined

1. **Execution** - start a server and run `analyze_catalog_views.py`, which executes
   every view's `view_sql` and buckets failures by root cause, writing a per-view
   report (`catalog_views_report.md`, a regenerable artifact - not committed).
   ```bash
   cargo build --release --bin datafusion_pg_catalog
   .venv/bin/python -c "import shutil; shutil.make_archive('/tmp/s','zip','pg_catalog_data/pg_schema')"
   RUST_LOG=off ./target/release/datafusion_pg_catalog /tmp/s.zip \
       --default-catalog pgtry --default-schema public --host 127.0.0.1 --port 5444 &
   .venv/bin/python analyze_catalog_views.py        # writes catalog_views_report.md
   ```
2. **Content** - `tests/test_view_output_snapshot.py` compares each view's rows
   against the PostgreSQL 17 snapshot (row count + order-independent content). Its
   `KNOWN_CONTENT_MISMATCHES` / `KNOWN_COUNT_MISMATCHES` / `KNOWN_EXEC_FAILURES`
   baselines are the authoritative list of accepted divergences; the partial/broken reasons
   above are drawn from them. A new divergence - or a baselined item that starts
   matching - fails the test.
   ```bash
   RUST_LOG=off .venv/bin/python -m pytest tests/test_view_output_snapshot.py -q
   ```

See [`TODO.md`](TODO.md) for the prioritized work to turn partial/broken into working and the
engine/registration backlog.

---

## Appendix: notable view-engine fixes

History of the rewrite passes / UDFs that made specific views work, kept for context
when touching the planner pipeline (`src/logical_plan_rules.rs`, `src/replace*.rs`,
`src/user_functions.rs`).

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
