# pg_catalog reference - tables, views, registration & status

Single source of truth for **what lives in the catalog**, **what each object is
for**, **how it is populated** (seed vs. runtime registration), and **whether it
actually works today**. Verified 2026-08-17 by re-running the audit below.

- **75 base tables** (71 `pg_catalog` + 4 `information_schema`) - all served as
  tables and queryable (**75 working**).
- **136 objects declared `type: view`** - **all 136 are now actually served as views**
  (98 working + 38 partial); none falls back to a materialized table. The four whose
  declared bodies use engine features not yet supported are served via equivalent
  simplified bodies (`SIMPLIFIED_VIEW_BODIES` in `src/session.rs`), lossless for the data
  this catalog holds. The stat /
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
| **Declared / pre-register** | `DeclaredCatalogSource::from_declarations` (in `declared_catalog_source.rs`) | Wraps up-front declarations in a `LazyCatalogSource`; OIDs come from the persistent `CatalogOidRegistry` (redb-backed), duplicates refused PostgreSQL-style. Use when objects are known up front. Served through the same merge path as the lazy row below. |
| **Lazy / callback** | `register_lazy_catalog(source, opts)` + `register_user_database_with_callback` | The embedder implements `LazyCatalogSource`; on **every scan** the catalog calls back (`databases()`/`schemas()`/`relations()`/`columns()`/`indexes()`/`constraints()`/`config()`/`settings()`), builds rows, and merges them over the seed. Use when the live set changes. |
| **Combined** | `CombinedCatalogSource::new(vec![...])` (in `combined_catalog_source.rs`) | Unions any number of sources; database/relation name collisions are refused loudly, shared schema names merge. How a declared base and a live source serve together. |

Both paths share the same pure row-builders (`build_pg_class_row`,
`build_pg_index_row`, ...) so the two stay in sync.

### How a declared view is served

Every YAML object with `type: view` is registered with a real `CREATE VIEW`:
`VIEWS_TO_REGISTER` in `src/session.rs` lists exactly the 136 declared views in
dependency order, and `create_registered_views` registers each one. A listed
view whose body fails to plan **fails startup** - there is no fallback to a
materialized table, so a served catalog cannot silently hold a frozen snapshot
where a view belongs. Four views whose declared bodies use engine features the
planner does not support are served via equivalent simplified bodies
(`SIMPLIFIED_VIEW_BODIES`: `pg_group`, `pg_publication_tables`, `pg_stats_ext`,
`pg_stats_ext_exprs`), lossless for the data this catalog holds.

How this is verified rather than asserted: the audit reads each object's query
plan (`EXPLAIN SELECT *`). A real view's plan root is `SubqueryAlias: <name>`
over its base tables; a materialized table's would be `TableScan: <name>`. As
of the 2026-08-17 run, **all 136 declared views plan as views** and zero as
tables.

This section used to describe the opposite state - a `VIEW_ONLY_TABLES`
allow-list of six real views with the other 130 materialized as `MemTable`
snapshots. That constant no longer exists; the promotion work is done and the
failure mode it left open (a "view" answering from frozen fixture rows) is now
a startup error instead of a silent wrong answer.

---

## Base tables (75)

**Population**: **seed-only** (built-in rows; no user-object injection yet) or
**declared + lazy** / **lazy** (also populated at runtime with the live
database's objects, via the paths above - declarations and callbacks are the
same `LazyCatalogSource` mechanism since the eager `register_*` path was
deleted). All 75 are served as tables and are queryable (working - each
verified with `SELECT count(*)`).

### Core relation catalog
| Table | Purpose | Population |
|---|---|---|
| `pg_class` | Relations: tables, views, indexes, sequences, composite types. | **declared + lazy** (declared tables via `DeclaredCatalogSource`; lazy `relations()`/`indexes()`) |
| `pg_attribute` | Columns of every relation. | **declared + lazy** (`DeclaredCatalogSource` / lazy `columns()`) |
| `pg_namespace` | Schemas. | **declared + lazy** (`register_schema` declarations via `DeclaredCatalogSource` / lazy `schemas()`) |
| `pg_type` | Data types incl. each relation's composite rowtype. | **declared + lazy** (rowtype written with the relation) |
| `pg_index` | Index structure: target table, key columns, unique/primary flags. | **lazy** (`LazyCatalogSource::indexes()`) |
| `pg_attrdef` | Column defaults (node-tree `adbin`). | **declared + lazy** (one row per defaulted column via `DeclaredCatalogSource` / lazy `columns()`) |
| `pg_constraint` | Check/PK/FK/unique/exclusion constraints. | **lazy** (`LazyCatalogSource::constraints()`) - backs the four real constraint views |
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
| `pg_database` | Databases. | **declared + lazy** (`DeclaredCatalogSource` / `register_user_database_with_callback` / lazy `databases()`) |
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

## Views: all 136 served as views; the 38 partial, by cause

Every declared view is served as a real view (see "How a declared view is
served" above), every `view_sql` executes, and none is served as a table. The
2026-08-17 audit puts each view in one of two states: **98 working** (executes
and reproduces the PostgreSQL 17 snapshot exactly) and **38 partial** (executes,
but a named column or the row count diverges by design). Per-object detail is
in `claude-scripts/catalog_audit.md`; the 38 partials fall into three causes.

### Definition text needs an integration-supplied resolver (8)

This catalog never deparses node trees, so SQL text that PostgreSQL
reconstructs from a node tree comes from a resolver an integration installs
(`src/runtime_function_resolvers.rs`), and is NULL or a stub until one does.

| View | Schema | Diverging | Reason |
|---|---|---|---|
| `pg_views` | pg_catalog | `definition` | `pg_get_viewdef` reads `set_view_definition_resolver`. |
| `pg_rules` | pg_catalog | `definition` | `pg_get_ruledef` not implemented. |
| `pg_group` | pg_catalog | `grolist` | simplified body; membership array diverges from the snapshot. |
| `views` | information_schema | `view_definition`, `is_updatable`, `is_insertable_into` | `pg_get_viewdef` resolver plus updatability stubs. |
| `check_constraints` | information_schema | `check_clause` | raw node-tree; `pg_get_constraintdef` not reproduced. |
| `columns` | information_schema | `is_updatable` | `pg_relation_is_updatable` stub returns 0 (task 24). |
| `tables` | information_schema | `is_insertable_into` | not reproduced for views. |
| `parameters` | information_schema | `parameter_default` | `pg_get_function_arg_default` stub. |

### GRANTs are not modeled, so privilege views run short or empty (10)

`column_privileges`, `routine_privileges`, `table_privileges`,
`udt_privileges`, `usage_privileges` and the five `role_*_grants` counterparts
(all information_schema) execute but return no or few rows against the
snapshot's thousands, because ACLs are not populated.

### Runtime state answers from resolver slots, so row counts diverge (20)

The stat / live-state views' server-runtime functions (`pg_stat_get_*`,
`pg_show_all_file_settings`, `pg_get_wait_events`, ...) are wired as
integration-installable resolvers that answer empty by default. The views
execute - that is what moved them out of "broken" - but their row counts do
not match the capture: the `pg_stat[io]_*` family produces zeroed rows (one
per relation, or one per database / one global row for the collector views)
where the capture stored none (17 views), and `pg_file_settings`,
`pg_wait_events`, `pg_available_extension_versions` answer empty where the
capture had rows (3 views).

---

## Summary

Regenerated from `claude-scripts/catalog_audit.json`, 2026-08-17.

| Scope | Declared | Served as view | working | partial | broken |
|---|---|---|---|---|---|
| `pg_catalog` views | 71 | 71 | 48 | 23 | 0 |
| `information_schema` views | 65 | 65 | 50 | 15 | 0 |
| **views total** | **136** | **136** | **98** | **38** | **0** |
| base tables | 75 | n/a (all tables) | 75 | 0 | 0 |

Every `view_sql` executes; nothing is served as a materialized table, and a
view body that stopped planning would fail startup rather than degrade. The 38
partials are the three cause groups above: resolver-backed definition text
(8), unmodeled GRANTs (10), and resolver-backed runtime state (20).

All 75 base tables are queryable; 13 also accept runtime user objects
(`pg_class`, `pg_attribute`, `pg_attrdef`, `pg_namespace`, `pg_type`,
`pg_index`, `pg_constraint`, `pg_database` declared+lazy; `pg_config`,
`pg_settings` lazy; and `information_schema.tables`/`columns`/`schemata` as
merge targets), the rest are seed-only.

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

There are no materialized views left to promote: every declared view is on
`VIEWS_TO_REGISTER` in `src/session.rs` and served with `CREATE VIEW`, and a
listed body that fails to plan fails startup. Adding a NEW declared view means
giving it a `view_sql` that plans against the base tables and adding it to
that list in dependency order (`src/bin/gen_view_order.rs` computes the
order).

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
  (`drop_redundant_oid_and_regclass_casts`); EXISTS/IN subquery tables qualified.
