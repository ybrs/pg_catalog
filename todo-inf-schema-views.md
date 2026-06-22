# information_schema views — status

Generated from `analyze_catalog_views.py` against the pg_catalog engine on DataFusion 54.

**60 working, 5 to do, 65 total.**

"Working" = the view SQL plans & executes on our engine (can be promoted to a live view). Today these are still served as materialized-snapshot MemTables.

## To do
| View | Status | What's needed |
|---|---|---|
| `element_types` | unsupported_cast_or_type | `COALESCE(proallargtypes, proargtypes)` can't coerce `List<Utf8>` (oid[]) vs `List<Int64>` (oidvector) to a common element type. (The earlier `::oid[]` blocker is fixed by `drop_redundant_oid_and_regclass_casts`.) |
| `parameters` | unsupported_cast_or_type | same `COALESCE` list-element-type mismatch as `element_types` |
| `table_constraints` | other_error | correlated scalar subquery must be aggregated to ≤1 row |
| `constraint_column_usage` | other_error | upstream DataFusion projection-name assertion (`nspname` vs `nspname_1`) |
| `user_mapping_options` | missing_table | `LATERAL pg_options_to_table(x) opts(c1,c2)` set-returning function in the FROM clause — needs a FROM-clause unnest rewrite (the SRF rewrite only covers projection/aliased forms). View is empty in practice (no user mappings). |

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
| `check_constraints` |
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
| `key_column_usage` |
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

- **`COALESCE` over mismatched list element types (`List<Utf8>` vs `List<Int64>`)** — 2: `element_types`, `parameters`
- **correlated scalar subquery must be aggregated to ≤1 row** — 1: `table_constraints`
- **upstream DataFusion projection-name assertion** — 1: `constraint_column_usage`
- **`LATERAL srf(...)` set-returning function in FROM** — 1: `user_mapping_options`

## Recently fixed
- `key_column_usage`, `check_constraints` — the dot→subscript pass was wrongly turning
  `tbl.arraycol[i]` into `tbl['arraycol'][i]` (sqlparser parses `a.b[1]` as
  `root=a, [Dot(b), Subscript(1)]`); now only parenthesized `(expr).field` / `(srf()).f`
  roots convert. `check_constraints` also needed `format()` (`register_format`, supports
  `%s`/`%I`/`%L`/`%%`).
- `columns`, `attributes` — `_pg_truetypid(a.*, t.*)` / `_pg_truetypmod(a.*, t.*)` whole-row
  composite args expanded into scalar columns (`rewrite_pg_truetypid_composite_args` +
  `register_pg_truetypid_helpers`); `pg_column_is_updatable` stub added; and a spurious
  `GROUP BY` fabricated by `rewrite_group_by_for_any` from `= ANY(ARRAY[...])` predicates
  on a query with no GROUP BY was fixed.
- `tables`, `views` — non-literal `::regclass` / `::oid` casts dropped
  (`rewrite_remaining_oid_regclass_casts`); EXISTS/IN subquery tables qualified.
