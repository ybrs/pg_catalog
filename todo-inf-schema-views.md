# information_schema views — status

Generated from `analyze_catalog_views.py` against the pg_catalog engine on DataFusion 54.

**58 working, 7 to do, 65 total.**

"Working" = the view SQL plans & executes on our engine (can be promoted to a live view). Today these are still served as materialized-snapshot MemTables.

## To do
| View | Status | What's needed |
|---|---|---|
| `element_types` | other_error | `Unsupported SQL type oid` — needs an `oid`-typed column/cast path the planner accepts |
| `parameters` | other_error | `Unsupported SQL type oid` — same `oid`-typed column/cast path as `element_types` |
| `key_column_usage` | missing_column | SRF rewrite: residual `ss` bare-alias ref in key_column_usage |
| `check_constraints` | missing_column | column scoping / correlated ref (residual `con` alias) |
| `table_constraints` | other_error | correlated scalar subquery must be aggregated to ≤1 row |
| `constraint_column_usage` | other_error | upstream DataFusion projection-name assertion (`nspname` vs `nspname_1`) |
| `user_mapping_options` | missing_table | `pg_options_to_table` SRF not resolved in this view's shape |

## Working
| View |
|---|
| `_pg_foreign_data_wrappers` |
| `_pg_foreign_servers` |
| `_pg_foreign_table_columns` |
| `_pg_foreign_tables` |
| `_pg_user_mappings` |
| `administrable_role_authorizations` |
| `applicable_roles` |
| `attributes` |
| `character_sets` |
| `check_constraint_routine_usage` |
| `collation_character_set_applicability` |
| `collations` |
| `column_column_usage` |
| `column_domain_usage` |
| `column_options` |
| `column_privileges` |
| `column_udt_usage` |
| `columns` |
| `constraint_table_usage` |
| `data_type_privileges` |
| `domain_constraints` |
| `domain_udt_usage` |
| `domains` |
| `enabled_roles` |
| `foreign_data_wrapper_options` |
| `foreign_data_wrappers` |
| `foreign_server_options` |
| `foreign_servers` |
| `foreign_table_options` |
| `foreign_tables` |
| `information_schema_catalog_name` |
| `referential_constraints` |
| `role_column_grants` |
| `role_routine_grants` |
| `role_table_grants` |
| `role_udt_grants` |
| `role_usage_grants` |
| `routine_column_usage` |
| `routine_privileges` |
| `routine_routine_usage` |
| `routine_sequence_usage` |
| `routine_table_usage` |
| `routines` |
| `schemata` |
| `sequences` |
| `table_privileges` |
| `tables` |
| `transforms` |
| `triggered_update_columns` |
| `triggers` |
| `udt_privileges` |
| `usage_privileges` |
| `user_defined_types` |
| `user_mappings` |
| `view_column_usage` |
| `view_routine_usage` |
| `view_table_usage` |
| `views` |

## Grouped by what's needed

- **`Unsupported SQL type oid`** — 2: `element_types`, `parameters`
- **column scoping / residual bare-alias ref** — 2: `key_column_usage` (`ss`), `check_constraints` (`con`)
- **correlated scalar subquery must be aggregated to ≤1 row** — 1: `table_constraints`
- **upstream DataFusion projection-name assertion** — 1: `constraint_column_usage`
- **`pg_options_to_table` SRF not resolved in this view's shape** — 1: `user_mapping_options`

## Recently fixed
- `columns`, `attributes` — `_pg_truetypid(a.*, t.*)` / `_pg_truetypmod(a.*, t.*)` whole-row
  composite args expanded into scalar columns (`rewrite_pg_truetypid_composite_args` +
  `register_pg_truetypid_helpers`); `pg_column_is_updatable` stub added; and a spurious
  `GROUP BY` fabricated by `rewrite_group_by_for_any` from `= ANY(ARRAY[...])` predicates
  on a query with no GROUP BY was fixed.
- `tables`, `views` — non-literal `::regclass` / `::oid` casts dropped
  (`rewrite_remaining_oid_regclass_casts`); EXISTS/IN subquery tables qualified.
