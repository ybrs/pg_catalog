- pg_get_userbyid


- pg_class
- pg_namespace - table but stub
  \- pg_catalog.pg_tables view works
  \- pg_catalog.pg_views view works



Processed 78 views
  Success: 11
  Missing functions: 0
  Other failures: 67

Views failing with other errors:
- pg_catalog.pg_available_extension_versions (pg_catalog__pg_available_extension_versions.yaml): Error during planning: Column in SELECT must be in GROUP BY or an aggregate function: While expanding wildcard, column "x.extname" must appear in the GROUP BY clause or must be part of an aggregate function, currently only "e.name, e.version, e.superuser, e.trusted, e.relocatable, e.schema, e.requires, e.comment" appears in the SELECT clause satisfies this requirement
- pg_catalog.pg_available_extensions (pg_catalog__pg_available_extensions.yaml): Error during planning: table function 'pg_catalog' not found
- pg_catalog.pg_backend_memory_contexts (pg_catalog__pg_backend_memory_contexts.yaml): Error during planning: table function 'pg_get_backend_memory_contexts' not found

# Note: this can stay as is but need cleanup
- pg_catalog.pg_config (pg_catalog__pg_config.yaml): Error during planning: table function 'pg_catalog' not found
- pg_catalog.pg_cursors (pg_catalog__pg_cursors.yaml): Error during planning: table function 'pg_cursor' not found
- pg_catalog.pg_file_settings (pg_catalog__pg_file_settings.yaml): Error during planning: table function 'pg_show_all_file_settings' not found
- pg_catalog.pg_group (pg_catalog__pg_group.yaml): Schema error: No field named pg_auth_members.roleid. Valid fields are subq0_t.admin_option, subq0_t.grantor, subq0_t.inherit_option, subq0_t.member, subq0_t.oid, subq0_t.roleid, subq0_t.set_option, subq0_t.xmin, subq0_t.xmax, subq0_t.ctid, subq0_t.tableoid, subq0_t.cmin, subq0_t.cmax.
- pg_catalog.pg_hba_file_rules (pg_catalog__pg_hba_file_rules.yaml): Error during planning: table function 'pg_catalog' not found
- pg_catalog.pg_ident_file_mappings (pg_catalog__pg_ident_file_mappings.yaml): Error during planning: table function 'pg_catalog' not found
- pg_catalog.pg_locks (pg_catalog__pg_locks.yaml): Error during planning: table function 'pg_lock_status' not found
- pg_catalog.pg_matviews (pg_catalog__pg_matviews.yaml): Error during planning: Invalid function 'pg_get_userbyid'.
Did you mean 'pg_get_array'?
- pg_catalog.pg_policies (pg_catalog__pg_policies.yaml): Schema error: No field named pg_authid.rolname. Valid fields are subq0_t.oid, subq0_t.rolbypassrls, subq0_t.rolcanlogin, subq0_t.rolconnlimit, subq0_t.rolcreatedb, subq0_t.rolcreaterole, subq0_t.rolinherit, subq0_t.rolname, subq0_t.rolpassword, subq0_t.rolreplication, subq0_t.rolsuper, subq0_t.rolvaliduntil, subq0_t.xmin, subq0_t.xmax, subq0_t.ctid, subq0_t.tableoid, subq0_t.cmin, subq0_t.cmax.
- pg_catalog.pg_prepared_statements (pg_catalog__pg_prepared_statements.yaml): Error during planning: table function 'pg_prepared_statement' not found
- pg_catalog.pg_prepared_xacts (pg_catalog__pg_prepared_xacts.yaml): Error during planning: table function 'pg_prepared_xact' not found
- pg_catalog.pg_publication_tables (pg_catalog__pg_publication_tables.yaml): SQL error: ParserError("Expected: ), found: ARRAY at Line: 9, Column: 48")
- pg_catalog.pg_replication_origin_status (pg_catalog__pg_replication_origin_status.yaml): Error during planning: table function 'pg_show_replication_origin_status' not found
- pg_catalog.pg_replication_slots (pg_catalog__pg_replication_slots.yaml): Error during planning: table function 'pg_get_replication_slots' not found
- pg_catalog.pg_seclabels (pg_catalog__pg_seclabels.yaml): Error during planning: Invalid function 'pg_table_is_visible'.
Did you mean 'pg_relation_size'?
- pg_catalog.pg_sequences (pg_catalog__pg_sequences.yaml): Error during planning: Invalid function 'pg_is_other_temp_schema'.
Did you mean 'pg_catalog.current_schema'?
- pg_catalog.pg_settings (pg_catalog__pg_settings.yaml): Error during planning: table function 'pg_show_all_settings' not found
- pg_catalog.pg_shmem_allocations (pg_catalog__pg_shmem_allocations.yaml): Error during planning: table function 'pg_get_shmem_allocations' not found
- pg_catalog.pg_stat_activity (pg_catalog__pg_stat_activity.yaml): Error during planning: table function 'pg_stat_get_activity' not found
- pg_catalog.pg_stat_all_indexes (pg_catalog__pg_stat_all_indexes.yaml): Error during planning: Invalid function 'pg_stat_get_numscans'.
Did you mean 'pg_catalog.translate'?
- pg_catalog.pg_stat_all_tables (pg_catalog__pg_stat_all_tables.yaml): Error during planning: Invalid function 'pg_stat_get_numscans'.
Did you mean 'pg_catalog.translate'?
- pg_catalog.pg_stat_archiver (pg_catalog__pg_stat_archiver.yaml): Error during planning: table function 'pg_stat_get_archiver' not found
- pg_catalog.pg_stat_bgwriter (pg_catalog__pg_stat_bgwriter.yaml): Error during planning: Invalid function 'pg_stat_get_bgwriter_buf_written_clean'.
Did you mean 'pg_catalog.pg_get_triggerdef'?
- pg_catalog.pg_stat_checkpointer (pg_catalog__pg_stat_checkpointer.yaml): Error during planning: Invalid function 'pg_stat_get_checkpointer_num_timed'.
Did you mean 'pg_postmaster_start_time'?
- pg_catalog.pg_stat_database (pg_catalog__pg_stat_database.yaml): Error during planning: Invalid function 'pg_stat_get_db_numbackends'.
Did you mean 'pg_catalog.txid_current'?
- pg_catalog.pg_stat_database_conflicts (pg_catalog__pg_stat_database_conflicts.yaml): Error during planning: Invalid function 'pg_stat_get_db_conflict_tablespace'.
Did you mean 'pg_get_function_result'?
- pg_catalog.pg_stat_gssapi (pg_catalog__pg_stat_gssapi.yaml): Error during planning: table function 'pg_stat_get_activity' not found
- pg_catalog.pg_stat_io (pg_catalog__pg_stat_io.yaml): Error during planning: table function 'pg_stat_get_io' not found
- pg_catalog.pg_stat_progress_analyze (pg_catalog__pg_stat_progress_analyze.yaml): Error during planning: table function 'pg_stat_get_progress_info' not found
- pg_catalog.pg_stat_progress_basebackup (pg_catalog__pg_stat_progress_basebackup.yaml): Error during planning: table function 'pg_stat_get_progress_info' not found
- pg_catalog.pg_stat_progress_cluster (pg_catalog__pg_stat_progress_cluster.yaml): Error during planning: table function 'pg_stat_get_progress_info' not found
- pg_catalog.pg_stat_progress_copy (pg_catalog__pg_stat_progress_copy.yaml): Error during planning: table function 'pg_stat_get_progress_info' not found
- pg_catalog.pg_stat_progress_create_index (pg_catalog__pg_stat_progress_create_index.yaml): Error during planning: table function 'pg_stat_get_progress_info' not found
- pg_catalog.pg_stat_progress_vacuum (pg_catalog__pg_stat_progress_vacuum.yaml): Error during planning: table function 'pg_stat_get_progress_info' not found
- pg_catalog.pg_stat_recovery_prefetch (pg_catalog__pg_stat_recovery_prefetch.yaml): Error during planning: table function 'pg_stat_get_recovery_prefetch' not found
- pg_catalog.pg_stat_replication (pg_catalog__pg_stat_replication.yaml): Error during planning: table function 'pg_stat_get_activity' not found
- pg_catalog.pg_stat_replication_slots (pg_catalog__pg_stat_replication_slots.yaml): Error during planning: table function 'pg_stat_get_replication_slot' not found
- pg_catalog.pg_stat_slru (pg_catalog__pg_stat_slru.yaml): Error during planning: table function 'pg_stat_get_slru' not found
- pg_catalog.pg_stat_ssl (pg_catalog__pg_stat_ssl.yaml): Error during planning: table function 'pg_stat_get_activity' not found
- pg_catalog.pg_stat_subscription (pg_catalog__pg_stat_subscription.yaml): Error during planning: table function 'pg_stat_get_subscription' not found
- pg_catalog.pg_stat_subscription_stats (pg_catalog__pg_stat_subscription_stats.yaml): Error during planning: table function 'pg_stat_get_subscription_stats' not found
- pg_catalog.pg_stat_user_functions (pg_catalog__pg_stat_user_functions.yaml): Error during planning: Invalid function 'pg_stat_get_function_calls'.
Did you mean 'pg_get_function_result'?
- pg_catalog.pg_stat_user_indexes (pg_catalog__pg_stat_user_indexes.yaml): This feature is not implemented: Unsupported ast node in sqltorel: AllOp { left: Identifier(Ident { value: "schemaname", quote_style: None, span: Span(Location(1,257)..Location(1,267)) }), compare_op: NotEq, right: Array(Array { elem: [Cast { kind: DoubleColon, expr: Value(ValueWithSpan { value: SingleQuotedString("pg_catalog"), span: Span(Location(1,281)..Location(1,293)) }), data_type: Text, format: None }, Cast { kind: DoubleColon, expr: Value(ValueWithSpan { value: SingleQuotedString("information_schema"), span: Span(Location(1,301)..Location(1,321)) }), data_type: Text, format: None }], named: true }) }
- pg_catalog.pg_stat_user_tables (pg_catalog__pg_stat_user_tables.yaml): This feature is not implemented: Unsupported ast node in sqltorel: AllOp { left: Identifier(Ident { value: "schemaname", quote_style: None, span: Span(Location(1,719)..Location(1,729)) }), compare_op: NotEq, right: Array(Array { elem: [Cast { kind: DoubleColon, expr: Value(ValueWithSpan { value: SingleQuotedString("pg_catalog"), span: Span(Location(1,743)..Location(1,755)) }), data_type: Text, format: None }, Cast { kind: DoubleColon, expr: Value(ValueWithSpan { value: SingleQuotedString("information_schema"), span: Span(Location(1,763)..Location(1,783)) }), data_type: Text, format: None }], named: true }) }
- pg_catalog.pg_stat_wal (pg_catalog__pg_stat_wal.yaml): Error during planning: table function 'pg_stat_get_wal' not found
- pg_catalog.pg_stat_wal_receiver (pg_catalog__pg_stat_wal_receiver.yaml): Error during planning: table function 'pg_stat_get_wal_receiver' not found
- pg_catalog.pg_stat_xact_all_tables (pg_catalog__pg_stat_xact_all_tables.yaml): Error during planning: Invalid function 'pg_stat_get_xact_numscans'.
Did you mean 'pg_catalog.translate'?
- pg_catalog.pg_stat_xact_user_functions (pg_catalog__pg_stat_xact_user_functions.yaml): Error during planning: Invalid function 'pg_stat_get_xact_function_calls'.
Did you mean 'pg_get_function_result'?
- pg_catalog.pg_stat_xact_user_tables (pg_catalog__pg_stat_xact_user_tables.yaml): This feature is not implemented: Unsupported ast node in sqltorel: AllOp { left: Identifier(Ident { value: "schemaname", quote_style: None, span: Span(Location(1,333)..Location(1,343)) }), compare_op: NotEq, right: Array(Array { elem: [Cast { kind: DoubleColon, expr: Value(ValueWithSpan { value: SingleQuotedString("pg_catalog"), span: Span(Location(1,357)..Location(1,369)) }), data_type: Text, format: None }, Cast { kind: DoubleColon, expr: Value(ValueWithSpan { value: SingleQuotedString("information_schema"), span: Span(Location(1,377)..Location(1,397)) }), data_type: Text, format: None }], named: true }) }
- pg_catalog.pg_statio_all_indexes (pg_catalog__pg_statio_all_indexes.yaml): Error during planning: Invalid function 'pg_stat_get_blocks_fetched'.
Did you mean 'pg_get_viewdef'?
- pg_catalog.pg_statio_all_sequences (pg_catalog__pg_statio_all_sequences.yaml): Error during planning: Invalid function 'pg_stat_get_blocks_fetched'.
Did you mean 'pg_get_viewdef'?
- pg_catalog.pg_statio_all_tables (pg_catalog__pg_statio_all_tables.yaml): Error during planning: Invalid function 'pg_stat_get_blocks_fetched'.
Did you mean 'pg_get_viewdef'?
- pg_catalog.pg_statio_user_indexes (pg_catalog__pg_statio_user_indexes.yaml): This feature is not implemented: Unsupported ast node in sqltorel: AllOp { left: Identifier(Ident { value: "schemaname", quote_style: None, span: Span(Location(1,212)..Location(1,222)) }), compare_op: NotEq, right: Array(Array { elem: [Cast { kind: DoubleColon, expr: Value(ValueWithSpan { value: SingleQuotedString("pg_catalog"), span: Span(Location(1,236)..Location(1,248)) }), data_type: Text, format: None }, Cast { kind: DoubleColon, expr: Value(ValueWithSpan { value: SingleQuotedString("information_schema"), span: Span(Location(1,256)..Location(1,276)) }), data_type: Text, format: None }], named: true }) }
- pg_catalog.pg_statio_user_sequences (pg_catalog__pg_statio_user_sequences.yaml): This feature is not implemented: Unsupported ast node in sqltorel: AllOp { left: Identifier(Ident { value: "schemaname", quote_style: None, span: Span(Location(1,158)..Location(1,168)) }), compare_op: NotEq, right: Array(Array { elem: [Cast { kind: DoubleColon, expr: Value(ValueWithSpan { value: SingleQuotedString("pg_catalog"), span: Span(Location(1,182)..Location(1,194)) }), data_type: Text, format: None }, Cast { kind: DoubleColon, expr: Value(ValueWithSpan { value: SingleQuotedString("information_schema"), span: Span(Location(1,202)..Location(1,222)) }), data_type: Text, format: None }], named: true }) }
- pg_catalog.pg_statio_user_tables (pg_catalog__pg_statio_user_tables.yaml): This feature is not implemented: Unsupported ast node in sqltorel: AllOp { left: Identifier(Ident { value: "schemaname", quote_style: None, span: Span(Location(1,326)..Location(1,336)) }), compare_op: NotEq, right: Array(Array { elem: [Cast { kind: DoubleColon, expr: Value(ValueWithSpan { value: SingleQuotedString("pg_catalog"), span: Span(Location(1,350)..Location(1,362)) }), data_type: Text, format: None }, Cast { kind: DoubleColon, expr: Value(ValueWithSpan { value: SingleQuotedString("information_schema"), span: Span(Location(1,370)..Location(1,390)) }), data_type: Text, format: None }], named: true }) }
- pg_catalog.pg_stats (pg_catalog__pg_stats.yaml): Error during planning: Invalid function 'has_column_privilege'.
Did you mean 'has_schema_privilege'?
- pg_catalog.pg_stats_ext (pg_catalog__pg_stats_ext.yaml): Schema error: No field named s.stxkeys.
- pg_catalog.pg_stats_ext_exprs (pg_catalog__pg_stats_ext_exprs.yaml): Error during planning: Invalid function 'pg_get_statisticsobjdef_expressions'.
Did you mean 'pg_catalog.pg_get_statisticsobjdef_columns'?
- pg_catalog.pg_timezone_abbrevs (pg_catalog__pg_timezone_abbrevs.yaml): Error during planning: table function 'pg_catalog' not found
- pg_catalog.pg_timezone_names (pg_catalog__pg_timezone_names.yaml): Error during planning: table function 'pg_catalog' not found
- pg_catalog.pg_user_mappings (pg_catalog__pg_user_mappings.yaml): Schema error: No field named pg_authid.rolname. Valid fields are subq0_t.oid, subq0_t.rolbypassrls, subq0_t.rolcanlogin, subq0_t.rolconnlimit, subq0_t.rolcreatedb, subq0_t.rolcreaterole, subq0_t.rolinherit, subq0_t.rolname, subq0_t.rolpassword, subq0_t.rolreplication, subq0_t.rolsuper, subq0_t.rolvaliduntil, subq0_t.xmin, subq0_t.xmax, subq0_t.ctid, subq0_t.tableoid, subq0_t.cmin, subq0_t.cmax.
- pg_catalog.pg_wait_events (pg_catalog__pg_wait_events.yaml): Error during planning: table function 'pg_get_wait_events' not found
