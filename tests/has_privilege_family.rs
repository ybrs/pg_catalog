//! Tests for the `has_*_privilege` family and `nameconcatoid` stubs.
//!
//! Each `has_*_privilege` returns `true` (emulated superuser holds everything)
//! and must accept the 2-arg `(object, privilege)` and 3-arg
//! `(user, object, privilege)` shapes with OID or name arguments.
//! `nameconcatoid(name, oid)` returns `"<name>_<oid>"`.

use arrow::array::{Array, BooleanArray, StringArray};
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

async fn one_bool(
    ctx: &datafusion::execution::context::SessionContext,
    sql: &str,
) -> DFResult<Option<bool>> {
    let batches = ctx.sql(sql).await?.collect().await?;
    let arr = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<BooleanArray>()
        .expect("expected a Boolean column");
    Ok(if arr.is_null(0) {
        None
    } else {
        Some(arr.value(0))
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn test_has_privilege_family_all_names() -> DFResult<()> {
    let ctx = base_ctx().await?;
    // Every registered function name, in its 2-arg form.
    for fname in [
        "has_table_privilege",
        "has_column_privilege",
        "has_any_column_privilege",
        "has_type_privilege",
        "has_sequence_privilege",
        "has_function_privilege",
        "has_server_privilege",
        "has_foreign_data_wrapper_privilege",
        "has_tablespace_privilege",
        "has_language_privilege",
        "has_parameter_privilege",
    ] {
        let sql = format!("SELECT {fname}(1, 'USAGE')");
        assert_eq!(
            one_bool(&ctx, &sql).await?,
            Some(true),
            "{fname}(oid, priv)"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_has_privilege_arg_shapes() -> DFResult<()> {
    let ctx = base_ctx().await?;
    // 2-arg by name, 3-arg (user, object, priv), and schema-qualified.
    assert_eq!(
        one_bool(&ctx, "SELECT has_table_privilege('pg_class', 'SELECT')").await?,
        Some(true)
    );
    assert_eq!(
        one_bool(
            &ctx,
            "SELECT has_table_privilege('sysuser', 'pg_class', 'SELECT')"
        )
        .await?,
        Some(true)
    );
    assert_eq!(
        one_bool(&ctx, "SELECT has_column_privilege(1, 2, 'SELECT')").await?,
        Some(true)
    );
    assert_eq!(
        one_bool(&ctx, "SELECT pg_catalog.has_type_privilege(1, 'USAGE')").await?,
        Some(true)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_nameconcatoid() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let batches = ctx
        .sql("SELECT nameconcatoid('myfunc', 42)")
        .await?
        .collect()
        .await?;
    let arr = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("expected a Utf8 column");
    assert_eq!(arr.value(0), "myfunc_42");
    Ok(())
}
