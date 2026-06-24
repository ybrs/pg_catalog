# Failing catalog views — grouped by blocker

From `catalog_views_report.md`: **83 succeed, 53 fail** (136 views total).
A view fails at its FIRST missing symbol; fixing one may reveal another blocker.

## Likely useful (non-monitoring) — prioritize

| Blocker | #views | Views |
|---|---|---|
| `column scoping after rewrite` | 2 | pg_group, pg_stats_ext |
| `upstream DataFusion internal assertion` | 1 | is.constraint_column_usage |
| `multi-column IN subquery` | 1 | is.element_types |
| `pg_options_to_table` | 1 | is.user_mapping_options |
| `spurious GROUP BY (group-by heuristic)` | 1 | pg_available_extension_versions |
| `unsupported SQL type` | 1 | pg_policies |
| `sqlparser parse error` | 1 | pg_publication_tables |
| `pg_table_is_visible` | 1 | pg_seclabels |
| `pg_sequence_last_value` | 1 | pg_sequences |

## Runtime / monitoring views — low value (would be empty/zero stubs)

| Blocker | #views | Views |
|---|---|---|
| `pg_stat_get_progress_info` | 6 | pg_stat_progress_analyze, pg_stat_progress_basebackup, pg_stat_progress_cluster, pg_stat_progress_copy, pg_stat_progress_create_index, pg_stat_progress_vacuum |
| `pg_stat_get_activity` | 4 | pg_stat_activity, pg_stat_gssapi, pg_stat_replication, pg_stat_ssl |
| `pg_stat_get_blocks_fetched` | 3 | pg_statio_all_indexes, pg_statio_all_sequences, pg_statio_all_tables |
| `pg_stat_get_numscans` | 2 | pg_stat_all_indexes, pg_stat_all_tables |
| `pg_get_backend_memory_contexts` | 1 | pg_backend_memory_contexts |
| `pg_cursor` | 1 | pg_cursors |
| `pg_show_all_file_settings` | 1 | pg_file_settings |
| `pg_lock_status` | 1 | pg_locks |
| `pg_prepared_statement` | 1 | pg_prepared_statements |
| `pg_prepared_xact` | 1 | pg_prepared_xacts |
| `pg_show_replication_origin_status` | 1 | pg_replication_origin_status |
| `pg_get_replication_slots` | 1 | pg_replication_slots |
| `pg_get_shmem_allocations` | 1 | pg_shmem_allocations |
| `pg_stat_get_archiver` | 1 | pg_stat_archiver |
| `pg_stat_get_bgwriter_buf_written_clean` | 1 | pg_stat_bgwriter |
| `pg_stat_get_checkpointer_num_timed` | 1 | pg_stat_checkpointer |
| `pg_stat_get_db_numbackends` | 1 | pg_stat_database |
| `pg_stat_get_db_conflict_tablespace` | 1 | pg_stat_database_conflicts |
| `pg_stat_get_io` | 1 | pg_stat_io |
| `pg_stat_get_recovery_prefetch` | 1 | pg_stat_recovery_prefetch |
| `pg_stat_get_replication_slot` | 1 | pg_stat_replication_slots |
| `pg_stat_get_slru` | 1 | pg_stat_slru |
| `pg_stat_get_subscription` | 1 | pg_stat_subscription |
| `pg_stat_get_subscription_stats` | 1 | pg_stat_subscription_stats |
| `pg_stat_get_function_calls` | 1 | pg_stat_user_functions |
| `pg_stat_get_wal` | 1 | pg_stat_wal |
| `pg_stat_get_wal_receiver` | 1 | pg_stat_wal_receiver |
| `pg_stat_get_xact_numscans` | 1 | pg_stat_xact_all_tables |
| `pg_stat_get_xact_function_calls` | 1 | pg_stat_xact_user_functions |
| `row_security_active` | 1 | pg_stats |
| `pg_get_statisticsobjdef_expressions` | 1 | pg_stats_ext_exprs |
| `pg_get_wait_events` | 1 | pg_wait_events |
