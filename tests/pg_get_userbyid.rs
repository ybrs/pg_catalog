//! Tests for the batched `pg_get_userbyid` implementation.
//!
//! `pg_get_userbyid(oid)` resolves a role OID to its `rolname`. It is evaluated
//! over whole columns (e.g. `pg_tables.tableowner`), so it must resolve the
//! distinct OIDs with a single `pg_authid` lookup rather than one query per row.
//! These tests pin down the result semantics that optimization must preserve.

use arrow::array::{Array, StringArray};
use datafusion::error::Result as DFResult;
use datafusion_pg_catalog::get_base_session_context;

/// Collect a single-column Utf8 result into a `Vec<Option<String>>`.
async fn utf8_column(
    ctx: &datafusion::execution::context::SessionContext,
    sql: &str,
) -> DFResult<Vec<Option<String>>> {
    let batches = ctx.sql(sql).await?.collect().await?;
    let mut out = Vec::new();
    for b in &batches {
        let arr = b
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("expected a Utf8 column");
        for i in 0..arr.len() {
            out.push(if arr.is_null(i) {
                None
            } else {
                Some(arr.value(i).to_string())
            });
        }
    }
    Ok(out)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_get_userbyid_matches_rolname_for_every_role() -> DFResult<()> {
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;

    // For every role, pg_get_userbyid(oid) must equal that row's rolname. This
    // exercises the batched lookup across many distinct OIDs in one call.
    let batches = ctx
        .sql(
            "SELECT rolname, pg_get_userbyid(oid) AS resolved \
             FROM pg_catalog.pg_authid ORDER BY oid",
        )
        .await?
        .collect()
        .await?;

    let mut total_rows = 0;
    for b in &batches {
        let rolname = b
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("rolname is text");
        let resolved = b
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("resolved is text");
        for i in 0..b.num_rows() {
            assert!(!rolname.is_null(i), "test assumes non-null rolnames");
            assert_eq!(
                resolved.value(i),
                rolname.value(i),
                "pg_get_userbyid(oid) should equal rolname"
            );
        }
        total_rows += b.num_rows();
    }
    assert!(
        total_rows >= 2,
        "expected several built-in roles to test against"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_get_userbyid_null_and_unknown() -> DFResult<()> {
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;

    // NULL OID -> NULL.
    let nulls = utf8_column(&ctx, "SELECT pg_get_userbyid(CAST(NULL AS BIGINT))").await?;
    assert_eq!(nulls, vec![None]);

    // An OID with no matching role -> "unknown (OID=...)" placeholder.
    let unknown = utf8_column(&ctx, "SELECT pg_get_userbyid(987654321)").await?;
    assert_eq!(unknown, vec![Some("unknown (OID=987654321)".to_string())]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_get_userbyid_dedups_repeated_oids() -> DFResult<()> {
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;

    // Pick a real role oid, then resolve it repeated many times in one column.
    // The batched lookup dedups to a single pg_authid query and every row must
    // still resolve to the same name.
    let role = ctx
        .sql("SELECT oid, rolname FROM pg_catalog.pg_authid ORDER BY oid LIMIT 1")
        .await?
        .collect()
        .await?;
    let oid = role[0]
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::Int32Array>()
        .map(|a| a.value(0) as i64)
        .or_else(|| {
            role[0]
                .column(0)
                .as_any()
                .downcast_ref::<arrow::array::Int64Array>()
                .map(|a| a.value(0))
        })
        .expect("oid column is an integer");
    let expected = role[0]
        .column(1)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap()
        .value(0)
        .to_string();

    let resolved = utf8_column(
        &ctx,
        &format!("SELECT pg_get_userbyid(o) FROM (VALUES ({oid}),({oid}),({oid})) AS v(o)"),
    )
    .await?;
    assert_eq!(
        resolved,
        vec![
            Some(expected.clone()),
            Some(expected.clone()),
            Some(expected)
        ]
    );
    Ok(())
}
