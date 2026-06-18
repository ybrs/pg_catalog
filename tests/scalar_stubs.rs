//! Tests for the small scalar compatibility stubs used by information_schema
//! views: `pg_my_temp_schema`, `getdatabaseencoding`, `pg_relation_is_updatable`,
//! and `information_schema._pg_char_max_length`.

use arrow::array::{Array, Int32Array, StringArray};
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

#[tokio::test]
async fn test_pg_my_temp_schema() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let b = ctx.sql("SELECT pg_my_temp_schema()").await?.collect().await?;
    let a = b[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int32Array>()
        .unwrap();
    assert_eq!(a.value(0), 0);
    Ok(())
}

#[tokio::test]
async fn test_getdatabaseencoding() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let b = ctx
        .sql("SELECT getdatabaseencoding()")
        .await?
        .collect()
        .await?;
    let a = b[0]
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(a.value(0), "UTF8");
    Ok(())
}

#[tokio::test]
async fn test_pg_relation_is_updatable() -> DFResult<()> {
    let ctx = base_ctx().await?;
    // Two-arg form, any arg types; returns 0 (not updatable) per row.
    let b = ctx
        .sql("SELECT pg_relation_is_updatable(oid, false) FROM pg_catalog.pg_class LIMIT 3")
        .await?
        .collect()
        .await?;
    let a = b[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int32Array>()
        .unwrap();
    assert!(a.len() > 0 && (0..a.len()).all(|i| a.value(i) == 0));
    Ok(())
}

#[tokio::test]
async fn test_pg_char_max_length_is_null() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let b = ctx
        .sql("SELECT information_schema._pg_char_max_length(23, -1)")
        .await?
        .collect()
        .await?;
    let a = b[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int32Array>()
        .unwrap();
    assert!(a.is_null(0), "expected NULL char max length");
    Ok(())
}
