# Runtime function reference

Generated from `src/runtime_function_resolvers.rs` by
`claude-scripts/generate_runtime_function_reference.py` - do not edit by hand.

Every function below is supplied through the named setter; all setters, resolver
type aliases, and row structs are re-exported from the crate root
(`use datafusion_pg_catalog::...`). See [runtime-functions.md](runtime-functions.md)
for the guide and worked examples.

## Scalar functions

Each takes the listed `Arc<dyn Fn ...>` resolver. A `timestamptz` result is an
`i64` count of microseconds since the Unix epoch, UTC.

| Function | Setter | Resolver type | Default |
| --- | --- | --- | --- |
| `pg_function_is_visible` | `set_pg_function_is_visible_resolver` | `Arc<dyn Fn(i64) -> Option<bool> + Send + Sync>` | `true` |
| `pg_get_statisticsobjdef_expressions` | `set_pg_get_statisticsobjdef_expressions_resolver` | `Arc<dyn Fn(i64) -> Option<Vec<String>> + Send + Sync>` | `None` (SQL NULL) |
| `pg_indexam_progress_phasename` | `set_pg_indexam_progress_phasename_resolver` | `Arc<dyn Fn(i64, i64) -> Option<String> + Send + Sync>` | `None` (SQL NULL) |
| `pg_sequence_last_value` | `set_pg_sequence_last_value_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_analyze_count` | `set_pg_stat_get_analyze_count_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_autoanalyze_count` | `set_pg_stat_get_autoanalyze_count_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_autovacuum_count` | `set_pg_stat_get_autovacuum_count_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_bgwriter_buf_written_clean` | `set_pg_stat_get_bgwriter_buf_written_clean_resolver` | `Arc<dyn Fn() -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_bgwriter_maxwritten_clean` | `set_pg_stat_get_bgwriter_maxwritten_clean_resolver` | `Arc<dyn Fn() -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_bgwriter_stat_reset_time` | `set_pg_stat_get_bgwriter_stat_reset_time_resolver` | `Arc<dyn Fn() -> Option<i64> + Send + Sync>` _(timestamp, microseconds UTC)_ | `None` (SQL NULL) |
| `pg_stat_get_blocks_fetched` | `set_pg_stat_get_blocks_fetched_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_blocks_hit` | `set_pg_stat_get_blocks_hit_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_buf_alloc` | `set_pg_stat_get_buf_alloc_resolver` | `Arc<dyn Fn() -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_checkpointer_buffers_written` | `set_pg_stat_get_checkpointer_buffers_written_resolver` | `Arc<dyn Fn() -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_checkpointer_num_requested` | `set_pg_stat_get_checkpointer_num_requested_resolver` | `Arc<dyn Fn() -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_checkpointer_num_timed` | `set_pg_stat_get_checkpointer_num_timed_resolver` | `Arc<dyn Fn() -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_checkpointer_restartpoints_performed` | `set_pg_stat_get_checkpointer_restartpoints_performed_resolver` | `Arc<dyn Fn() -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_checkpointer_restartpoints_requested` | `set_pg_stat_get_checkpointer_restartpoints_requested_resolver` | `Arc<dyn Fn() -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_checkpointer_restartpoints_timed` | `set_pg_stat_get_checkpointer_restartpoints_timed_resolver` | `Arc<dyn Fn() -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_checkpointer_stat_reset_time` | `set_pg_stat_get_checkpointer_stat_reset_time_resolver` | `Arc<dyn Fn() -> Option<i64> + Send + Sync>` _(timestamp, microseconds UTC)_ | `None` (SQL NULL) |
| `pg_stat_get_checkpointer_sync_time` | `set_pg_stat_get_checkpointer_sync_time_resolver` | `Arc<dyn Fn() -> Option<f64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_checkpointer_write_time` | `set_pg_stat_get_checkpointer_write_time_resolver` | `Arc<dyn Fn() -> Option<f64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_active_time` | `set_pg_stat_get_db_active_time_resolver` | `Arc<dyn Fn(i64) -> Option<f64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_blk_read_time` | `set_pg_stat_get_db_blk_read_time_resolver` | `Arc<dyn Fn(i64) -> Option<f64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_blk_write_time` | `set_pg_stat_get_db_blk_write_time_resolver` | `Arc<dyn Fn(i64) -> Option<f64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_blocks_fetched` | `set_pg_stat_get_db_blocks_fetched_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_blocks_hit` | `set_pg_stat_get_db_blocks_hit_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_checksum_failures` | `set_pg_stat_get_db_checksum_failures_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_checksum_last_failure` | `set_pg_stat_get_db_checksum_last_failure_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` _(timestamp, microseconds UTC)_ | `None` (SQL NULL) |
| `pg_stat_get_db_conflict_all` | `set_pg_stat_get_db_conflict_all_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_conflict_bufferpin` | `set_pg_stat_get_db_conflict_bufferpin_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_conflict_lock` | `set_pg_stat_get_db_conflict_lock_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_conflict_logicalslot` | `set_pg_stat_get_db_conflict_logicalslot_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_conflict_snapshot` | `set_pg_stat_get_db_conflict_snapshot_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_conflict_startup_deadlock` | `set_pg_stat_get_db_conflict_startup_deadlock_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_conflict_tablespace` | `set_pg_stat_get_db_conflict_tablespace_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_deadlocks` | `set_pg_stat_get_db_deadlocks_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_idle_in_transaction_time` | `set_pg_stat_get_db_idle_in_transaction_time_resolver` | `Arc<dyn Fn(i64) -> Option<f64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_numbackends` | `set_pg_stat_get_db_numbackends_resolver` | `Arc<dyn Fn(i64) -> Option<i32> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_session_time` | `set_pg_stat_get_db_session_time_resolver` | `Arc<dyn Fn(i64) -> Option<f64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_sessions` | `set_pg_stat_get_db_sessions_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_sessions_abandoned` | `set_pg_stat_get_db_sessions_abandoned_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_sessions_fatal` | `set_pg_stat_get_db_sessions_fatal_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_sessions_killed` | `set_pg_stat_get_db_sessions_killed_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_stat_reset_time` | `set_pg_stat_get_db_stat_reset_time_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` _(timestamp, microseconds UTC)_ | `None` (SQL NULL) |
| `pg_stat_get_db_temp_bytes` | `set_pg_stat_get_db_temp_bytes_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_temp_files` | `set_pg_stat_get_db_temp_files_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_tuples_deleted` | `set_pg_stat_get_db_tuples_deleted_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_tuples_fetched` | `set_pg_stat_get_db_tuples_fetched_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_tuples_inserted` | `set_pg_stat_get_db_tuples_inserted_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_tuples_returned` | `set_pg_stat_get_db_tuples_returned_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_tuples_updated` | `set_pg_stat_get_db_tuples_updated_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_xact_commit` | `set_pg_stat_get_db_xact_commit_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_db_xact_rollback` | `set_pg_stat_get_db_xact_rollback_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_dead_tuples` | `set_pg_stat_get_dead_tuples_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_function_calls` | `set_pg_stat_get_function_calls_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_function_self_time` | `set_pg_stat_get_function_self_time_resolver` | `Arc<dyn Fn(i64) -> Option<f64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_function_total_time` | `set_pg_stat_get_function_total_time_resolver` | `Arc<dyn Fn(i64) -> Option<f64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_ins_since_vacuum` | `set_pg_stat_get_ins_since_vacuum_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_last_analyze_time` | `set_pg_stat_get_last_analyze_time_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` _(timestamp, microseconds UTC)_ | `None` (SQL NULL) |
| `pg_stat_get_last_autoanalyze_time` | `set_pg_stat_get_last_autoanalyze_time_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` _(timestamp, microseconds UTC)_ | `None` (SQL NULL) |
| `pg_stat_get_last_autovacuum_time` | `set_pg_stat_get_last_autovacuum_time_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` _(timestamp, microseconds UTC)_ | `None` (SQL NULL) |
| `pg_stat_get_last_vacuum_time` | `set_pg_stat_get_last_vacuum_time_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` _(timestamp, microseconds UTC)_ | `None` (SQL NULL) |
| `pg_stat_get_lastscan` | `set_pg_stat_get_lastscan_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` _(timestamp, microseconds UTC)_ | `None` (SQL NULL) |
| `pg_stat_get_live_tuples` | `set_pg_stat_get_live_tuples_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_mod_since_analyze` | `set_pg_stat_get_mod_since_analyze_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_numscans` | `set_pg_stat_get_numscans_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_tuples_deleted` | `set_pg_stat_get_tuples_deleted_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_tuples_fetched` | `set_pg_stat_get_tuples_fetched_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_tuples_hot_updated` | `set_pg_stat_get_tuples_hot_updated_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_tuples_inserted` | `set_pg_stat_get_tuples_inserted_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_tuples_newpage_updated` | `set_pg_stat_get_tuples_newpage_updated_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_tuples_returned` | `set_pg_stat_get_tuples_returned_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_tuples_updated` | `set_pg_stat_get_tuples_updated_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_vacuum_count` | `set_pg_stat_get_vacuum_count_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_xact_function_calls` | `set_pg_stat_get_xact_function_calls_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_xact_function_self_time` | `set_pg_stat_get_xact_function_self_time_resolver` | `Arc<dyn Fn(i64) -> Option<f64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_xact_function_total_time` | `set_pg_stat_get_xact_function_total_time_resolver` | `Arc<dyn Fn(i64) -> Option<f64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_xact_numscans` | `set_pg_stat_get_xact_numscans_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_xact_tuples_deleted` | `set_pg_stat_get_xact_tuples_deleted_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_xact_tuples_fetched` | `set_pg_stat_get_xact_tuples_fetched_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_xact_tuples_hot_updated` | `set_pg_stat_get_xact_tuples_hot_updated_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_xact_tuples_inserted` | `set_pg_stat_get_xact_tuples_inserted_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_xact_tuples_newpage_updated` | `set_pg_stat_get_xact_tuples_newpage_updated_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_xact_tuples_returned` | `set_pg_stat_get_xact_tuples_returned_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_stat_get_xact_tuples_updated` | `set_pg_stat_get_xact_tuples_updated_resolver` | `Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>` | `None` (SQL NULL) |
| `pg_table_is_visible` | `set_pg_table_is_visible_resolver` | `Arc<dyn Fn(i64) -> Option<bool> + Send + Sync>` | `true` |
| `pg_type_is_visible` | `set_pg_type_is_visible_resolver` | `Arc<dyn Fn(i64) -> Option<bool> + Send + Sync>` | `true` |
| `row_security_active` | `set_row_security_active_resolver` | `Arc<dyn Fn(i64) -> bool + Send + Sync>` | `false` |

## Set-returning functions

Each resolver returns a `Vec` of the row struct shown; every field is `pub` and
`Option`, and the struct derives `Default`, so `..Default::default()` leaves the
columns you do not set as NULL. A `timestamptz` field is an `i64` count of
microseconds since the Unix epoch, UTC.

### `pg_cursor`

Setter: `set_pg_cursor_resolver(Arc<dyn Fn() -> Vec<PgCursorRow> + Send + Sync>)`

```rust
pub struct PgCursorRow {
    pub name: Option<String>,
    pub statement: Option<String>,
    pub is_holdable: Option<bool>,
    pub is_binary: Option<bool>,
    pub is_scrollable: Option<bool>,
    pub creation_time: Option<i64>,
}
```

### `pg_get_backend_memory_contexts`

Setter: `set_pg_get_backend_memory_contexts_resolver(Arc<dyn Fn() -> Vec<PgGetBackendMemoryContextsRow> + Send + Sync>)`

```rust
pub struct PgGetBackendMemoryContextsRow {
    pub name: Option<String>,
    pub ident: Option<String>,
    pub parent: Option<String>,
    pub level: Option<i32>,
    pub total_bytes: Option<i64>,
    pub total_nblocks: Option<i64>,
    pub free_bytes: Option<i64>,
    pub free_chunks: Option<i64>,
    pub used_bytes: Option<i64>,
}
```

### `pg_get_publication_tables`

Setter: `set_pg_get_publication_tables_resolver(Arc<dyn Fn() -> Vec<PgGetPublicationTablesRow> + Send + Sync>)`

```rust
pub struct PgGetPublicationTablesRow {
    pub pubid: Option<i64>,
    pub relid: Option<i64>,
    pub attrs: Option<String>,
    pub qual: Option<String>,
}
```

### `pg_get_replication_slots`

Setter: `set_pg_get_replication_slots_resolver(Arc<dyn Fn() -> Vec<PgGetReplicationSlotsRow> + Send + Sync>)`

```rust
pub struct PgGetReplicationSlotsRow {
    pub slot_name: Option<String>,
    pub plugin: Option<String>,
    pub slot_type: Option<String>,
    pub datoid: Option<i64>,
    pub temporary: Option<bool>,
    pub active: Option<bool>,
    pub active_pid: Option<i32>,
    pub xmin: Option<i64>,
    pub catalog_xmin: Option<i64>,
    pub restart_lsn: Option<String>,
    pub confirmed_flush_lsn: Option<String>,
    pub wal_status: Option<String>,
    pub safe_wal_size: Option<i64>,
    pub two_phase: Option<bool>,
    pub inactive_since: Option<i64>,
    pub conflicting: Option<bool>,
    pub invalidation_reason: Option<String>,
    pub failover: Option<bool>,
    pub synced: Option<bool>,
}
```

### `pg_get_shmem_allocations`

Setter: `set_pg_get_shmem_allocations_resolver(Arc<dyn Fn() -> Vec<PgGetShmemAllocationsRow> + Send + Sync>)`

```rust
pub struct PgGetShmemAllocationsRow {
    pub name: Option<String>,
    pub off: Option<i64>,
    pub size: Option<i64>,
    pub allocated_size: Option<i64>,
}
```

### `pg_get_wait_events`

Setter: `set_pg_get_wait_events_resolver(Arc<dyn Fn() -> Vec<PgGetWaitEventsRow> + Send + Sync>)`

```rust
pub struct PgGetWaitEventsRow {
    pub r#type: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
}
```

### `pg_lock_status`

Setter: `set_pg_lock_status_resolver(Arc<dyn Fn() -> Vec<PgLockStatusRow> + Send + Sync>)`

```rust
pub struct PgLockStatusRow {
    pub locktype: Option<String>,
    pub database: Option<i64>,
    pub relation: Option<i64>,
    pub page: Option<i32>,
    pub tuple: Option<i32>,
    pub virtualxid: Option<String>,
    pub transactionid: Option<i64>,
    pub classid: Option<i64>,
    pub objid: Option<i64>,
    pub objsubid: Option<i32>,
    pub virtualtransaction: Option<String>,
    pub pid: Option<i32>,
    pub mode: Option<String>,
    pub granted: Option<bool>,
    pub fastpath: Option<bool>,
    pub waitstart: Option<i64>,
}
```

### `pg_mcv_list_items`

Setter: `set_pg_mcv_list_items_resolver(Arc<dyn Fn() -> Vec<PgMcvListItemsRow> + Send + Sync>)`

```rust
pub struct PgMcvListItemsRow {
    pub index: Option<i32>,
    pub values: Option<String>,
    pub nulls: Option<String>,
    pub frequency: Option<f64>,
    pub base_frequency: Option<f64>,
}
```

### `pg_prepared_statement`

Setter: `set_pg_prepared_statement_resolver(Arc<dyn Fn() -> Vec<PgPreparedStatementRow> + Send + Sync>)`

```rust
pub struct PgPreparedStatementRow {
    pub name: Option<String>,
    pub statement: Option<String>,
    pub prepare_time: Option<i64>,
    pub parameter_types: Option<String>,
    pub result_types: Option<String>,
    pub from_sql: Option<bool>,
    pub generic_plans: Option<i64>,
    pub custom_plans: Option<i64>,
}
```

### `pg_prepared_xact`

Setter: `set_pg_prepared_xact_resolver(Arc<dyn Fn() -> Vec<PgPreparedXactRow> + Send + Sync>)`

```rust
pub struct PgPreparedXactRow {
    pub transaction: Option<i64>,
    pub gid: Option<String>,
    pub prepared: Option<i64>,
    pub ownerid: Option<i64>,
    pub dbid: Option<i64>,
}
```

### `pg_show_all_file_settings`

Setter: `set_pg_show_all_file_settings_resolver(Arc<dyn Fn() -> Vec<PgShowAllFileSettingsRow> + Send + Sync>)`

```rust
pub struct PgShowAllFileSettingsRow {
    pub sourcefile: Option<String>,
    pub sourceline: Option<i32>,
    pub seqno: Option<i32>,
    pub name: Option<String>,
    pub setting: Option<String>,
    pub applied: Option<bool>,
    pub error: Option<String>,
}
```

### `pg_show_replication_origin_status`

Setter: `set_pg_show_replication_origin_status_resolver(Arc<dyn Fn() -> Vec<PgShowReplicationOriginStatusRow> + Send + Sync>)`

```rust
pub struct PgShowReplicationOriginStatusRow {
    pub local_id: Option<i64>,
    pub external_id: Option<String>,
    pub remote_lsn: Option<String>,
    pub local_lsn: Option<String>,
}
```

### `pg_stat_get_activity`

Setter: `set_pg_stat_get_activity_resolver(Arc<dyn Fn() -> Vec<PgStatGetActivityRow> + Send + Sync>)`

```rust
pub struct PgStatGetActivityRow {
    pub datid: Option<i64>,
    pub pid: Option<i32>,
    pub usesysid: Option<i64>,
    pub application_name: Option<String>,
    pub state: Option<String>,
    pub query: Option<String>,
    pub wait_event_type: Option<String>,
    pub wait_event: Option<String>,
    pub xact_start: Option<i64>,
    pub query_start: Option<i64>,
    pub backend_start: Option<i64>,
    pub state_change: Option<i64>,
    pub client_addr: Option<String>,
    pub client_hostname: Option<String>,
    pub client_port: Option<i32>,
    pub backend_xid: Option<i64>,
    pub backend_xmin: Option<i64>,
    pub backend_type: Option<String>,
    pub ssl: Option<bool>,
    pub sslversion: Option<String>,
    pub sslcipher: Option<String>,
    pub sslbits: Option<i32>,
    pub ssl_client_dn: Option<String>,
    pub ssl_client_serial: Option<String>,
    pub ssl_issuer_dn: Option<String>,
    pub gss_auth: Option<bool>,
    pub gss_princ: Option<String>,
    pub gss_enc: Option<bool>,
    pub gss_delegation: Option<bool>,
    pub leader_pid: Option<i32>,
    pub query_id: Option<i64>,
}
```

### `pg_stat_get_archiver`

Setter: `set_pg_stat_get_archiver_resolver(Arc<dyn Fn() -> Vec<PgStatGetArchiverRow> + Send + Sync>)`

```rust
pub struct PgStatGetArchiverRow {
    pub archived_count: Option<i64>,
    pub last_archived_wal: Option<String>,
    pub last_archived_time: Option<i64>,
    pub failed_count: Option<i64>,
    pub last_failed_wal: Option<String>,
    pub last_failed_time: Option<i64>,
    pub stats_reset: Option<i64>,
}
```

### `pg_stat_get_io`

Setter: `set_pg_stat_get_io_resolver(Arc<dyn Fn() -> Vec<PgStatGetIoRow> + Send + Sync>)`

```rust
pub struct PgStatGetIoRow {
    pub backend_type: Option<String>,
    pub object: Option<String>,
    pub context: Option<String>,
    pub reads: Option<i64>,
    pub read_time: Option<f64>,
    pub writes: Option<i64>,
    pub write_time: Option<f64>,
    pub writebacks: Option<i64>,
    pub writeback_time: Option<f64>,
    pub extends: Option<i64>,
    pub extend_time: Option<f64>,
    pub op_bytes: Option<i64>,
    pub hits: Option<i64>,
    pub evictions: Option<i64>,
    pub reuses: Option<i64>,
    pub fsyncs: Option<i64>,
    pub fsync_time: Option<f64>,
    pub stats_reset: Option<i64>,
}
```

### `pg_stat_get_progress_info`

Setter: `set_pg_stat_get_progress_info_resolver(Arc<dyn Fn() -> Vec<PgStatGetProgressInfoRow> + Send + Sync>)`

```rust
pub struct PgStatGetProgressInfoRow {
    pub pid: Option<i32>,
    pub datid: Option<i64>,
    pub relid: Option<i64>,
    pub param1: Option<i64>,
    pub param2: Option<i64>,
    pub param3: Option<i64>,
    pub param4: Option<i64>,
    pub param5: Option<i64>,
    pub param6: Option<i64>,
    pub param7: Option<i64>,
    pub param8: Option<i64>,
    pub param9: Option<i64>,
    pub param10: Option<i64>,
    pub param11: Option<i64>,
    pub param12: Option<i64>,
    pub param13: Option<i64>,
    pub param14: Option<i64>,
    pub param15: Option<i64>,
    pub param16: Option<i64>,
    pub param17: Option<i64>,
    pub param18: Option<i64>,
    pub param19: Option<i64>,
    pub param20: Option<i64>,
}
```

### `pg_stat_get_recovery_prefetch`

Setter: `set_pg_stat_get_recovery_prefetch_resolver(Arc<dyn Fn() -> Vec<PgStatGetRecoveryPrefetchRow> + Send + Sync>)`

```rust
pub struct PgStatGetRecoveryPrefetchRow {
    pub stats_reset: Option<i64>,
    pub prefetch: Option<i64>,
    pub hit: Option<i64>,
    pub skip_init: Option<i64>,
    pub skip_new: Option<i64>,
    pub skip_fpw: Option<i64>,
    pub skip_rep: Option<i64>,
    pub wal_distance: Option<i32>,
    pub block_distance: Option<i32>,
    pub io_depth: Option<i32>,
}
```

### `pg_stat_get_replication_slot`

Setter: `set_pg_stat_get_replication_slot_resolver(Arc<dyn Fn() -> Vec<PgStatGetReplicationSlotRow> + Send + Sync>)`

```rust
pub struct PgStatGetReplicationSlotRow {
    pub slot_name: Option<String>,
    pub spill_txns: Option<i64>,
    pub spill_count: Option<i64>,
    pub spill_bytes: Option<i64>,
    pub stream_txns: Option<i64>,
    pub stream_count: Option<i64>,
    pub stream_bytes: Option<i64>,
    pub total_txns: Option<i64>,
    pub total_bytes: Option<i64>,
    pub stats_reset: Option<i64>,
}
```

### `pg_stat_get_slru`

Setter: `set_pg_stat_get_slru_resolver(Arc<dyn Fn() -> Vec<PgStatGetSlruRow> + Send + Sync>)`

```rust
pub struct PgStatGetSlruRow {
    pub name: Option<String>,
    pub blks_zeroed: Option<i64>,
    pub blks_hit: Option<i64>,
    pub blks_read: Option<i64>,
    pub blks_written: Option<i64>,
    pub blks_exists: Option<i64>,
    pub flushes: Option<i64>,
    pub truncates: Option<i64>,
    pub stats_reset: Option<i64>,
}
```

### `pg_stat_get_subscription`

Setter: `set_pg_stat_get_subscription_resolver(Arc<dyn Fn() -> Vec<PgStatGetSubscriptionRow> + Send + Sync>)`

```rust
pub struct PgStatGetSubscriptionRow {
    pub subid: Option<i64>,
    pub relid: Option<i64>,
    pub pid: Option<i32>,
    pub leader_pid: Option<i32>,
    pub received_lsn: Option<String>,
    pub last_msg_send_time: Option<i64>,
    pub last_msg_receipt_time: Option<i64>,
    pub latest_end_lsn: Option<String>,
    pub latest_end_time: Option<i64>,
    pub worker_type: Option<String>,
}
```

### `pg_stat_get_subscription_stats`

Setter: `set_pg_stat_get_subscription_stats_resolver(Arc<dyn Fn() -> Vec<PgStatGetSubscriptionStatsRow> + Send + Sync>)`

```rust
pub struct PgStatGetSubscriptionStatsRow {
    pub subid: Option<i64>,
    pub apply_error_count: Option<i64>,
    pub sync_error_count: Option<i64>,
    pub stats_reset: Option<i64>,
}
```

### `pg_stat_get_wal`

Setter: `set_pg_stat_get_wal_resolver(Arc<dyn Fn() -> Vec<PgStatGetWalRow> + Send + Sync>)`

```rust
pub struct PgStatGetWalRow {
    pub wal_records: Option<i64>,
    pub wal_fpi: Option<i64>,
    pub wal_bytes: Option<String>,
    pub wal_buffers_full: Option<i64>,
    pub wal_write: Option<i64>,
    pub wal_sync: Option<i64>,
    pub wal_write_time: Option<f64>,
    pub wal_sync_time: Option<f64>,
    pub stats_reset: Option<i64>,
}
```

### `pg_stat_get_wal_receiver`

Setter: `set_pg_stat_get_wal_receiver_resolver(Arc<dyn Fn() -> Vec<PgStatGetWalReceiverRow> + Send + Sync>)`

```rust
pub struct PgStatGetWalReceiverRow {
    pub pid: Option<i32>,
    pub status: Option<String>,
    pub receive_start_lsn: Option<String>,
    pub receive_start_tli: Option<i32>,
    pub written_lsn: Option<String>,
    pub flushed_lsn: Option<String>,
    pub received_tli: Option<i32>,
    pub last_msg_send_time: Option<i64>,
    pub last_msg_receipt_time: Option<i64>,
    pub latest_end_lsn: Option<String>,
    pub latest_end_time: Option<i64>,
    pub slot_name: Option<String>,
    pub sender_host: Option<String>,
    pub sender_port: Option<i32>,
    pub conninfo: Option<String>,
}
```

### `pg_stat_get_wal_senders`

Setter: `set_pg_stat_get_wal_senders_resolver(Arc<dyn Fn() -> Vec<PgStatGetWalSendersRow> + Send + Sync>)`

```rust
pub struct PgStatGetWalSendersRow {
    pub pid: Option<i32>,
    pub state: Option<String>,
    pub sent_lsn: Option<String>,
    pub write_lsn: Option<String>,
    pub flush_lsn: Option<String>,
    pub replay_lsn: Option<String>,
    pub write_lag: Option<String>,
    pub flush_lag: Option<String>,
    pub replay_lag: Option<String>,
    pub sync_priority: Option<i32>,
    pub sync_state: Option<String>,
    pub reply_time: Option<i64>,
}
```
