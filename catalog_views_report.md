# Catalog view compatibility report

Ran every `type: view` from `pg_catalog_data/pg_schema` against the live engine and recorded whether DataFusion can execute its `view_sql`.

**143 views total** 65 information_schema, 78 pg_catalog.

## Outcome by category

| Category | Count |
|---|---|
| success | 82 |
| missing_table | 37 |
| missing_function | 16 |
| other_error | 5 |
| missing_column | 2 |
| parse_error | 1 |

## Outcome by schema

| Schema | success | missing_table | missing_function | other_error | missing_column | parse_error |
|---|---|---|---|---|---|---|
| information_schema | 61 | 1 | 0 | 3 | 0 | 0 |
| pg_catalog | 21 | 36 | 16 | 2 | 2 | 1 |

## Missing functions (most impactful to implement)

| Function | Views blocked |
|---|---|
| `pg_stat_get_blocks_fetched()` | 3 |
| `pg_stat_get_numscans()` | 2 |
| `pg_table_is_visible()` | 1 |
| `pg_sequence_last_value()` | 1 |
| `pg_stat_get_bgwriter_buf_written_clean()` | 1 |
| `pg_stat_get_checkpointer_num_timed()` | 1 |
| `pg_stat_get_db_numbackends()` | 1 |
| `pg_stat_get_db_conflict_tablespace()` | 1 |
| `pg_stat_get_function_calls()` | 1 |
| `pg_stat_get_xact_numscans()` | 1 |
| `pg_stat_get_xact_function_calls()` | 1 |
| `row_security_active()` | 1 |
| `pg_get_statisticsobjdef_expressions()` | 1 |

## Per-view detail

| Schema.View | Category | Problem |
|---|---|---|
| information_schema._pg_foreign_data_wrappers | success |  |
| information_schema._pg_foreign_servers | success |  |
| information_schema._pg_foreign_table_columns | success |  |
| information_schema._pg_foreign_tables | success |  |
| information_schema._pg_user_mappings | success |  |
| information_schema.administrable_role_authorizations | success |  |
| information_schema.applicable_roles | success |  |
| information_schema.attributes | success |  |
| information_schema.character_sets | success |  |
| information_schema.check_constraint_routine_usage | success |  |
| information_schema.check_constraints | success |  |
| information_schema.collation_character_set_applicability | success |  |
| information_schema.collations | success |  |
| information_schema.column_column_usage | success |  |
| information_schema.column_domain_usage | success |  |
| information_schema.column_options | success |  |
| information_schema.column_privileges | success |  |
| information_schema.column_udt_usage | success |  |
| information_schema.columns | success |  |
| information_schema.constraint_table_usage | success |  |
| information_schema.data_type_privileges | success |  |
| information_schema.domain_constraints | success |  |
| information_schema.domain_udt_usage | success |  |
| information_schema.domains | success |  |
| information_schema.enabled_roles | success |  |
| information_schema.foreign_data_wrapper_options | success |  |
| information_schema.foreign_data_wrappers | success |  |
| information_schema.foreign_server_options | success |  |
| information_schema.foreign_servers | success |  |
| information_schema.foreign_table_options | success |  |
| information_schema.foreign_tables | success |  |
| information_schema.information_schema_catalog_name | success |  |
| information_schema.key_column_usage | success |  |
| information_schema.parameters | success |  |
| information_schema.referential_constraints | success |  |
| information_schema.role_column_grants | success |  |
| information_schema.role_routine_grants | success |  |
| information_schema.role_table_grants | success |  |
| information_schema.role_udt_grants | success |  |
| information_schema.role_usage_grants | success |  |
| information_schema.routine_column_usage | success |  |
| information_schema.routine_privileges | success |  |
| information_schema.routine_routine_usage | success |  |
| information_schema.routine_sequence_usage | success |  |
| information_schema.routine_table_usage | success |  |
| information_schema.routines | success |  |
| information_schema.schemata | success |  |
| information_schema.sequences | success |  |
| information_schema.table_privileges | success |  |
| information_schema.tables | success |  |
| information_schema.transforms | success |  |
| information_schema.triggered_update_columns | success |  |
| information_schema.triggers | success |  |
| information_schema.udt_privileges | success |  |
| information_schema.usage_privileges | success |  |
| information_schema.user_defined_types | success |  |
| information_schema.user_mappings | success |  |
| information_schema.view_column_usage | success |  |
| information_schema.view_routine_usage | success |  |
| information_schema.view_table_usage | success |  |
| information_schema.views | success |  |
| pg_catalog.pg_indexes | success |  |
| pg_catalog.pg_matviews | success |  |
| pg_catalog.pg_roles | success |  |
| pg_catalog.pg_rules | success |  |
| pg_catalog.pg_shadow | success |  |
| pg_catalog.pg_stat_sys_indexes | success |  |
| pg_catalog.pg_stat_sys_tables | success |  |
| pg_catalog.pg_stat_user_indexes | success |  |
| pg_catalog.pg_stat_user_tables | success |  |
| pg_catalog.pg_stat_xact_sys_tables | success |  |
| pg_catalog.pg_stat_xact_user_tables | success |  |
| pg_catalog.pg_statio_sys_indexes | success |  |
| pg_catalog.pg_statio_sys_sequences | success |  |
| pg_catalog.pg_statio_sys_tables | success |  |
| pg_catalog.pg_statio_user_indexes | success |  |
| pg_catalog.pg_statio_user_sequences | success |  |
| pg_catalog.pg_statio_user_tables | success |  |
| pg_catalog.pg_tables | success |  |
| pg_catalog.pg_user | success |  |
| pg_catalog.pg_user_mappings | success |  |
| pg_catalog.pg_views | success |  |
| information_schema.constraint_column_usage | other_error | Internal error: Assertion failed: col.name() == matching_name: Input field name nspname does not match with the projection expression nspname_1. This issue was  |
| information_schema.element_types | other_error | Error during planning: Too many columns! The subquery should only return one column: subq0_t.object_schema, subq0_t.object_name, subq0_t.object_type, subq0_t.dt |
| information_schema.table_constraints | other_error | Invalid (non-executable) plan after Analyzer caused by Error during planning: Correlated scalar subquery must be aggregated to return at most one row |
| information_schema.user_mapping_options | missing_table | Error during planning: table function 'pg_options_to_table' not found |
| pg_catalog.pg_available_extension_versions | other_error | Error during planning: Column in SELECT must be in GROUP BY or an aggregate function: While expanding wildcard, column "x.extname" must appear in the GROUP BY c |
| pg_catalog.pg_available_extensions | missing_table | Error during planning: table function 'pg_catalog' not found |
| pg_catalog.pg_backend_memory_contexts | missing_table | Error during planning: table function 'pg_get_backend_memory_contexts' not found |
| pg_catalog.pg_config | missing_table | Error during planning: table function 'pg_catalog' not found |
| pg_catalog.pg_cursors | missing_table | Error during planning: table function 'pg_cursor' not found |
| pg_catalog.pg_file_settings | missing_table | Error during planning: table function 'pg_show_all_file_settings' not found |
| pg_catalog.pg_group | missing_column | Schema error: No field named pg_authid.oid. Valid fields are subq0_t.admin_option, subq0_t.grantor, subq0_t.inherit_option, subq0_t.member, subq0_t.oid, subq0_t |
| pg_catalog.pg_hba_file_rules | missing_table | Error during planning: table function 'pg_catalog' not found |
| pg_catalog.pg_ident_file_mappings | missing_table | Error during planning: table function 'pg_catalog' not found |
| pg_catalog.pg_locks | missing_table | Error during planning: table function 'pg_lock_status' not found |
| pg_catalog.pg_policies | other_error | This feature is not implemented: Unsupported SQL type name |
| pg_catalog.pg_prepared_statements | missing_table | Error during planning: table function 'pg_prepared_statement' not found |
| pg_catalog.pg_prepared_xacts | missing_table | Error during planning: table function 'pg_prepared_xact' not found |
| pg_catalog.pg_publication_tables | parse_error | Error during planning: failed to parse SQL: sql parser error: Expected: ), found: ARRAY at Line: 9, Column: 48 |
| pg_catalog.pg_replication_origin_status | missing_table | Error during planning: table function 'pg_show_replication_origin_status' not found |
| pg_catalog.pg_replication_slots | missing_table | Error during planning: table function 'pg_get_replication_slots' not found |
| pg_catalog.pg_seclabels | missing_function | function pg_table_is_visible() does not exist |
| pg_catalog.pg_sequences | missing_function | function pg_sequence_last_value() does not exist |
| pg_catalog.pg_settings | missing_table | Error during planning: table function 'pg_show_all_settings' not found |
| pg_catalog.pg_shmem_allocations | missing_table | Error during planning: table function 'pg_get_shmem_allocations' not found |
| pg_catalog.pg_stat_activity | missing_table | Error during planning: table function 'pg_stat_get_activity' not found |
| pg_catalog.pg_stat_all_indexes | missing_function | function pg_stat_get_numscans() does not exist |
| pg_catalog.pg_stat_all_tables | missing_function | function pg_stat_get_numscans() does not exist |
| pg_catalog.pg_stat_archiver | missing_table | Error during planning: table function 'pg_stat_get_archiver' not found |
| pg_catalog.pg_stat_bgwriter | missing_function | function pg_stat_get_bgwriter_buf_written_clean() does not exist |
| pg_catalog.pg_stat_checkpointer | missing_function | function pg_stat_get_checkpointer_num_timed() does not exist |
| pg_catalog.pg_stat_database | missing_function | function pg_stat_get_db_numbackends() does not exist |
| pg_catalog.pg_stat_database_conflicts | missing_function | function pg_stat_get_db_conflict_tablespace() does not exist |
| pg_catalog.pg_stat_gssapi | missing_table | Error during planning: table function 'pg_stat_get_activity' not found |
| pg_catalog.pg_stat_io | missing_table | Error during planning: table function 'pg_stat_get_io' not found |
| pg_catalog.pg_stat_progress_analyze | missing_table | Error during planning: table function 'pg_stat_get_progress_info' not found |
| pg_catalog.pg_stat_progress_basebackup | missing_table | Error during planning: table function 'pg_stat_get_progress_info' not found |
| pg_catalog.pg_stat_progress_cluster | missing_table | Error during planning: table function 'pg_stat_get_progress_info' not found |
| pg_catalog.pg_stat_progress_copy | missing_table | Error during planning: table function 'pg_stat_get_progress_info' not found |
| pg_catalog.pg_stat_progress_create_index | missing_table | Error during planning: table function 'pg_stat_get_progress_info' not found |
| pg_catalog.pg_stat_progress_vacuum | missing_table | Error during planning: table function 'pg_stat_get_progress_info' not found |
| pg_catalog.pg_stat_recovery_prefetch | missing_table | Error during planning: table function 'pg_stat_get_recovery_prefetch' not found |
| pg_catalog.pg_stat_replication | missing_table | Error during planning: table function 'pg_stat_get_activity' not found |
| pg_catalog.pg_stat_replication_slots | missing_table | Error during planning: table function 'pg_stat_get_replication_slot' not found |
| pg_catalog.pg_stat_slru | missing_table | Error during planning: table function 'pg_stat_get_slru' not found |
| pg_catalog.pg_stat_ssl | missing_table | Error during planning: table function 'pg_stat_get_activity' not found |
| pg_catalog.pg_stat_subscription | missing_table | Error during planning: table function 'pg_stat_get_subscription' not found |
| pg_catalog.pg_stat_subscription_stats | missing_table | Error during planning: table function 'pg_stat_get_subscription_stats' not found |
| pg_catalog.pg_stat_user_functions | missing_function | function pg_stat_get_function_calls() does not exist |
| pg_catalog.pg_stat_wal | missing_table | Error during planning: table function 'pg_stat_get_wal' not found |
| pg_catalog.pg_stat_wal_receiver | missing_table | Error during planning: table function 'pg_stat_get_wal_receiver' not found |
| pg_catalog.pg_stat_xact_all_tables | missing_function | function pg_stat_get_xact_numscans() does not exist |
| pg_catalog.pg_stat_xact_user_functions | missing_function | function pg_stat_get_xact_function_calls() does not exist |
| pg_catalog.pg_statio_all_indexes | missing_function | function pg_stat_get_blocks_fetched() does not exist |
| pg_catalog.pg_statio_all_sequences | missing_function | function pg_stat_get_blocks_fetched() does not exist |
| pg_catalog.pg_statio_all_tables | missing_function | function pg_stat_get_blocks_fetched() does not exist |
| pg_catalog.pg_stats | missing_function | function row_security_active() does not exist |
| pg_catalog.pg_stats_ext | missing_column | Schema error: No field named s.stxkeys. |
| pg_catalog.pg_stats_ext_exprs | missing_function | function pg_get_statisticsobjdef_expressions() does not exist |
| pg_catalog.pg_timezone_abbrevs | missing_table | Error during planning: table function 'pg_catalog' not found |
| pg_catalog.pg_timezone_names | missing_table | Error during planning: table function 'pg_catalog' not found |
| pg_catalog.pg_wait_events | missing_table | Error during planning: table function 'pg_get_wait_events' not found |
