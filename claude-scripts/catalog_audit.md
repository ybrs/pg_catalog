# Catalog object audit (machine-checked)

- base tables: 75 (71 pg_catalog, 4 information_schema)
- views: 136 (71 pg_catalog, 65 information_schema)

## View status

| status | count |
|---|---|
| working | 98 |
| partial | 38 |

## Base-table status (every table queried with SELECT count(*))

| status | count |
|---|---|
| working | 75 |

## How declared views are actually served

A YAML `type: view` is only a real view if the server registers it with `CREATE VIEW` (it then re-derives from its base tables on every query). Anything served as a `table` is a frozen MemTable snapshot - a view in name only. Read empirically from each object's query plan.

| served as | count |
|---|---|
| view | 136 |

Of the declared views NOT served as views, whether their `view_sql` would even run if promoted to a real view:

| view_sql | count |
|---|---|

## pg_catalog views

| view | status | served as | view_sql exec | content | diverging / error |
|---|---|---|---|---|---|
| pg_available_extension_versions | partial | view | ok | count-mismatch |  |
| pg_backend_memory_contexts | working | view | ok | match |  |
| pg_cursors | working | view | ok | match |  |
| pg_file_settings | partial | view | ok | count-mismatch |  |
| pg_group | partial | view | ok | content-mismatch | grolist |
| pg_indexes | working | view | ok | match |  |
| pg_locks | working | view | ok | match |  |
| pg_matviews | working | view | ok | match |  |
| pg_policies | working | view | ok | match |  |
| pg_prepared_statements | working | view | ok | match |  |
| pg_prepared_xacts | working | view | ok | match |  |
| pg_publication_tables | working | view | ok | match |  |
| pg_replication_origin_status | working | view | ok | match |  |
| pg_replication_slots | working | view | ok | match |  |
| pg_roles | working | view | ok | match |  |
| pg_rules | partial | view | ok | content-mismatch | definition |
| pg_seclabels | working | view | ok | match |  |
| pg_sequences | working | view | ok | match |  |
| pg_shadow | working | view | ok | match |  |
| pg_shmem_allocations | working | view | ok | match |  |
| pg_stat_activity | working | view | ok | match |  |
| pg_stat_all_indexes | partial | view | ok | count-mismatch |  |
| pg_stat_all_tables | partial | view | ok | count-mismatch |  |
| pg_stat_archiver | working | view | ok | match |  |
| pg_stat_bgwriter | partial | view | ok | count-mismatch |  |
| pg_stat_checkpointer | partial | view | ok | count-mismatch |  |
| pg_stat_database | partial | view | ok | count-mismatch |  |
| pg_stat_database_conflicts | partial | view | ok | count-mismatch |  |
| pg_stat_gssapi | working | view | ok | match |  |
| pg_stat_io | working | view | ok | match |  |
| pg_stat_progress_analyze | working | view | ok | match |  |
| pg_stat_progress_basebackup | working | view | ok | match |  |
| pg_stat_progress_cluster | working | view | ok | match |  |
| pg_stat_progress_copy | working | view | ok | match |  |
| pg_stat_progress_create_index | working | view | ok | match |  |
| pg_stat_progress_vacuum | working | view | ok | match |  |
| pg_stat_recovery_prefetch | working | view | ok | match |  |
| pg_stat_replication | working | view | ok | match |  |
| pg_stat_replication_slots | working | view | ok | match |  |
| pg_stat_slru | working | view | ok | match |  |
| pg_stat_ssl | working | view | ok | match |  |
| pg_stat_subscription | working | view | ok | match |  |
| pg_stat_subscription_stats | working | view | ok | match |  |
| pg_stat_sys_indexes | partial | view | ok | count-mismatch |  |
| pg_stat_sys_tables | partial | view | ok | count-mismatch |  |
| pg_stat_user_functions | working | view | ok | match |  |
| pg_stat_user_indexes | working | view | ok | match |  |
| pg_stat_user_tables | partial | view | ok | count-mismatch |  |
| pg_stat_wal | working | view | ok | match |  |
| pg_stat_wal_receiver | working | view | ok | match |  |
| pg_stat_xact_all_tables | partial | view | ok | count-mismatch |  |
| pg_stat_xact_sys_tables | partial | view | ok | count-mismatch |  |
| pg_stat_xact_user_functions | working | view | ok | match |  |
| pg_stat_xact_user_tables | partial | view | ok | count-mismatch |  |
| pg_statio_all_indexes | partial | view | ok | count-mismatch |  |
| pg_statio_all_sequences | working | view | ok | match |  |
| pg_statio_all_tables | partial | view | ok | count-mismatch |  |
| pg_statio_sys_indexes | partial | view | ok | count-mismatch |  |
| pg_statio_sys_sequences | working | view | ok | match |  |
| pg_statio_sys_tables | partial | view | ok | count-mismatch |  |
| pg_statio_user_indexes | working | view | ok | match |  |
| pg_statio_user_sequences | working | view | ok | match |  |
| pg_statio_user_tables | partial | view | ok | count-mismatch |  |
| pg_stats | working | view | ok | match |  |
| pg_stats_ext | working | view | ok | match |  |
| pg_stats_ext_exprs | working | view | ok | match |  |
| pg_tables | working | view | ok | match |  |
| pg_user | working | view | ok | match |  |
| pg_user_mappings | working | view | ok | match |  |
| pg_views | partial | view | ok | content-mismatch | definition |
| pg_wait_events | partial | view | ok | count-mismatch |  |

## information_schema views

| view | status | served as | view_sql exec | content | diverging / error |
|---|---|---|---|---|---|
| _pg_foreign_data_wrappers | working | view | ok | match |  |
| _pg_foreign_servers | working | view | ok | match |  |
| _pg_foreign_table_columns | working | view | ok | match |  |
| _pg_foreign_tables | working | view | ok | match |  |
| _pg_user_mappings | working | view | ok | match |  |
| administrable_role_authorizations | working | view | ok | match |  |
| applicable_roles | working | view | ok | match |  |
| attributes | working | view | ok | match |  |
| character_sets | working | view | ok | match |  |
| check_constraint_routine_usage | working | view | ok | match |  |
| check_constraints | partial | view | ok | content-mismatch | check_clause |
| collation_character_set_applicability | working | view | ok | match |  |
| collations | working | view | ok | match |  |
| column_column_usage | working | view | ok | match |  |
| column_domain_usage | working | view | ok | match |  |
| column_options | working | view | ok | match |  |
| column_privileges | partial | view | ok | count-mismatch |  |
| column_udt_usage | working | view | ok | match |  |
| columns | partial | view | ok | content-mismatch | is_updatable |
| constraint_column_usage | working | view | ok | match |  |
| constraint_table_usage | working | view | ok | match |  |
| data_type_privileges | working | view | ok | match |  |
| domain_constraints | working | view | ok | match |  |
| domain_udt_usage | working | view | ok | match |  |
| domains | working | view | ok | match |  |
| element_types | working | view | ok | match |  |
| enabled_roles | working | view | ok | match |  |
| foreign_data_wrapper_options | working | view | ok | match |  |
| foreign_data_wrappers | working | view | ok | match |  |
| foreign_server_options | working | view | ok | match |  |
| foreign_servers | working | view | ok | match |  |
| foreign_table_options | working | view | ok | match |  |
| foreign_tables | working | view | ok | match |  |
| information_schema_catalog_name | working | view | ok | match |  |
| key_column_usage | working | view | ok | match |  |
| parameters | partial | view | ok | content-mismatch | parameter_default |
| referential_constraints | working | view | ok | match |  |
| role_column_grants | partial | view | ok | count-mismatch |  |
| role_routine_grants | partial | view | ok | count-mismatch |  |
| role_table_grants | partial | view | ok | count-mismatch |  |
| role_udt_grants | partial | view | ok | count-mismatch |  |
| role_usage_grants | partial | view | ok | count-mismatch |  |
| routine_column_usage | working | view | ok | match |  |
| routine_privileges | partial | view | ok | count-mismatch |  |
| routine_routine_usage | working | view | ok | match |  |
| routine_sequence_usage | working | view | ok | match |  |
| routine_table_usage | working | view | ok | match |  |
| routines | working | view | ok | match |  |
| schemata | working | view | ok | match |  |
| sequences | working | view | ok | match |  |
| table_constraints | working | view | ok | match |  |
| table_privileges | partial | view | ok | count-mismatch |  |
| tables | partial | view | ok | content-mismatch | is_insertable_into |
| transforms | working | view | ok | match |  |
| triggered_update_columns | working | view | ok | match |  |
| triggers | working | view | ok | match |  |
| udt_privileges | partial | view | ok | count-mismatch |  |
| usage_privileges | partial | view | ok | count-mismatch |  |
| user_defined_types | working | view | ok | match |  |
| user_mapping_options | working | view | ok | match |  |
| user_mappings | working | view | ok | match |  |
| view_column_usage | working | view | ok | match |  |
| view_routine_usage | working | view | ok | match |  |
| view_table_usage | working | view | ok | match |  |
| views | partial | view | ok | content-mismatch | is_insertable_into, is_updatable, view_definition |

## pg_catalog base tables

| table | status | rows | error |
|---|---|---|---|
| pg_aggregate | working | 157 |  |
| pg_am | working | 7 |  |
| pg_amop | working | 945 |  |
| pg_amproc | working | 696 |  |
| pg_attrdef | working | 0 |  |
| pg_attribute | working | 3128 |  |
| pg_auth_members | working | 3 |  |
| pg_authid | working | 16 |  |
| pg_available_extensions | working | 49 |  |
| pg_cast | working | 229 |  |
| pg_class | working | 416 |  |
| pg_collation | working | 7 |  |
| pg_config | working | 23 |  |
| pg_constraint | working | 112 |  |
| pg_conversion | working | 128 |  |
| pg_database | working | 4 |  |
| pg_db_role_setting | working | 0 |  |
| pg_default_acl | working | 0 |  |
| pg_depend | working | 1709 |  |
| pg_description | working | 4433 |  |
| pg_enum | working | 0 |  |
| pg_event_trigger | working | 0 |  |
| pg_extension | working | 1 |  |
| pg_foreign_data_wrapper | working | 0 |  |
| pg_foreign_server | working | 0 |  |
| pg_foreign_table | working | 0 |  |
| pg_hba_file_rules | working | 6 |  |
| pg_ident_file_mappings | working | 0 |  |
| pg_index | working | 164 |  |
| pg_inherits | working | 0 |  |
| pg_init_privs | working | 223 |  |
| pg_language | working | 4 |  |
| pg_largeobject | working | 0 |  |
| pg_largeobject_metadata | working | 0 |  |
| pg_namespace | working | 4 |  |
| pg_opclass | working | 177 |  |
| pg_operator | working | 799 |  |
| pg_opfamily | working | 146 |  |
| pg_parameter_acl | working | 0 |  |
| pg_partitioned_table | working | 0 |  |
| pg_policy | working | 0 |  |
| pg_proc | working | 3330 |  |
| pg_publication | working | 0 |  |
| pg_publication_namespace | working | 0 |  |
| pg_publication_rel | working | 0 |  |
| pg_range | working | 6 |  |
| pg_replication_origin | working | 0 |  |
| pg_rewrite | working | 145 |  |
| pg_seclabel | working | 0 |  |
| pg_sequence | working | 0 |  |
| pg_settings | working | 377 |  |
| pg_shdepend | working | 0 |  |
| pg_shdescription | working | 3 |  |
| pg_shseclabel | working | 0 |  |
| pg_statistic | working | 0 |  |
| pg_statistic_ext | working | 0 |  |
| pg_statistic_ext_data | working | 0 |  |
| pg_subscription | working | 0 |  |
| pg_subscription_rel | working | 0 |  |
| pg_tablespace | working | 2 |  |
| pg_timezone_abbrevs | working | 195 |  |
| pg_timezone_names | working | 487 |  |
| pg_transform | working | 0 |  |
| pg_trigger | working | 0 |  |
| pg_ts_config | working | 29 |  |
| pg_ts_config_map | working | 551 |  |
| pg_ts_dict | working | 29 |  |
| pg_ts_parser | working | 1 |  |
| pg_ts_template | working | 5 |  |
| pg_type | working | 618 |  |
| pg_user_mapping | working | 0 |  |

## information_schema base tables

| table | status | rows | error |
|---|---|---|---|
| sql_features | working | 755 |  |
| sql_implementation_info | working | 12 |  |
| sql_parts | working | 11 |  |
| sql_sizing | working | 23 |  |
