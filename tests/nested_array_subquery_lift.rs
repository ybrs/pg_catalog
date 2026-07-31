//! An `ARRAY(SELECT ...)` written inside another `ARRAY(SELECT ...)`.
//!
//! `rewrite_array_subquery` turns every `ARRAY(SELECT ...)` into
//! `pg_get_array(<sub-query>)`, an aggregate over a scalar sub-query, long
//! before the lifter runs. The lifter then moves the outer sub-query into a CTE
//! and joins it back -- but until now it never looked at the body it had just
//! created, so a second `ARRAY(...)` written inside the first stayed a plain
//! scalar sub-query in that body. A scalar sub-query returning more than one
//! row is not an error `DataFusion` catches while planning; it fails at execution
//! with "Scalar subquery returned more than one row".
//!
//! The lift now recurses into each body it produces. Two things follow, and
//! both are tested here:
//!
//! 1. A nested sub-query correlated by an equality predicate is lifted into the
//!    body's own WITH clause, and the array it builds holds one element per
//!    inner row.
//! 2. A nested sub-query with no correlation predicate is NOT lifted. It could
//!    only be joined back with `ON true`, and the GROUP BY that cross join
//!    forces would collapse the body's rows -- silently handing the enclosing
//!    aggregate an array with the wrong contents. The planner's loud rejection
//!    is the better answer, so the rewriter leaves that shape alone.
//!
//! The element-count assertions below are the point of the file. A lifted body
//! that fails to resolve says so; one that collapses rows returns a plausible
//! string of the wrong length, which is why the counts are compared against the
//! same numbers computed by a plain grouped join.

use std::collections::HashMap;

use arrow::array::Array;
use datafusion::error::Result as DFResult;
use datafusion::prelude::SessionContext;
use datafusion_pg_catalog::scalar_to_cte::rewrite_subquery_as_cte;
use datafusion_pg_catalog::session::execute_sql;

mod common;
use common::base_ctx;

/// A nested ARRAY over `pg_class`, keyed by namespace, whose elements each hold
/// that relation's column names.
///
/// Both levels are correlated by a qualified equality predicate, which is the
/// shape the lifter can carry into a CTE: the outer sub-query on
/// `c.relnamespace = n.oid`, the inner on `a.attrelid = c.oid`.
const NESTED_ARRAY_QUERY: &str = "SELECT n.nspname, pg_catalog.array_to_string(ARRAY(\
     SELECT c.relname || ':' || pg_catalog.array_to_string(ARRAY(\
       SELECT a.attname FROM pg_catalog.pg_attribute a WHERE a.attrelid = c.oid), ',') \
     FROM pg_catalog.pg_class c WHERE c.relnamespace = n.oid), '|') \
     FROM pg_catalog.pg_namespace n";

/// Run `sql` and return its two text columns as a name -> value map.
///
/// Every query here projects a name and one built-up string, so this is the
/// only result shape the file needs.
async fn text_pairs(ctx: &SessionContext, sql: &str) -> DFResult<HashMap<String, String>> {
    let (batches, _schema) = execute_sql(ctx, sql, None, None).await?;
    let mut out = HashMap::new();
    for batch in &batches {
        let names = arrow::array::as_string_array(batch.column(0));
        let values = arrow::array::as_string_array(batch.column(1));
        for row in 0..batch.num_rows() {
            let value = if values.is_null(row) {
                String::new()
            } else {
                values.value(row).to_string()
            };
            out.insert(names.value(row).to_string(), value);
        }
    }
    Ok(out)
}

/// How many relations each namespace holds, computed with a plain grouped join.
///
/// This is the number the nested array's element count has to match. Deriving
/// it separately is what makes the comparison worth anything: a lifter that
/// collapses rows would agree with itself but not with this.
async fn relations_per_namespace(ctx: &SessionContext) -> DFResult<HashMap<String, usize>> {
    let (batches, _schema) = execute_sql(
        ctx,
        "SELECT n.nspname, count(*) FROM pg_catalog.pg_namespace n \
         JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid GROUP BY n.nspname",
        None,
        None,
    )
    .await?;
    let mut out = HashMap::new();
    for batch in &batches {
        let names = arrow::array::as_string_array(batch.column(0));
        let counts =
            arrow::array::as_primitive_array::<arrow::datatypes::Int64Type>(batch.column(1));
        for row in 0..batch.num_rows() {
            out.insert(
                names.value(row).to_string(),
                usize::try_from(counts.value(row)).unwrap(),
            );
        }
    }
    Ok(out)
}

/// The outer array holds one element per inner row, not one per group.
///
/// Before the lift recursed this query failed outright. The risk in fixing it
/// is not that it fails again but that it succeeds with the wrong number of
/// elements: the body's rows feed an aggregate, so a stray GROUP BY inside the
/// body silently shortens every array. Hence counting elements against a
/// separately computed relation count rather than asserting the query runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_nested_array_holds_one_element_per_inner_row() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let expected = relations_per_namespace(&ctx).await?;
    assert!(
        expected.len() >= 3,
        "the fixture catalog should have several populated namespaces: {expected:?}"
    );

    let actual = text_pairs(&ctx, NESTED_ARRAY_QUERY).await?;
    for (namespace, relation_count) in &expected {
        let joined = actual
            .get(namespace)
            .unwrap_or_else(|| panic!("no row for namespace {namespace}"));
        let elements = joined.split('|').count();
        assert_eq!(
            elements, *relation_count,
            "namespace {namespace} holds {relation_count} relations, so its array \
             needs that many elements, not {elements}"
        );
    }
    Ok(())
}

/// Each element of the outer array carries the inner array's own answer.
///
/// The count assertion above would still pass if every element held the same
/// wrongly shared value, which is exactly what a cross join produces. This
/// compares one element against the inner sub-query run on its own.
#[tokio::test(flavor = "multi_thread")]
async fn each_nested_element_matches_the_inner_query_run_alone() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let nested = text_pairs(&ctx, NESTED_ARRAY_QUERY).await?;
    let pg_catalog_elements = nested
        .get("pg_catalog")
        .expect("the pg_catalog namespace is always populated");

    let alone = text_pairs(
        &ctx,
        "SELECT c.relname, pg_catalog.array_to_string(ARRAY(\
         SELECT a.attname FROM pg_catalog.pg_attribute a WHERE a.attrelid = c.oid), ',') \
         FROM pg_catalog.pg_class c WHERE c.relname = 'pg_class'",
    )
    .await?;
    let columns_of_pg_class = alone
        .get("pg_class")
        .expect("pg_class is a relation in the fixture catalog");
    assert!(
        !columns_of_pg_class.is_empty(),
        "pg_class has columns, so the inner array is not empty"
    );

    let expected_element = format!("pg_class:{columns_of_pg_class}");
    assert!(
        pg_catalog_elements
            .split('|')
            .any(|element| element == expected_element),
        "the element for pg_class should be {expected_element:?}, and the array \
         should hold it verbatim"
    );
    Ok(())
}

/// A nested sub-query the lift produced is rewritten in the body's own WITH.
///
/// Structural rather than behavioural: it pins WHERE the inner CTE lands. Hung
/// on the outer query's WITH instead, it would be out of scope for the body
/// that references it.
#[test]
fn a_nested_correlated_subquery_lifts_into_the_body_it_was_written_in() {
    let rewritten = rewrite_subquery_as_cte(
        "SELECT n.nspname, pg_catalog.pg_get_array((\
         SELECT pg_catalog.pg_get_array((SELECT a.attname FROM pg_attribute a \
         WHERE a.attrelid = c.oid)) FROM pg_class c WHERE c.relnamespace = n.oid)) \
         FROM pg_namespace n",
    );
    let body_start = rewritten
        .find("__cte1 AS (")
        .expect("the outer sub-query is lifted");
    let body = &rewritten[body_start..];
    assert!(
        body.starts_with("__cte1 AS (WITH __cte2 AS ("),
        "the inner sub-query belongs to __cte1's own WITH clause: {rewritten}"
    );
}

/// A nested sub-query with no correlation predicate is left for the planner.
///
/// Lifting it means `LEFT JOIN ... ON true` plus a GROUP BY over whatever else
/// the body projects, and that collapses the rows the enclosing aggregate is
/// counting on. Declining keeps the failure loud instead of turning it into an
/// array that is quietly missing most of its elements.
#[test]
fn an_uncorrelated_subquery_inside_a_lifted_body_is_not_lifted() {
    let rewritten = rewrite_subquery_as_cte(
        "SELECT n.nspname, pg_catalog.pg_get_array((\
         SELECT pg_catalog.pg_get_array((SELECT r.rolname FROM pg_roles r)) \
         FROM pg_class c WHERE c.relnamespace = n.oid)) \
         FROM pg_namespace n",
    );
    assert!(
        rewritten.contains("__cte1"),
        "the outer correlated sub-query is still lifted: {rewritten}"
    );
    assert!(
        !rewritten.contains("__cte2"),
        "the uncorrelated inner sub-query must not be lifted into the body: {rewritten}"
    );
}

/// The declined shape fails rather than returning a short array.
///
/// The string check above says the rewriter leaves it alone; this says what
/// leaving it alone is worth. The value to protect is that a wrong answer never
/// reaches the client -- so if a later change makes this shape work, this test
/// fails and has to be replaced by an assertion on the elements, exactly as the
/// psql tier's expected failures are meant to be promoted.
#[tokio::test(flavor = "multi_thread")]
async fn an_uncorrelated_nested_array_fails_instead_of_answering_wrongly() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let result = execute_sql(
        &ctx,
        "SELECT n.nspname, pg_catalog.array_to_string(ARRAY(\
         SELECT pg_catalog.array_to_string(ARRAY(\
           SELECT r.rolname FROM pg_catalog.pg_roles r), ',') \
         FROM pg_catalog.pg_class c WHERE c.relnamespace = n.oid), '|') \
         FROM pg_catalog.pg_namespace n",
        None,
        None,
    )
    .await;
    assert!(
        result.is_err(),
        "an uncorrelated ARRAY nested in a lifted body has no correct rewrite \
         yet, so it must fail rather than return an array missing most of its \
         elements"
    );
    Ok(())
}

/// The same uncorrelated shape at the top level is still lifted.
///
/// The rule above is about lifted bodies only. At the top level the injected
/// GROUP BY reproduces the grouping the projection already implies, and that
/// behaviour predates this work -- narrowing it would be a regression, so this
/// pins the boundary between the two cases.
#[test]
fn a_top_level_uncorrelated_subquery_is_still_lifted() {
    let rewritten = rewrite_subquery_as_cte(
        "SELECT c.relname, pg_catalog.pg_get_array((SELECT n.nspname FROM pg_namespace n)) \
         FROM pg_class c",
    );
    assert!(
        rewritten.contains("__cte1"),
        "an uncorrelated sub-query in the outermost query is lifted as before: {rewritten}"
    );
}

/// An uncorrelated ARRAY over many rows still returns its one row.
///
/// The behavioural half of the boundary above, run end to end so the top-level
/// path is covered by more than a string check.
#[tokio::test(flavor = "multi_thread")]
async fn a_top_level_uncorrelated_array_still_returns_one_row() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let (batches, _schema) = execute_sql(
        &ctx,
        "SELECT array(SELECT n.nspname FROM pg_catalog.pg_namespace n)",
        None,
        None,
    )
    .await?;
    let rows: usize = batches
        .iter()
        .map(arrow::array::RecordBatch::num_rows)
        .sum();
    assert_eq!(rows, 1, "an array of every namespace name is one row");
    Ok(())
}
