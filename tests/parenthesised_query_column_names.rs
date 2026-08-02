//! A parenthesised statement names its columns the way an unparenthesised one does.
//!
//! `alias_columns_in_set_expr` only aliases a projection at depth 0 - those are
//! the columns the client sees. sqlparser wraps a parenthesised body in
//! `SetExpr::Query`, and that arm recursed with `depth + 1`, so wrapping a
//! statement in brackets pushed its own projection out of reach of the guard.
//! Parentheses do not introduce a scope, so the depth must not change.
//!
//! The symptom was client-visible, and it leaked `DataFusion`'s internal
//! expression names:
//!
//! ```text
//! SELECT 1        ?column?                  (correct)
//! (SELECT 1)      Int64(1)
//! (SELECT 1 + 1)  Int64(1) + Int64(1)
//! ```
//!
//! `PostgreSQL` answers `?column?` for all of them.

use datafusion::error::Result as DFResult;
use datafusion_pg_catalog::session::execute_sql;

mod common;
use common::base_ctx;

/// The name of the single column `sql` projects.
async fn only_column_name(
    ctx: &datafusion::prelude::SessionContext,
    sql: &str,
) -> DFResult<String> {
    let (_batches, schema) = execute_sql(ctx, sql, None, None).await?;
    Ok(schema.field(0).name().clone())
}

/// Bracketing a statement does not change what its column is called.
#[tokio::test(flavor = "multi_thread")]
async fn parentheses_do_not_change_a_columns_name() -> DFResult<()> {
    let ctx = base_ctx().await?;
    for (plain, bracketed) in [
        ("SELECT 1", "(SELECT 1)"),
        ("SELECT 1 + 1", "(SELECT 1 + 1)"),
        (
            "SELECT relname FROM pg_catalog.pg_class LIMIT 1",
            "(SELECT relname FROM pg_catalog.pg_class LIMIT 1)",
        ),
    ] {
        assert_eq!(
            only_column_name(&ctx, plain).await?,
            only_column_name(&ctx, bracketed).await?,
            "{bracketed} should name its column exactly as {plain} does"
        );
    }
    Ok(())
}

/// An unnamed expression is `?column?`, bracketed or not.
///
/// The assertion above only says the two agree; without this one they could
/// agree on `Int64(1)` and still be wrong.
#[tokio::test(flavor = "multi_thread")]
async fn an_unnamed_expression_is_called_column() -> DFResult<()> {
    let ctx = base_ctx().await?;
    for sql in ["SELECT 1", "(SELECT 1)", "SELECT 1 + 1", "(SELECT 1 + 1)"] {
        let name = only_column_name(&ctx, sql).await?;
        assert_eq!(
            name, "?column?",
            "{sql} should name its column ?column? as PostgreSQL does, got {name}"
        );
    }
    Ok(())
}

/// A named column keeps its own name through brackets.
#[tokio::test(flavor = "multi_thread")]
async fn a_named_column_keeps_its_name() -> DFResult<()> {
    let ctx = base_ctx().await?;
    assert_eq!(
        only_column_name(&ctx, "(SELECT 1 AS answer)").await?,
        "answer"
    );
    assert_eq!(
        only_column_name(&ctx, "(SELECT relname FROM pg_catalog.pg_class LIMIT 1)").await?,
        "relname"
    );
    Ok(())
}
