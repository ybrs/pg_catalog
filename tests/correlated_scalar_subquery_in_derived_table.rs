//! A correlated scalar subquery must work inside a derived table, not only at
//! the top level of a SELECT.
//!
//! `DataFusion` refuses to plan a correlated scalar subquery unless it is
//! aggregated or provably single-row (datafusion-expr, `logical_plan/invariants.rs`).
//! `pg_catalog` avoids that by rewriting such subqueries into a CTE plus a LEFT
//! JOIN in `scalar_to_cte`. That rewriter used to walk only the outermost
//! SELECT's projection, so a subquery inside a derived table was never
//! converted and reached `DataFusion`, which rejected the whole query with
//! "Invalid (non-executable) plan after Analyzer".
//!
//! Npgsql's startup type query is exactly that shape -- its correlated
//! multirange lookup sits in a SELECT that is then wrapped as a derived table --
//! so this decided whether .NET clients could connect at all.

use arrow::array::{Array, StringArray};
use datafusion::error::Result as DFResult;
use datafusion_pg_catalog::scalar_to_cte::rewrite_subquery_as_cte;
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
fn test_subquery_inside_a_derived_table_is_rewritten() {
    // The rewriter must reach into the derived table; before, it only walked
    // the outermost projection and left this subquery in place.
    let out = rewrite_subquery_as_cte(
        "SELECT s.typname, s.ns FROM ( \
           SELECT t.typname, (SELECT max(n.nspname) FROM pg_namespace n \
             WHERE n.oid = t.typnamespace) AS ns \
           FROM pg_type t \
         ) AS s",
    );
    assert!(
        out.to_uppercase().contains("WITH"),
        "expected a CTE to be introduced: {out}"
    );
}

#[test]
fn test_top_level_subquery_is_still_rewritten() {
    // The case that already worked must keep working.
    let out = rewrite_subquery_as_cte(
        "SELECT t.typname, (SELECT max(n.nspname) FROM pg_namespace n \
           WHERE n.oid = t.typnamespace) AS ns FROM pg_type t",
    );
    assert!(out.to_uppercase().contains("WITH"), "got {out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_derived_table_subquery_plans_and_returns_values() -> DFResult<()> {
    // Before the fix this failed to plan outright.
    let ctx = base_ctx().await?;
    let sql = "SELECT s.typname, s.ns FROM ( \
                 SELECT t.typname, (SELECT n.nspname FROM pg_catalog.pg_namespace n \
                   WHERE n.oid = t.typnamespace) AS ns \
                 FROM pg_catalog.pg_type t \
               ) AS s ORDER BY s.typname LIMIT 5";
    let values = string_column(&ctx, sql, 1).await?;
    assert!(!values.is_empty(), "expected rows");
    assert!(
        values.iter().all(std::option::Option::is_some),
        "got {values:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_derived_table_subquery_matches_the_join_equivalent() -> DFResult<()> {
    // Planning is not enough: the values must match the equivalent LEFT JOIN,
    // so a rewrite that plans but joins wrongly still fails.
    let ctx = base_ctx().await?;
    let derived = "SELECT s.typname, s.ns FROM ( \
                     SELECT t.typname, (SELECT n.nspname FROM pg_catalog.pg_namespace n \
                       WHERE n.oid = t.typnamespace) AS ns \
                     FROM pg_catalog.pg_type t \
                   ) AS s ORDER BY s.typname LIMIT 5";
    let join = "SELECT t.typname, n.nspname AS ns FROM pg_catalog.pg_type t \
                LEFT JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
                ORDER BY t.typname LIMIT 5";
    assert_eq!(
        string_column(&ctx, derived, 1).await?,
        string_column(&ctx, join, 1).await?
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_subquery_inside_a_case_in_a_derived_table_plans() -> DFResult<()> {
    // Npgsql's actual shape: the correlated lookup is one arm of a CASE inside
    // the derived table.
    let ctx = base_ctx().await?;
    let sql = "SELECT s.typname, s.ns FROM ( \
                 SELECT t.typname, \
                        CASE WHEN t.typtype = 'b' \
                             THEN (SELECT n.nspname FROM pg_catalog.pg_namespace n \
                                    WHERE n.oid = t.typnamespace) \
                        END AS ns \
                 FROM pg_catalog.pg_type t \
               ) AS s ORDER BY s.typname LIMIT 5";
    let values = string_column(&ctx, sql, 1).await?;
    assert_eq!(values.len(), 5);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_two_levels_of_derived_tables_plan() -> DFResult<()> {
    // Npgsql nests two derived tables above the subquery, so one level of
    // recursion would not have been enough.
    let ctx = base_ctx().await?;
    let sql = "SELECT o.typname, o.ns FROM ( \
                 SELECT s.typname, s.ns FROM ( \
                   SELECT t.typname, (SELECT n.nspname FROM pg_catalog.pg_namespace n \
                     WHERE n.oid = t.typnamespace) AS ns \
                   FROM pg_catalog.pg_type t \
                 ) AS s \
               ) AS o ORDER BY o.typname LIMIT 5";
    let values = string_column(&ctx, sql, 1).await?;
    assert!(!values.is_empty(), "expected rows");
    assert!(
        values.iter().all(std::option::Option::is_some),
        "got {values:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_top_level_subquery_still_returns_values() -> DFResult<()> {
    // Guards the path that already worked against the recursion change.
    let ctx = base_ctx().await?;
    let sql = "SELECT t.typname, (SELECT n.nspname FROM pg_catalog.pg_namespace n \
               WHERE n.oid = t.typnamespace) AS ns \
               FROM pg_catalog.pg_type t ORDER BY t.typname LIMIT 5";
    let values = string_column(&ctx, sql, 1).await?;
    assert!(
        values.iter().all(std::option::Option::is_some),
        "got {values:?}"
    );
    Ok(())
}
