# Runtime functions: integration-supplied statistics and live state

Many `pg_catalog` views call server-runtime functions that a static catalog cannot
compute on its own - per-object statistics (`pg_stat_get_numscans(oid)`, ...), live
session and lock state (`pg_stat_get_activity(pid)`, `pg_lock_status()`, ...), WAL and
replication state, and so on. This library registers every one of those functions so the
views that call them are real, queryable views, and lets your application supply the
values through typed callbacks called **resolvers**.

With no resolver installed a function returns its empty default - SQL `NULL` for a scalar
function, no rows for a set-returning one - so every view is correct (just empty) out of
the box. Install a resolver and the corresponding view starts reporting your data.

This document explains how to supply them, with worked examples. The exhaustive,
copy-pasteable reference - every setter's exact signature and every row struct's full field
list, in Rust - is in [runtime-functions-reference.md](runtime-functions-reference.md).

## The model

- **Every function has its own explicit, typed setter.** There is no
  `register_resolver("name", ...)` string-keyed API. To supply
  `pg_stat_get_numscans(oid)` you call `set_pg_stat_get_numscans_resolver(...)`; to supply
  `pg_lock_status()` you call `set_pg_lock_status_resolver(...)`. Each setter's argument
  type is checked at compile time.
- **The name is mechanical.** For a PostgreSQL function `F`, the setter is
  `set_F_resolver` and the clearer is `clear_F_resolver`. So once you know the function
  name (from the reference above) you know the API.
- **Resolvers are process-global and read at call time.** Install them once, after you
  build the session context and before you serve queries. A resolver is consulted on
  every call to its function, so updating live data is just a matter of the closure you
  install reading fresh state each time it runs.
- **Defaults are safe.** A function with no resolver installed returns NULL (scalar) or no
  rows (set-returning). `clear_F_resolver()` returns a function to that default.

Everything below is re-exported from the crate root, e.g.
`use datafusion_pg_catalog::set_pg_stat_get_numscans_resolver;`.

## Scalar functions

A scalar runtime function takes an object identifier (or no argument) and returns one
value. Its resolver is an `Arc<dyn Fn(args) -> Option<T> + Send + Sync>`; returning `None`
reports SQL `NULL`.

```rust
use std::sync::Arc;
use datafusion_pg_catalog::set_pg_stat_get_numscans_resolver;

// Report a sequential-scan count per relation OID.
set_pg_stat_get_numscans_resolver(Arc::new(|relation_oid: i64| {
    Some(my_stats.seq_scans_for(relation_oid))   // or None for "unknown"
}));
```

The argument is the OID the view passes in (widened to `i64`). Functions that take no
argument (e.g. `pg_stat_get_buf_alloc()`) take an `Arc<dyn Fn() -> Option<T>>`. Return
types follow the PostgreSQL signature: `int8 -> Option<i64>`, `int4 -> Option<i32>`,
`float8 -> Option<f64>`, `bool -> Option<bool>`, and `timestamptz -> Option<i64>` (a
microsecond UTC timestamp).

The `pg_stat_get_*` accessors (around 80 of them) all follow this shape. They group into families -
per-table/index accessors keyed by relation OID (`pg_stat_get_numscans`,
`pg_stat_get_live_tuples`, ...), per-database accessors keyed by database OID
(`pg_stat_get_db_xact_commit`, ...), the transaction-scoped `pg_stat_get_xact_*` set, and
the no-argument `pg_stat_get_bgwriter_*` / `pg_stat_get_checkpointer_*` / buffer-allocation
accessors. Each one's exact setter and resolver type is listed in
[runtime-functions-reference.md](runtime-functions-reference.md).

### Scalar functions with non-`pg_stat_get_*` shapes

| Function | Setter | Resolver type | Default |
| --- | --- | --- | --- |
| `pg_sequence_last_value(oid)` | `set_pg_sequence_last_value_resolver` | `Arc<dyn Fn(i64) -> Option<i64>>` | NULL |
| `row_security_active(oid)` | `set_row_security_active_resolver` | `Arc<dyn Fn(i64) -> bool>` | `false` |
| `pg_table_is_visible(oid)` | `set_pg_table_is_visible_resolver` | `Arc<dyn Fn(i64) -> Option<bool>>` | `true` |
| `pg_function_is_visible(oid)` | `set_pg_function_is_visible_resolver` | `Arc<dyn Fn(i64) -> Option<bool>>` | `true` |
| `pg_type_is_visible(oid)` | `set_pg_type_is_visible_resolver` | `Arc<dyn Fn(i64) -> Option<bool>>` | `true` |
| `pg_indexam_progress_phasename(oid, int8)` | `set_pg_indexam_progress_phasename_resolver` | `Arc<dyn Fn(i64, i64) -> Option<String>>` | NULL |
| `pg_get_statisticsobjdef_expressions(oid)` | `set_pg_get_statisticsobjdef_expressions_resolver` | `Arc<dyn Fn(i64) -> Option<Vec<String>>>` | NULL |

The `*_is_visible` predicates default to `true` (a static catalog has no search-path
notion, so every object is treated as visible); install a resolver to apply search-path
logic.

## Set-returning functions

A set-returning function produces a row set. Its resolver returns a `Vec` of a generated,
typed row struct - one struct per function, with one `Option` field per output column
(named exactly as PostgreSQL names it). Return the rows you want; an empty `Vec` (the
default) means no rows.

```rust
use std::sync::Arc;
use datafusion_pg_catalog::{set_pg_lock_status_resolver, PgLockStatusRow};

set_pg_lock_status_resolver(Arc::new(|| {
    vec![PgLockStatusRow {
        locktype: Some("relation".into()),
        database: Some(1),
        relation: Some(1259),
        pid: Some(42),
        mode: Some("AccessShareLock".into()),
        granted: Some(true),
        ..Default::default()           // every other column defaults to NULL
    }]
}));
```

The row struct for a function `F` is named `<F>Row` in upper-camel-case - `pg_lock_status`
-> `PgLockStatusRow`, `pg_stat_get_activity` -> `PgStatGetActivityRow`. Field types mirror
the column types: `text -> Option<String>`, `bool -> Option<bool>`, `int4 -> Option<i32>`,
`int8 -> Option<i64>`, `float8 -> Option<f64>`, `timestamptz -> Option<i64>` (microsecond
UTC). PostgreSQL array / LSN / numeric columns are represented as `Option<String>`.

The set-returning functions and the views they back:

| Function | Row struct | Backs view(s) |
| --- | --- | --- |
| `pg_stat_get_activity` | `PgStatGetActivityRow` | `pg_stat_activity`, `pg_stat_replication`, `pg_stat_ssl`, `pg_stat_gssapi` |
| `pg_lock_status` | `PgLockStatusRow` | `pg_locks` |
| `pg_cursor` | `PgCursorRow` | `pg_cursors` |
| `pg_prepared_statement` | `PgPreparedStatementRow` | `pg_prepared_statements` |
| `pg_prepared_xact` | `PgPreparedXactRow` | `pg_prepared_xacts` |
| `pg_stat_get_progress_info` | `PgStatGetProgressInfoRow` | the `pg_stat_progress_*` views |
| `pg_stat_get_io` | `PgStatGetIoRow` | `pg_stat_io` |
| `pg_stat_get_slru` | `PgStatGetSlruRow` | `pg_stat_slru` |
| `pg_stat_get_subscription` | `PgStatGetSubscriptionRow` | `pg_stat_subscription` |
| `pg_stat_get_wal_senders` | `PgStatGetWalSendersRow` | `pg_stat_replication` |
| `pg_stat_get_recovery_prefetch` | `PgStatGetRecoveryPrefetchRow` | `pg_stat_recovery_prefetch` |
| `pg_stat_get_archiver` | `PgStatGetArchiverRow` | `pg_stat_archiver` |
| `pg_stat_get_wal` | `PgStatGetWalRow` | `pg_stat_wal` |
| `pg_stat_get_wal_receiver` | `PgStatGetWalReceiverRow` | `pg_stat_wal_receiver` |
| `pg_stat_get_replication_slot` | `PgStatGetReplicationSlotRow` | `pg_stat_replication_slots` |
| `pg_stat_get_subscription_stats` | `PgStatGetSubscriptionStatsRow` | `pg_stat_subscription_stats` |
| `pg_get_replication_slots` | `PgGetReplicationSlotsRow` | `pg_replication_slots` |
| `pg_show_replication_origin_status` | `PgShowReplicationOriginStatusRow` | `pg_replication_origin_status` |
| `pg_get_backend_memory_contexts` | `PgGetBackendMemoryContextsRow` | `pg_backend_memory_contexts` |
| `pg_get_shmem_allocations` | `PgGetShmemAllocationsRow` | `pg_shmem_allocations` |
| `pg_show_all_file_settings` | `PgShowAllFileSettingsRow` | `pg_file_settings` |
| `pg_get_wait_events` | `PgGetWaitEventsRow` | `pg_wait_events` |
| `pg_get_publication_tables` | `PgGetPublicationTablesRow` | `pg_publication_tables` |
| `pg_mcv_list_items` | `PgMcvListItemsRow` | `pg_stats_ext` |

The full field list of every row struct - field name and Rust type, ready to copy - is in
[runtime-functions-reference.md](runtime-functions-reference.md). For example,
`PgLockStatusRow` is:

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

## Session identity

`current_user`, `session_user`, and `current_role` report the current session user. Unlike
the resolvers above (which are process-global), the session user is updated per query from
the connecting client, so view bodies referencing `CURRENT_USER` resolve to the querying
connection's user. If you embed the catalog without the bundled pgwire server, set it
yourself:

```rust
use datafusion_pg_catalog::set_session_user;
set_session_user("alice");
```

## Caveats

- Resolvers are process-global. They are shared across all sessions of the context; they
  do not receive any per-connection context beyond the arguments the SQL passes (an OID, a
  PID, ...). The one exception is the session user above.
- A resolver runs synchronously inside query execution. Keep it cheap, and do not block on
  it; read from state you maintain elsewhere.
- Install resolvers once at startup. Because a live view captures the function at the time
  it is created, re-installing a resolver is supported (the function reads the current slot
  on each call), but there is no need to re-install per query.
