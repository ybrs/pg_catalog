# Catalog runtime-function contract

These 111 functions are referenced by catalog views but cannot be computed by a static
catalog: 19 set-returning (table) and 92 scalar. Every one is now wired as an
integration-installable resolver in `src/runtime_function_resolvers.rs` - empty / NULL
by default, so the calling views are real (empty) views until an integration supplies
values via the typed `set_<fn>_resolver` setters. This file is the contract: the
signature and (for table functions) output schema each resolver must satisfy.
Signatures are from the seed `pg_catalog.pg_proc` (i.e. real PostgreSQL).

## Set-returning (table) functions (19)

### `pg_cursor((no args))`
- returns: SETOF rows of (name text, statement text, is_holdable bool, is_binary bool, is_scrollable bool, creation_time timestamptz)
- used by: pg_catalog.pg_cursors

### `pg_get_backend_memory_contexts((no args))`
- returns: SETOF rows of (name text, ident text, parent text, level int4, total_bytes int8, total_nblocks int8, free_bytes int8, free_chunks int8, used_bytes int8)
- used by: pg_catalog.pg_backend_memory_contexts

### `pg_get_publication_tables(pubname _text)`
- returns: SETOF rows of (pubid oid, relid oid, attrs int2vector, qual pg_node_tree)
- used by: pg_catalog.pg_publication_tables

### `pg_get_replication_slots((no args))`
- returns: SETOF rows of (slot_name name, plugin name, slot_type text, datoid oid, temporary bool, active bool, active_pid int4, xmin xid, catalog_xmin xid, restart_lsn pg_lsn, confirmed_flush_lsn pg_lsn, wal_status text, safe_wal_size int8, two_phase bool, inactive_since timestamptz, conflicting bool, invalidation_reason text, failover bool, synced bool)
- used by: pg_catalog.pg_replication_slots

### `pg_get_shmem_allocations((no args))`
- returns: SETOF rows of (name text, off int8, size int8, allocated_size int8)
- used by: pg_catalog.pg_shmem_allocations

### `pg_get_wait_events((no args))`
- returns: SETOF rows of (type text, name text, description text)
- used by: pg_catalog.pg_wait_events

### `pg_lock_status((no args))`
- returns: SETOF rows of (locktype text, database oid, relation oid, page int4, tuple int2, virtualxid text, transactionid xid, classid oid, objid oid, objsubid int2, virtualtransaction text, pid int4, mode text, granted bool, fastpath bool, waitstart timestamptz)
- used by: pg_catalog.pg_locks

### `pg_mcv_list_items(mcv_list pg_mcv_list)`
- returns: SETOF rows of (index int4, values _text, nulls _bool, frequency float8, base_frequency float8)
- used by: pg_catalog.pg_stats_ext

### `pg_prepared_statement((no args))`
- returns: SETOF rows of (name text, statement text, prepare_time timestamptz, parameter_types _regtype, result_types _regtype, from_sql bool, generic_plans int8, custom_plans int8)
- used by: pg_catalog.pg_prepared_statements

### `pg_prepared_xact((no args))`
- returns: SETOF rows of (transaction xid, gid text, prepared timestamptz, ownerid oid, dbid oid)
- used by: pg_catalog.pg_prepared_xacts

### `pg_show_all_file_settings((no args))`
- returns: SETOF rows of (sourcefile text, sourceline int4, seqno int4, name text, setting text, applied bool, error text)
- used by: pg_catalog.pg_file_settings

### `pg_show_replication_origin_status((no args))`
- returns: SETOF rows of (local_id oid, external_id text, remote_lsn pg_lsn, local_lsn pg_lsn)
- used by: pg_catalog.pg_replication_origin_status

### `pg_stat_get_activity(pid int4)`
- returns: SETOF rows of (datid oid, pid int4, usesysid oid, application_name text, state text, query text, wait_event_type text, wait_event text, xact_start timestamptz, query_start timestamptz, backend_start timestamptz, state_change timestamptz, client_addr inet, client_hostname text, client_port int4, backend_xid xid, backend_xmin xid, backend_type text, ssl bool, sslversion text, sslcipher text, sslbits int4, ssl_client_dn text, ssl_client_serial numeric, ssl_issuer_dn text, gss_auth bool, gss_princ text, gss_enc bool, gss_delegation bool, leader_pid int4, query_id int8)
- used by: pg_catalog.pg_stat_activity, pg_catalog.pg_stat_gssapi, pg_catalog.pg_stat_replication, pg_catalog.pg_stat_ssl

### `pg_stat_get_io((no args))`
- returns: SETOF rows of (backend_type text, object text, context text, reads int8, read_time float8, writes int8, write_time float8, writebacks int8, writeback_time float8, extends int8, extend_time float8, op_bytes int8, hits int8, evictions int8, reuses int8, fsyncs int8, fsync_time float8, stats_reset timestamptz)
- used by: pg_catalog.pg_stat_io

### `pg_stat_get_progress_info(cmdtype text)`
- returns: SETOF rows of (pid int4, datid oid, relid oid, param1 int8, param2 int8, param3 int8, param4 int8, param5 int8, param6 int8, param7 int8, param8 int8, param9 int8, param10 int8, param11 int8, param12 int8, param13 int8, param14 int8, param15 int8, param16 int8, param17 int8, param18 int8, param19 int8, param20 int8)
- used by: pg_catalog.pg_stat_progress_analyze, pg_catalog.pg_stat_progress_basebackup, pg_catalog.pg_stat_progress_cluster, pg_catalog.pg_stat_progress_copy, pg_catalog.pg_stat_progress_create_index, pg_catalog.pg_stat_progress_vacuum

### `pg_stat_get_recovery_prefetch((no args))`
- returns: SETOF rows of (stats_reset timestamptz, prefetch int8, hit int8, skip_init int8, skip_new int8, skip_fpw int8, skip_rep int8, wal_distance int4, block_distance int4, io_depth int4)
- used by: pg_catalog.pg_stat_recovery_prefetch

### `pg_stat_get_slru((no args))`
- returns: SETOF rows of (name text, blks_zeroed int8, blks_hit int8, blks_read int8, blks_written int8, blks_exists int8, flushes int8, truncates int8, stats_reset timestamptz)
- used by: pg_catalog.pg_stat_slru

### `pg_stat_get_subscription(subid oid)`
- returns: SETOF rows of (subid oid, relid oid, pid int4, leader_pid int4, received_lsn pg_lsn, last_msg_send_time timestamptz, last_msg_receipt_time timestamptz, latest_end_lsn pg_lsn, latest_end_time timestamptz, worker_type text)
- used by: pg_catalog.pg_stat_subscription

### `pg_stat_get_wal_senders((no args))`
- returns: SETOF rows of (pid int4, state text, sent_lsn pg_lsn, write_lsn pg_lsn, flush_lsn pg_lsn, replay_lsn pg_lsn, write_lag interval, flush_lag interval, replay_lag interval, sync_priority int4, sync_state text, reply_time timestamptz)
- used by: pg_catalog.pg_stat_replication

## Scalar functions (92)

### `pg_function_is_visible($1 oid)`
- returns: bool
- used by: pg_catalog.pg_seclabels

### `pg_get_statisticsobjdef_expressions($1 oid)`
- returns: _text
- used by: pg_catalog.pg_stats_ext, pg_catalog.pg_stats_ext_exprs

### `pg_indexam_progress_phasename($1 oid, $2 int8)`
- returns: text
- used by: pg_catalog.pg_stat_progress_create_index

### `pg_stat_get_analyze_count($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_all_tables

### `pg_stat_get_archiver((no args))`
- returns: record
- used by: pg_catalog.pg_stat_archiver

### `pg_stat_get_autoanalyze_count($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_all_tables

### `pg_stat_get_autovacuum_count($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_all_tables

### `pg_stat_get_bgwriter_buf_written_clean((no args))`
- returns: int8
- used by: pg_catalog.pg_stat_bgwriter

### `pg_stat_get_bgwriter_maxwritten_clean((no args))`
- returns: int8
- used by: pg_catalog.pg_stat_bgwriter

### `pg_stat_get_bgwriter_stat_reset_time((no args))`
- returns: timestamptz
- used by: pg_catalog.pg_stat_bgwriter

### `pg_stat_get_blocks_fetched($1 oid)`
- returns: int8
- used by: pg_catalog.pg_statio_all_indexes, pg_catalog.pg_statio_all_sequences, pg_catalog.pg_statio_all_tables

### `pg_stat_get_blocks_hit($1 oid)`
- returns: int8
- used by: pg_catalog.pg_statio_all_indexes, pg_catalog.pg_statio_all_sequences, pg_catalog.pg_statio_all_tables

### `pg_stat_get_buf_alloc((no args))`
- returns: int8
- used by: pg_catalog.pg_stat_bgwriter

### `pg_stat_get_checkpointer_buffers_written((no args))`
- returns: int8
- used by: pg_catalog.pg_stat_checkpointer

### `pg_stat_get_checkpointer_num_requested((no args))`
- returns: int8
- used by: pg_catalog.pg_stat_checkpointer

### `pg_stat_get_checkpointer_num_timed((no args))`
- returns: int8
- used by: pg_catalog.pg_stat_checkpointer

### `pg_stat_get_checkpointer_restartpoints_performed((no args))`
- returns: int8
- used by: pg_catalog.pg_stat_checkpointer

### `pg_stat_get_checkpointer_restartpoints_requested((no args))`
- returns: int8
- used by: pg_catalog.pg_stat_checkpointer

### `pg_stat_get_checkpointer_restartpoints_timed((no args))`
- returns: int8
- used by: pg_catalog.pg_stat_checkpointer

### `pg_stat_get_checkpointer_stat_reset_time((no args))`
- returns: timestamptz
- used by: pg_catalog.pg_stat_checkpointer

### `pg_stat_get_checkpointer_sync_time((no args))`
- returns: float8
- used by: pg_catalog.pg_stat_checkpointer

### `pg_stat_get_checkpointer_write_time((no args))`
- returns: float8
- used by: pg_catalog.pg_stat_checkpointer

### `pg_stat_get_db_active_time($1 oid)`
- returns: float8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_blk_read_time($1 oid)`
- returns: float8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_blk_write_time($1 oid)`
- returns: float8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_blocks_fetched($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_blocks_hit($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_checksum_failures($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_checksum_last_failure($1 oid)`
- returns: timestamptz
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_conflict_all($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_conflict_bufferpin($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database_conflicts

### `pg_stat_get_db_conflict_lock($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database_conflicts

### `pg_stat_get_db_conflict_logicalslot($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database_conflicts

### `pg_stat_get_db_conflict_snapshot($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database_conflicts

### `pg_stat_get_db_conflict_startup_deadlock($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database_conflicts

### `pg_stat_get_db_conflict_tablespace($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database_conflicts

### `pg_stat_get_db_deadlocks($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_idle_in_transaction_time($1 oid)`
- returns: float8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_numbackends($1 oid)`
- returns: int4
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_session_time($1 oid)`
- returns: float8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_sessions($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_sessions_abandoned($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_sessions_fatal($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_sessions_killed($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_stat_reset_time($1 oid)`
- returns: timestamptz
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_temp_bytes($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_temp_files($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_tuples_deleted($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_tuples_fetched($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_tuples_inserted($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_tuples_returned($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_tuples_updated($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_xact_commit($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_db_xact_rollback($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_database

### `pg_stat_get_dead_tuples($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_all_tables

### `pg_stat_get_function_calls($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_user_functions

### `pg_stat_get_function_self_time($1 oid)`
- returns: float8
- used by: pg_catalog.pg_stat_user_functions

### `pg_stat_get_function_total_time($1 oid)`
- returns: float8
- used by: pg_catalog.pg_stat_user_functions

### `pg_stat_get_ins_since_vacuum($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_all_tables

### `pg_stat_get_last_analyze_time($1 oid)`
- returns: timestamptz
- used by: pg_catalog.pg_stat_all_tables

### `pg_stat_get_last_autoanalyze_time($1 oid)`
- returns: timestamptz
- used by: pg_catalog.pg_stat_all_tables

### `pg_stat_get_last_autovacuum_time($1 oid)`
- returns: timestamptz
- used by: pg_catalog.pg_stat_all_tables

### `pg_stat_get_last_vacuum_time($1 oid)`
- returns: timestamptz
- used by: pg_catalog.pg_stat_all_tables

### `pg_stat_get_lastscan($1 oid)`
- returns: timestamptz
- used by: pg_catalog.pg_stat_all_indexes, pg_catalog.pg_stat_all_tables

### `pg_stat_get_live_tuples($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_all_tables

### `pg_stat_get_mod_since_analyze($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_all_tables

### `pg_stat_get_numscans($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_all_indexes, pg_catalog.pg_stat_all_tables

### `pg_stat_get_replication_slot(slot_name text)`
- returns: record
- used by: pg_catalog.pg_stat_replication_slots

### `pg_stat_get_subscription_stats(subid oid)`
- returns: record
- used by: pg_catalog.pg_stat_subscription_stats

### `pg_stat_get_tuples_deleted($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_all_tables

### `pg_stat_get_tuples_fetched($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_all_indexes, pg_catalog.pg_stat_all_tables

### `pg_stat_get_tuples_hot_updated($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_all_tables

### `pg_stat_get_tuples_inserted($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_all_tables

### `pg_stat_get_tuples_newpage_updated($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_all_tables

### `pg_stat_get_tuples_returned($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_all_indexes, pg_catalog.pg_stat_all_tables

### `pg_stat_get_tuples_updated($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_all_tables

### `pg_stat_get_vacuum_count($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_all_tables

### `pg_stat_get_wal((no args))`
- returns: record
- used by: pg_catalog.pg_stat_wal

### `pg_stat_get_wal_receiver((no args))`
- returns: record
- used by: pg_catalog.pg_stat_wal_receiver

### `pg_stat_get_xact_function_calls($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_xact_user_functions

### `pg_stat_get_xact_function_self_time($1 oid)`
- returns: float8
- used by: pg_catalog.pg_stat_xact_user_functions

### `pg_stat_get_xact_function_total_time($1 oid)`
- returns: float8
- used by: pg_catalog.pg_stat_xact_user_functions

### `pg_stat_get_xact_numscans($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_xact_all_tables

### `pg_stat_get_xact_tuples_deleted($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_xact_all_tables

### `pg_stat_get_xact_tuples_fetched($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_xact_all_tables

### `pg_stat_get_xact_tuples_hot_updated($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_xact_all_tables

### `pg_stat_get_xact_tuples_inserted($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_xact_all_tables

### `pg_stat_get_xact_tuples_newpage_updated($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_xact_all_tables

### `pg_stat_get_xact_tuples_returned($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_xact_all_tables

### `pg_stat_get_xact_tuples_updated($1 oid)`
- returns: int8
- used by: pg_catalog.pg_stat_xact_all_tables

### `pg_table_is_visible($1 oid)`
- returns: bool
- used by: pg_catalog.pg_seclabels

### `pg_type_is_visible($1 oid)`
- returns: bool
- used by: pg_catalog.pg_seclabels
