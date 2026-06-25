//! Tests for the `pg_has_role()` compatibility stub.
//!
//! It always returns `true` (the emulated superuser is a member of every role),
//! but it must be callable in every form the information_schema views use: the
//! 2-arg `pg_has_role(role, privilege)` and 3-arg
//! `pg_has_role(user, role, privilege)` shapes, with role/user as an OID or a
//! name, bare or `pg_catalog`-qualified.

use arrow::array::{Array, BooleanArray};
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

#[tokio::test]
async fn test_pg_has_role_two_arg() -> DFResult<()> {
    let ctx = base_ctx().await?;
    // role by name and by OID
    assert_eq!(
        bool_column(&ctx, "SELECT pg_has_role('sysuser', 'USAGE')").await?,
        vec![Some(true)]
    );
    assert_eq!(
        bool_column(&ctx, "SELECT pg_has_role(10, 'MEMBER')").await?,
        vec![Some(true)]
    );
    // schema-qualified
    assert_eq!(
        bool_column(&ctx, "SELECT pg_catalog.pg_has_role(10, 'USAGE')").await?,
        vec![Some(true)]
    );
    Ok(())
}

#[tokio::test]
async fn test_pg_has_role_three_arg() -> DFResult<()> {
    let ctx = base_ctx().await?;
    // (user, role, privilege) - names and OIDs
    assert_eq!(
        bool_column(&ctx, "SELECT pg_has_role('sysuser', 'sysuser', 'USAGE')").await?,
        vec![Some(true)]
    );
    assert_eq!(
        bool_column(&ctx, "SELECT pg_has_role(10, 10, 'MEMBER')").await?,
        vec![Some(true)]
    );
    Ok(())
}

#[tokio::test]
async fn test_pg_has_role_over_a_column() -> DFResult<()> {
    let ctx = base_ctx().await?;
    // Per-row evaluation (as the views use it): every row must be true.
    let rows = bool_column(
        &ctx,
        "SELECT pg_has_role(oid, 'USAGE') FROM pg_catalog.pg_namespace",
    )
    .await?;
    assert!(!rows.is_empty());
    assert!(
        rows.iter().all(|r| *r == Some(true)),
        "pg_has_role must be true for every row, got {rows:?}"
    );
    Ok(())
}
