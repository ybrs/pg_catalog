//! Runtime-robustness tests for catalog scalar UDFs that run a sub-query.
//!
//! `oid(text)` (and `pg_get_userbyid`) resolve values by running a catalog SQL
//! query from a synchronous UDF body. `run_catalog_query` has two branches and
//! these tests exercise both: on a current-thread runtime (the default
//! `#[tokio::test]`) it spawns onto a fallback multi-thread runtime and blocks on
//! the result, while on a multi-thread runtime (`#[tokio::test(flavor =
//! "multi_thread")]`, the flavor the production server uses) it takes the
//! `block_in_place` + `spawn` path. Either way the nested catalog query must
//! resolve without panicking or deadlocking.

use arrow::array::{Array, Int64Array};
use datafusion::error::Result as DFResult;

mod common;
use common::base_ctx;

/// Collect a single Int64 column into `Vec<Option<i64>>`.
async fn int64_column(
    ctx: &datafusion::execution::context::SessionContext,
    sql: &str,
) -> DFResult<Vec<Option<i64>>> {
    let batches = ctx.sql(sql).await?.collect().await?;
    let mut out = Vec::new();
    for b in &batches {
        let arr = b
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("expected an Int64 column");
        for i in 0..arr.len() {
            out.push(if arr.is_null(i) {
                None
            } else {
                Some(arr.value(i))
            });
        }
    }
    Ok(out)
}

#[tokio::test]
async fn test_oid_udf_scalar_resolves_on_current_thread_runtime() -> DFResult<()> {
    let ctx = base_ctx().await?;

    // The scalar branch of oid(text) runs a pg_class sub-query; it must resolve
    // pg_class's own OID and not panic on the current-thread runtime.
    let resolved = int64_column(&ctx, "SELECT oid('pg_class')").await?;
    assert_eq!(resolved.len(), 1);
    assert!(
        matches!(resolved[0], Some(v) if v > 0),
        "oid('pg_class') should resolve to a positive OID, got {:?}",
        resolved[0]
    );

    // Cross-check it equals the row's own oid in pg_class.
    let direct = int64_column(
        &ctx,
        "SELECT oid::bigint FROM pg_catalog.pg_class WHERE relname = 'pg_class'",
    )
    .await?;
    assert_eq!(resolved[0], direct[0]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_oid_udf_unknown_relation_is_null() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let resolved = int64_column(&ctx, "SELECT oid('no_such_relation_xyz')").await?;
    assert_eq!(resolved, vec![None]);
    Ok(())
}

#[tokio::test]
async fn test_oid_udf_array_branch_on_current_thread_runtime() -> DFResult<()> {
    let ctx = base_ctx().await?;

    // The array branch resolves oid(text) per row; exercise it over several
    // relation names (including a non-existent one) on the current-thread runtime.
    let resolved = int64_column(
        &ctx,
        "SELECT oid(relname) \
         FROM (VALUES ('pg_class'), ('pg_type'), ('no_such_relation_xyz')) AS v(relname)",
    )
    .await?;
    assert_eq!(resolved.len(), 3);
    assert!(
        matches!(resolved[0], Some(v) if v > 0),
        "pg_class should resolve"
    );
    assert!(
        matches!(resolved[1], Some(v) if v > 0),
        "pg_type should resolve"
    );
    assert_eq!(resolved[2], None, "unknown relation should be NULL");
    Ok(())
}

/// `pg_sequence_last_value` is NULL with no resolver installed and reports the
/// installed resolver's value otherwise.
#[tokio::test]
async fn test_pg_sequence_last_value_default_null_then_resolver() -> DFResult<()> {
    use datafusion_pg_catalog::{
        clear_pg_sequence_last_value_resolver, set_pg_sequence_last_value_resolver,
    };
    use std::sync::Arc;

    let ctx = base_ctx().await?;

    clear_pg_sequence_last_value_resolver();
    assert_eq!(
        int64_column(&ctx, "SELECT pg_sequence_last_value(42)").await?,
        vec![None],
        "NULL when no resolver is installed"
    );

    set_pg_sequence_last_value_resolver(Arc::new(|oid: i64| Some(oid * 10)));
    assert_eq!(
        int64_column(&ctx, "SELECT pg_sequence_last_value(42)").await?,
        vec![Some(420)],
        "resolver value is reported"
    );
    clear_pg_sequence_last_value_resolver();
    Ok(())
}

/// `row_security_active` is false with no resolver installed and reports the
/// installed resolver's answer otherwise.
#[tokio::test]
async fn test_row_security_active_default_false_then_resolver() -> DFResult<()> {
    use arrow::array::BooleanArray;
    use datafusion_pg_catalog::{
        clear_row_security_active_resolver, set_row_security_active_resolver,
    };
    use std::sync::Arc;

    let ctx = base_ctx().await?;

    clear_row_security_active_resolver();
    let batches = ctx
        .sql("SELECT row_security_active(7)")
        .await?
        .collect()
        .await?;
    let arr = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<BooleanArray>()
        .expect("boolean column");
    assert!(!arr.value(0), "false when no resolver is installed");

    set_row_security_active_resolver(Arc::new(|_oid: i64| true));
    let batches = ctx
        .sql("SELECT row_security_active(7)")
        .await?
        .collect()
        .await?;
    let arr = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<BooleanArray>()
        .expect("boolean column");
    assert!(arr.value(0), "resolver answer is reported");
    clear_row_security_active_resolver();
    Ok(())
}

/// A macro-generated `pg_stat_get_*` resolver: NULL by default, the installed
/// callback's value otherwise. Exercises the generated explicit setter and the
/// shared `DynScalarUdf` plumbing in one go.
#[tokio::test]
async fn test_generated_stat_resolver_default_null_then_resolver() -> DFResult<()> {
    use datafusion_pg_catalog::{
        clear_pg_stat_get_numscans_resolver, set_pg_stat_get_numscans_resolver,
    };
    use std::sync::Arc;

    let ctx = base_ctx().await?;

    clear_pg_stat_get_numscans_resolver();
    assert_eq!(
        int64_column(&ctx, "SELECT pg_stat_get_numscans(1)").await?,
        vec![None],
        "NULL when no resolver is installed"
    );

    set_pg_stat_get_numscans_resolver(Arc::new(|oid: i64| Some(oid + 100)));
    assert_eq!(
        int64_column(&ctx, "SELECT pg_stat_get_numscans(5)").await?,
        vec![Some(105)],
        "resolver value is reported"
    );
    clear_pg_stat_get_numscans_resolver();
    Ok(())
}

/// `current_user` / `session_user` read the session's own `ClientOpts` at call time,
/// so they reflect `set_session_user` on that session (this is what lets a view body's
/// `CURRENT_USER`, planned at startup, resolve to the querying connection's user).
///
/// Checked here against a context built from the real catalog, which
/// `tests/session_identity.rs` does not do - it isolates the mechanism on a bare
/// context. Both matter: this one would catch the catalog's own build overwriting or
/// shadowing the identity functions.
#[tokio::test]
async fn test_session_user_reflects_client_opts() -> DFResult<()> {
    use arrow::array::StringArray;
    use datafusion_pg_catalog::session::set_session_user;

    let ctx = base_ctx().await?;
    let read = |label: &'static str| {
        let ctx = ctx.clone();
        async move {
            let batches = ctx
                .sql("SELECT current_user, session_user")
                .await?
                .collect()
                .await?;
            let col = |i: usize| {
                batches[0]
                    .column(i)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap_or_else(|| panic!("{label} col {i} text"))
                    .value(0)
                    .to_string()
            };
            Ok::<_, datafusion::error::DataFusionError>((col(0), col(1)))
        }
    };

    set_session_user(&ctx, "alice")?;
    assert_eq!(
        read("alice").await?,
        ("alice".to_string(), "alice".to_string())
    );

    set_session_user(&ctx, "bob")?;
    assert_eq!(read("bob").await?, ("bob".to_string(), "bob".to_string()));
    Ok(())
}

/// Read a single Boolean column into `Vec<Option<bool>>`.
async fn bool_column(
    ctx: &datafusion::execution::context::SessionContext,
    sql: &str,
) -> DFResult<Vec<Option<bool>>> {
    use arrow::array::BooleanArray;
    let batches = ctx.sql(sql).await?.collect().await?;
    let mut out = Vec::new();
    for b in &batches {
        let arr = b
            .column(0)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .expect("expected a Boolean column");
        for i in 0..arr.len() {
            out.push((!arr.is_null(i)).then(|| arr.value(i)));
        }
    }
    Ok(out)
}

/// A visibility predicate defaults to `true` (a static catalog treats every object as
/// visible) and reports the installed resolver's answer otherwise.
#[tokio::test]
async fn test_visibility_predicate_default_true_then_resolver() -> DFResult<()> {
    use datafusion_pg_catalog::{
        clear_pg_table_is_visible_resolver, set_pg_table_is_visible_resolver,
    };
    use std::sync::Arc;

    let ctx = base_ctx().await?;

    clear_pg_table_is_visible_resolver();
    assert_eq!(
        bool_column(&ctx, "SELECT pg_table_is_visible(1259)").await?,
        vec![Some(true)],
        "visible by default when no resolver is installed"
    );

    set_pg_table_is_visible_resolver(Arc::new(|oid: i64| Some(oid == 1259)));
    assert_eq!(
        bool_column(
            &ctx,
            "SELECT pg_table_is_visible(oid) \
             FROM (VALUES (1259), (42)) AS v(oid) ORDER BY oid DESC"
        )
        .await?,
        vec![Some(true), Some(false)],
        "resolver answer is reported per row"
    );
    clear_pg_table_is_visible_resolver();
    Ok(())
}

/// `pg_indexam_progress_phasename(oid, int8)` is NULL with no resolver installed and
/// reports the installed resolver's phase name otherwise. Exercises the two-argument
/// hand-written scalar built on the shared `DynScalarUdf`.
#[tokio::test]
async fn test_pg_indexam_progress_phasename_default_null_then_resolver() -> DFResult<()> {
    use arrow::array::StringArray;
    use datafusion_pg_catalog::{
        clear_pg_indexam_progress_phasename_resolver, set_pg_indexam_progress_phasename_resolver,
    };
    use std::sync::Arc;

    let ctx = base_ctx().await?;

    let phase = |sql: &str| {
        let ctx = ctx.clone();
        let sql = sql.to_string();
        async move {
            let batches = ctx.sql(&sql).await?.collect().await?;
            let arr = batches[0]
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("text column")
                .clone();
            Ok::<_, datafusion::error::DataFusionError>(
                (!arr.is_null(0)).then(|| arr.value(0).to_string()),
            )
        }
    };

    clear_pg_indexam_progress_phasename_resolver();
    assert_eq!(
        phase("SELECT pg_indexam_progress_phasename(403, 2)").await?,
        None,
        "NULL when no resolver is installed"
    );

    set_pg_indexam_progress_phasename_resolver(Arc::new(|_am: i64, phase: i64| {
        Some(format!("phase {phase}"))
    }));
    assert_eq!(
        phase("SELECT pg_indexam_progress_phasename(403, 2)").await?,
        Some("phase 2".to_string()),
        "resolver phase name is reported"
    );
    clear_pg_indexam_progress_phasename_resolver();
    Ok(())
}

/// `pg_get_statisticsobjdef_expressions(oid)` is NULL with no resolver installed and
/// returns the installed resolver's text array otherwise. Exercises the array-returning
/// hand-written scalar.
#[tokio::test]
async fn test_pg_get_statisticsobjdef_expressions_default_null_then_resolver() -> DFResult<()> {
    use arrow::array::{ListArray, StringArray};
    use datafusion_pg_catalog::{
        clear_pg_get_statisticsobjdef_expressions_resolver,
        set_pg_get_statisticsobjdef_expressions_resolver,
    };
    use std::sync::Arc;

    let ctx = base_ctx().await?;

    clear_pg_get_statisticsobjdef_expressions_resolver();
    let batches = ctx
        .sql("SELECT pg_get_statisticsobjdef_expressions(7)")
        .await?
        .collect()
        .await?;
    let list = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<ListArray>()
        .expect("list column");
    assert!(list.is_null(0), "NULL when no resolver is installed");

    set_pg_get_statisticsobjdef_expressions_resolver(Arc::new(|_oid: i64| {
        Some(vec!["(a + b)".to_string(), "lower(c)".to_string()])
    }));
    let batches = ctx
        .sql("SELECT pg_get_statisticsobjdef_expressions(7)")
        .await?
        .collect()
        .await?;
    let list = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<ListArray>()
        .expect("list column");
    let items = list.value(0);
    let items = items
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("text elements");
    assert_eq!(items.len(), 2);
    assert_eq!(items.value(0), "(a + b)");
    assert_eq!(items.value(1), "lower(c)");
    clear_pg_get_statisticsobjdef_expressions_resolver();
    Ok(())
}

/// A record-returning runtime function (`pg_stat_get_archiver`): no rows with no
/// resolver installed, the resolver's single record otherwise. Exercises the typed
/// generated row struct and the shared `DynTableUdf` table-function plumbing.
#[tokio::test(flavor = "multi_thread")]
async fn test_record_returning_function_default_empty_then_resolver() -> DFResult<()> {
    use datafusion_pg_catalog::{
        clear_pg_stat_get_archiver_resolver, set_pg_stat_get_archiver_resolver,
        PgStatGetArchiverRow,
    };
    use std::sync::Arc;

    let ctx = base_ctx().await?;

    clear_pg_stat_get_archiver_resolver();
    let count = int64_column(
        &ctx,
        "SELECT count(*) FROM pg_stat_get_archiver() \
         s(archived_count, last_archived_wal, last_archived_time, failed_count, \
           last_failed_wal, last_failed_time, stats_reset)",
    )
    .await?;
    assert_eq!(
        count,
        vec![Some(0)],
        "no rows when no resolver is installed"
    );

    set_pg_stat_get_archiver_resolver(Arc::new(|| {
        vec![PgStatGetArchiverRow {
            archived_count: Some(7),
            last_archived_wal: Some("000000010000000000000003".to_string()),
            ..Default::default()
        }]
    }));
    let archived = int64_column(
        &ctx,
        "SELECT archived_count FROM pg_stat_get_archiver() \
         s(archived_count, last_archived_wal, last_archived_time, failed_count, \
           last_failed_wal, last_failed_time, stats_reset)",
    )
    .await?;
    assert_eq!(archived, vec![Some(7)], "resolver record is reported");
    clear_pg_stat_get_archiver_resolver();
    Ok(())
}
