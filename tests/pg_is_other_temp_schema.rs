//! Tests for the `pg_is_other_temp_schema()` compatibility stub.
//!
//! We emulate a single session with no other backends, so it always returns
//! `false`. It must be callable with an OID argument given as an int or a name,
//! bare or `pg_catalog`-qualified, and evaluate per-row over a column.

use arrow::array::{Array, BooleanArray};
use datafusion::error::Result as DFResult;

mod common;
use common::base_ctx;

/// Run `sql` and collect a single Boolean column.
async fn bool_column(
    ctx: &datafusion::execution::context::SessionContext,
    sql: &str,
) -> DFResult<Vec<Option<bool>>> {
    let batches = ctx.sql(sql).await?.collect().await?;
    let mut out = Vec::new();
    for b in &batches {
        let arr = b
            .column(0)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .expect("expected a Boolean column");
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
async fn test_pg_is_other_temp_schema_scalar() -> DFResult<()> {
    let ctx = base_ctx().await?;
    // OID as int, bare and qualified.
    assert_eq!(
        bool_column(&ctx, "SELECT pg_is_other_temp_schema(11)").await?,
        vec![Some(false)]
    );
    assert_eq!(
        bool_column(&ctx, "SELECT pg_catalog.pg_is_other_temp_schema(11)").await?,
        vec![Some(false)]
    );
    // OID as name.
    assert_eq!(
        bool_column(&ctx, "SELECT pg_is_other_temp_schema('public')").await?,
        vec![Some(false)]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_is_other_temp_schema_over_a_column() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let rows = bool_column(
        &ctx,
        "SELECT pg_is_other_temp_schema(oid) FROM pg_catalog.pg_namespace",
    )
    .await?;
    assert!(!rows.is_empty());
    assert!(
        rows.iter().all(|r| *r == Some(false)),
        "pg_is_other_temp_schema must be false for every row, got {rows:?}"
    );
    Ok(())
}
