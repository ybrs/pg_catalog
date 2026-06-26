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
