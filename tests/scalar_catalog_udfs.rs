//! Runtime-robustness tests for catalog scalar UDFs that run a sub-query.
//!
//! `oid(text)` (and `pg_get_userbyid`) resolve values by running a catalog SQL
//! query from a synchronous UDF body. They must work regardless of the caller's
//! tokio runtime flavor - in particular on the current-thread runtime that
//! `#[tokio::test(flavor = "multi_thread")]` uses by default (where `tokio::task::block_in_place` would
//! panic). These tests deliberately use the default `#[tokio::test(flavor = "multi_thread")]` runtime.

use arrow::array::{Array, Int64Array};
use datafusion::error::Result as DFResult;
use datafusion_pg_catalog::get_base_session_context;

async fn base_ctx() -> DFResult<datafusion::execution::context::SessionContext> {
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
        None,
    )
    .await?;
    Ok(ctx)
}

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

#[tokio::test(flavor = "multi_thread")]
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

#[tokio::test(flavor = "multi_thread")]
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
