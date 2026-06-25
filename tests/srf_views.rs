//! Tests for the set-returning-function support that lets information_schema
//! views using `(srf(x)).field` run on DataFusion: the SRFs are scalar functions
//! returning `List<Struct>` and `rewrite_srf_to_unnest` turns the projection
//! access into an `unnest` + struct-subscript form.

use arrow::array::{Array, StringArray};
use datafusion::error::Result as DFResult;
use datafusion_pg_catalog::get_base_session_context;
use datafusion_pg_catalog::replace::rewrite_srf_to_unnest;
use datafusion_pg_catalog::session::execute_sql;

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

#[test]
fn test_rewrite_srf_shape() {
    // `(srf(x)).a, (srf(x)).b FROM t` -> unnest in a derived table, fields via [].
    let out = rewrite_srf_to_unnest(
        "SELECT (pg_options_to_table(opts)).option_name AS n, \
         (pg_options_to_table(opts)).option_value AS v FROM t WHERE id > 0",
    )
    .unwrap();
    let lo = out.to_lowercase();
    assert!(
        lo.contains("unnest(pg_options_to_table(opts))"),
        "got {out}"
    );
    assert!(lo.contains("__srf_unnest['option_name']"), "got {out}");
    assert!(lo.contains("__srf_unnest['option_value']"), "got {out}");
    assert!(
        lo.contains("where id > 0"),
        "WHERE should move inside: {out}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_options_to_table_unnest_executes() -> DFResult<()> {
    let ctx = base_ctx().await?;
    // The rewritten form executes and parses "k=v" options into rows.
    let sql = rewrite_srf_to_unnest(
        "SELECT (pg_options_to_table(opts)).option_name AS n, \
         (pg_options_to_table(opts)).option_value AS v \
         FROM (SELECT ARRAY['host=h','port=5432'] AS opts) d",
    )
    .unwrap();
    let batches = ctx.sql(&sql).await?.collect().await?;
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 2);
    let n = batches[0]
        .column_by_name("n")
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    let v = batches[0]
        .column_by_name("v")
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    // Rows: (host, h) and (port, 5432) in array order.
    assert!(!n.is_null(0) && n.value(0) == "host" && v.value(0) == "h");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_foreign_data_wrapper_options_view_runs() -> DFResult<()> {
    // The real information_schema view SQL, end-to-end through the full pipeline
    // (this is the case that previously failed: SRF in projection + the
    // group-by-injection heuristic). It must plan and execute (0 rows is fine -
    // no foreign-data wrappers exist in the base catalog).
    let ctx = base_ctx().await?;
    let raw = "SELECT foreign_data_wrapper_catalog, foreign_data_wrapper_name, \
        (pg_options_to_table(fdwoptions)).option_name::information_schema.sql_identifier AS option_name, \
        (pg_options_to_table(fdwoptions)).option_value::information_schema.character_data AS option_value \
        FROM information_schema._pg_foreign_data_wrappers w";
    let (batches, _schema) = execute_sql(&ctx, raw, None, None).await?;
    // Executes without error; row count is 0 (no FDWs).
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 0);
    Ok(())
}

#[test]
fn test_rewrite_srf_aliased_form() {
    // `_pg_expandarray(x) AS a` in a subquery + `(ss.a).field` one level up:
    // the SRF is wrapped in `unnest`, and the `(ss.a).field` becomes a subscript.
    let out = rewrite_srf_to_unnest(
        "SELECT (ss.a).n AS n FROM (SELECT _pg_expandarray(c.conkey) AS a FROM t c) ss \
         WHERE x = (ss.a).x",
    )
    .unwrap();
    let lo = out.to_lowercase();
    assert!(
        lo.contains("unnest(_pg_expandarray(c.conkey)) as a"),
        "got {out}"
    );
    assert!(lo.contains("(ss.a)['n']"), "got {out}");
    assert!(lo.contains("(ss.a)['x']"), "got {out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_triggered_update_columns_view_runs() -> DFResult<()> {
    // A real `_pg_expandarray` view end-to-end (0 rows: no triggers).
    let ctx = base_ctx().await?;
    let raw = "SELECT (ss.x).n AS ordinal_position, (ss.x).x AS attnum \
        FROM ( SELECT information_schema._pg_expandarray(ARRAY['1','2']) AS x ) ss";
    let (batches, _schema) = execute_sql(&ctx, raw, None, None).await?;
    // unnest fans the 2-element array to 2 rows.
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_expandarray_integer_array_yields_integer_x() -> DFResult<()> {
    // `_pg_expandarray` over an integer array (the form `conkey`/`proargtypes`
    // now arrive as) must expose the element value as an integer `x`, so views
    // can compare it to int columns (e.g. `pg_attribute.attnum = (ss.x).x`).
    use arrow::array::Int64Array;
    let ctx = base_ctx().await?;
    let raw = "SELECT (ss.x).x AS attnum, (ss.x).n AS ordinal_position \
        FROM ( SELECT information_schema._pg_expandarray(ARRAY[7, 9]) AS x ) ss \
        ORDER BY ordinal_position";
    let (batches, _schema) = execute_sql(&ctx, raw, None, None).await?;
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 2);
    let attnum = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("x is Int64");
    assert_eq!(attnum.value(0), 7);
    assert_eq!(attnum.value(1), 9);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_aclexplode_stub_empty() -> DFResult<()> {
    // aclexplode returns no rows (we model no grants), so the inline privilege
    // pattern unnests to zero rows and the surrounding view returns empty.
    let ctx = base_ctx().await?;
    let raw = "SELECT (aclexplode(acldefault('r', 10))).grantee AS grantee \
        FROM (SELECT 1) d";
    let (batches, _schema) = execute_sql(&ctx, raw, None, None).await?;
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_table_privileges_view_runs() -> DFResult<()> {
    // The full table_privileges view (inline aclexplode over pg_class, with a
    // positional column-alias list and qualified column refs) must plan and run.
    let ctx = base_ctx().await?;
    let raw = "SELECT c.relname FROM ( SELECT pg_class.oid, pg_class.relname, \
        (aclexplode(COALESCE(pg_class.relacl, acldefault('r', pg_class.relowner)))).grantee AS grantee \
        FROM pg_catalog.pg_class) c(oid, relname, grantee)";
    let (batches, _schema) = execute_sql(&ctx, raw, None, None).await?;
    // No grants -> aclexplode empty -> 0 rows.
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 0);
    Ok(())
}
