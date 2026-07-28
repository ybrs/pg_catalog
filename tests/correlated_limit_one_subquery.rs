//! A correlated scalar subquery with `LIMIT 1` must return the matching value.
//!
//! `DataFusion` decorrelates such a subquery by pulling its correlation predicate
//! up into a join while leaving the `LIMIT` in place, which limits the subquery
//! relation as a whole instead of each outer row. The plan is accepted and
//! every row comes back NULL, so this fails silently rather than loudly --
//! hence the execution tests below assert the values, not just that the query
//! plans.
//!
//! `rewrite_correlated_limit_one_subquery_to_max` rewrites the shape into a
//! `max` aggregate, which decorrelates correctly.

use arrow::array::{Array, StringArray};
use datafusion::error::Result as DFResult;
use datafusion_pg_catalog::replace::rewrite_correlated_limit_one_subquery_to_max;
use datafusion_pg_catalog::session::execute_sql;

mod common;
use common::base_ctx;

/// Collect one string column from a query's results, with NULLs as None.
async fn string_column(
    ctx: &datafusion::prelude::SessionContext,
    sql: &str,
    column: usize,
) -> DFResult<Vec<Option<String>>> {
    let (batches, _schema) = execute_sql(ctx, sql, None, None).await?;
    let mut values = Vec::new();
    for batch in &batches {
        let array = batch
            .column(column)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("column is a string");
        for row in 0..array.len() {
            values.push(if array.is_null(row) {
                None
            } else {
                Some(array.value(row).to_string())
            });
        }
    }
    Ok(values)
}

#[test]
fn test_correlated_limit_one_becomes_max() {
    let out = rewrite_correlated_limit_one_subquery_to_max(
        "SELECT t.typname, (SELECT n.nspname FROM pg_namespace n \
         WHERE n.oid = t.typnamespace LIMIT 1) AS ns FROM pg_type t",
    )
    .unwrap();
    let lowered = out.to_lowercase();
    assert!(lowered.contains("max(n.nspname)"), "got {out}");
    assert!(!lowered.contains("limit 1"), "limit should be gone: {out}");
}

#[test]
fn test_uncorrelated_limit_one_is_left_alone() {
    // Uncorrelated LIMIT 1 subqueries already plan and answer correctly, and
    // max() would change which row they return.
    let sql = "SELECT (SELECT n.nspname FROM pg_namespace n LIMIT 1) AS ns FROM pg_type t";
    let out = rewrite_correlated_limit_one_subquery_to_max(sql).unwrap();
    let lowered = out.to_lowercase();
    assert!(!lowered.contains("max("), "got {out}");
    assert!(lowered.contains("limit 1"), "got {out}");
}

#[test]
fn test_ordered_limit_one_is_left_alone() {
    // ORDER BY ... LIMIT 1 means "first by that ordering", which max() does not
    // express; rewriting it would silently change the answer.
    let sql = "SELECT t.typname, (SELECT n.nspname FROM pg_namespace n \
               WHERE n.oid = t.typnamespace ORDER BY n.nspname LIMIT 1) AS ns FROM pg_type t";
    let out = rewrite_correlated_limit_one_subquery_to_max(sql).unwrap();
    assert!(!out.to_lowercase().contains("max("), "got {out}");
}

#[test]
fn test_limit_other_than_one_is_left_alone() {
    let sql = "SELECT t.typname, (SELECT n.nspname FROM pg_namespace n \
               WHERE n.oid = t.typnamespace LIMIT 2) AS ns FROM pg_type t";
    let out = rewrite_correlated_limit_one_subquery_to_max(sql).unwrap();
    assert!(!out.to_lowercase().contains("max("), "got {out}");
}

#[test]
fn test_already_aggregated_projection_is_left_alone() {
    // Wrapping an existing aggregate in another max() would change what it
    // computes, and such a subquery decorrelates correctly already.
    let sql = "SELECT t.typname, (SELECT max(n.nspname) FROM pg_namespace n \
               WHERE n.oid = t.typnamespace LIMIT 1) AS ns FROM pg_type t";
    let out = rewrite_correlated_limit_one_subquery_to_max(sql).unwrap();
    assert!(!out.to_lowercase().contains("max(max("), "got {out}");
}

#[test]
fn test_subquery_without_limit_is_left_alone() {
    let sql = "SELECT t.typname, (SELECT n.nspname FROM pg_namespace n \
               WHERE n.oid = t.typnamespace) AS ns FROM pg_type t";
    let out = rewrite_correlated_limit_one_subquery_to_max(sql).unwrap();
    assert!(!out.to_lowercase().contains("max("), "got {out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_correlated_limit_one_returns_the_matching_value() -> DFResult<()> {
    // The regression this whole rewrite exists for: before it, every row came
    // back NULL instead of its namespace name.
    let ctx = base_ctx().await?;
    let sql = "SELECT t.typname, (SELECT n.nspname FROM pg_catalog.pg_namespace n \
               WHERE n.oid = t.typnamespace LIMIT 1) AS ns \
               FROM pg_catalog.pg_type t ORDER BY t.typname LIMIT 5";
    let values = string_column(&ctx, sql, 1).await?;
    assert!(!values.is_empty(), "expected rows");
    assert!(
        values.iter().all(std::option::Option::is_some),
        "correlated LIMIT 1 subquery returned NULLs: {values:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_limit_one_matches_the_join_equivalent() -> DFResult<()> {
    // Not merely non-NULL: the values must equal what the equivalent LEFT JOIN
    // reports, so a rewrite that returns some other row would still fail.
    let ctx = base_ctx().await?;
    let subquery = "SELECT t.typname, (SELECT n.nspname FROM pg_catalog.pg_namespace n \
                    WHERE n.oid = t.typnamespace LIMIT 1) AS ns \
                    FROM pg_catalog.pg_type t ORDER BY t.typname LIMIT 5";
    let join = "SELECT t.typname, n.nspname AS ns FROM pg_catalog.pg_type t \
                LEFT JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
                ORDER BY t.typname LIMIT 5";
    assert_eq!(
        string_column(&ctx, subquery, 1).await?,
        string_column(&ctx, join, 1).await?
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_uncorrelated_limit_one_still_executes() -> DFResult<()> {
    // The rewrite skips this shape, so it must still plan and return a value.
    let ctx = base_ctx().await?;
    let sql = "SELECT (SELECT n.nspname FROM pg_catalog.pg_namespace n LIMIT 1) AS ns \
               FROM pg_catalog.pg_type t LIMIT 3";
    let values = string_column(&ctx, sql, 0).await?;
    assert_eq!(values.len(), 3);
    assert!(
        values.iter().all(std::option::Option::is_some),
        "got {values:?}"
    );
    Ok(())
}
