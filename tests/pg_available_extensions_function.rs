//! `pg_available_extensions` as the FUNCTION psql 18 joins, not only the
//! relation psql 17 selects from.
//!
//! `PostgreSQL` defines the `pg_available_extensions` VIEW over the function of
//! the same name, and psql 18 rewrote `\dx` to join the function directly:
//! `LEFT JOIN pg_catalog.pg_available_extensions() ae(name, default_version,
//! comment)`. Only the relation existed here, so the newer client failed -
//! and failed with a diagnostic naming `pg_catalog` instead of the function,
//! because `DataFusion` reports an unresolved compound table-function name by
//! its first part. Both halves are covered: the function serves the
//! relation's rows, and an unknown table function now fails naming ITSELF.

use datafusion::error::Result as DFResult;
use datafusion_pg_catalog::session::execute_sql;

mod common;
use common::base_ctx;

/// The single `Int64` value `sql` returns.
async fn single_count(ctx: &datafusion::prelude::SessionContext, sql: &str) -> DFResult<i64> {
    let (batches, _schema) = execute_sql(ctx, sql, None, None).await?;
    let cast = arrow::compute::cast(batches[0].column(0), &arrow::datatypes::DataType::Int64)?;
    Ok(arrow::array::as_primitive_array::<arrow::datatypes::Int64Type>(&cast).value(0))
}

/// The function answers the same rows as the relation, spelled every way a
/// client writes it: bare, qualified, and qualified with the column alias
/// list psql 18 uses.
#[tokio::test(flavor = "multi_thread")]
async fn the_function_serves_the_relations_rows_under_every_spelling() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let relation_rows = single_count(
        &ctx,
        "SELECT count(*) FROM pg_catalog.pg_available_extensions",
    )
    .await?;
    assert!(relation_rows > 0, "the snapshot ships extensions");

    for spelling in [
        "SELECT count(*) FROM pg_available_extensions()",
        "SELECT count(*) FROM pg_catalog.pg_available_extensions()",
        "SELECT count(*) FROM pg_catalog.pg_available_extensions() \
         ae(name, default_version, comment)",
    ] {
        assert_eq!(
            single_count(&ctx, spelling).await?,
            relation_rows,
            "{spelling}"
        );
    }
    Ok(())
}

/// The exact join psql 18's `\dx` writes resolves and finds the installed
/// extension.
#[tokio::test(flavor = "multi_thread")]
async fn the_psql_18_join_resolves() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let (batches, _schema) = execute_sql(
        &ctx,
        "SELECT e.extname, ae.default_version FROM pg_catalog.pg_extension e \
         LEFT JOIN pg_catalog.pg_available_extensions() ae(name, default_version, comment) \
         ON ae.name = e.extname ORDER BY 1",
        None,
        None,
    )
    .await?;
    let rows: usize = batches
        .iter()
        .map(arrow::array::RecordBatch::num_rows)
        .sum();
    assert_eq!(rows, 1, "one installed extension in the snapshot: plpgsql");
    Ok(())
}

/// An unknown table function fails naming the FUNCTION, not its schema.
///
/// The diagnostic half of the fix: every `pg_catalog`-qualified table-function
/// factor is unqualified before planning, so `DataFusion`'s first-part
/// reporting can no longer blame `pg_catalog`.
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_table_function_is_reported_by_its_own_name() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let error = execute_sql(
        &ctx,
        "SELECT * FROM pg_catalog.no_such_table_function()",
        None,
        None,
    )
    .await
    .expect_err("the function does not exist");
    let message = error.to_string();
    assert!(
        message.contains("no_such_table_function"),
        "the failing name must be the function's own: {message}"
    );
    assert!(
        !message.contains("table function 'pg_catalog'"),
        "the schema must not be blamed: {message}"
    );
    Ok(())
}
