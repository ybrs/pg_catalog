//! A correlated `count(*)` matching no rows answers 0, not NULL.
//!
//! The lift turns a correlated scalar sub-query into a CTE plus a LEFT JOIN, and
//! an outer row that matched nothing takes a NULL from that join. For `max`,
//! `min` and `string_agg` that is exactly right - an aggregate over an empty set
//! IS NULL. `count` is the exception: `PostgreSQL` counts an empty set as 0.
//!
//! So the sub-query's own aggregate was never consulted about what its
//! empty-set value should be, and a schema with no relations reported NULL where
//! `PostgreSQL` reports 0. Silent, and in one of the most common catalog shapes
//! there is - counting children per parent, which is what a navigator pane does
//! for every node it draws.
//!
//! The tests below check both directions. Coalescing everything to 0 would pass
//! the count assertions and be just as wrong, so the NULL cases carry equal
//! weight.

use datafusion::error::Result as DFResult;
use datafusion::prelude::SessionContext;
use datafusion_pg_catalog::session::execute_sql;

mod common;
use common::base_ctx;

/// A predicate no relation satisfies, so every outer row matches nothing.
const MATCHES_NOTHING: &str = "AND c.relname = 'zzz_no_such_relation'";

/// The single value `sql` projects, as an optional integer.
async fn single_count(ctx: &SessionContext, sql: &str) -> DFResult<Option<i64>> {
    use arrow::array::Array;

    let (batches, _schema) = execute_sql(ctx, sql, None, None).await?;
    for batch in &batches {
        if batch.num_rows() > 0 {
            let column = arrow::compute::cast(batch.column(0), &arrow::datatypes::DataType::Int64)?;
            if column.is_null(0) {
                return Ok(None);
            }
            return Ok(Some(
                arrow::array::as_primitive_array::<arrow::datatypes::Int64Type>(&column).value(0),
            ));
        }
    }
    Ok(None)
}

/// Whether the single value `sql` projects is NULL.
async fn projects_null(ctx: &SessionContext, sql: &str) -> DFResult<bool> {
    use arrow::array::Array;

    let (batches, _schema) = execute_sql(ctx, sql, None, None).await?;
    for batch in &batches {
        if batch.num_rows() > 0 {
            return Ok(batch.column(0).is_null(0));
        }
    }
    Ok(true)
}

/// A correlated count that matches nothing is 0.
#[tokio::test(flavor = "multi_thread")]
async fn a_correlated_count_over_no_matching_rows_is_zero() -> DFResult<()> {
    let ctx = base_ctx().await?;
    for aggregate in ["count(*)", "count(c.relname)", "count(DISTINCT c.relname)"] {
        let answer = single_count(
            &ctx,
            &format!(
                "SELECT (SELECT {aggregate} FROM pg_catalog.pg_class c \
                 WHERE c.relnamespace = n.oid {MATCHES_NOTHING}) \
                 FROM pg_catalog.pg_namespace n WHERE n.nspname = 'pg_catalog'"
            ),
        )
        .await?;
        assert_eq!(
            answer,
            Some(0),
            "{aggregate} over an empty set is 0 in PostgreSQL, not NULL"
        );
    }
    Ok(())
}

/// Every other aggregate over an empty set is still NULL.
///
/// The other half of the fix: coalescing indiscriminately would make `max` of
/// nothing report 0, which is a different wrong answer.
#[tokio::test(flavor = "multi_thread")]
async fn other_aggregates_over_no_matching_rows_are_still_null() -> DFResult<()> {
    let ctx = base_ctx().await?;
    for aggregate in [
        "max(c.relname)",
        "min(c.relname)",
        "string_agg(c.relname, ',')",
        "sum(c.relnatts)",
    ] {
        let is_null = projects_null(
            &ctx,
            &format!(
                "SELECT (SELECT {aggregate} FROM pg_catalog.pg_class c \
                 WHERE c.relnamespace = n.oid {MATCHES_NOTHING}) \
                 FROM pg_catalog.pg_namespace n WHERE n.nspname = 'pg_catalog'"
            ),
        )
        .await?;
        assert!(is_null, "{aggregate} over an empty set must stay NULL");
    }
    Ok(())
}

/// A count that DOES match still counts, and agrees with a grouped join.
///
/// Derived separately rather than compared against itself: a rewrite that
/// returned 0 for everything would satisfy the first test and fail this one.
#[tokio::test(flavor = "multi_thread")]
async fn a_correlated_count_matches_the_same_count_written_as_a_join() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let correlated = single_count(
        &ctx,
        "SELECT (SELECT count(*) FROM pg_catalog.pg_class c WHERE c.relnamespace = n.oid) \
         FROM pg_catalog.pg_namespace n WHERE n.nspname = 'pg_catalog'",
    )
    .await?;
    let grouped = single_count(
        &ctx,
        "SELECT count(*) FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'pg_catalog'",
    )
    .await?;
    assert_eq!(correlated, grouped, "the two spellings must agree");
    assert!(
        correlated.is_some_and(|n| n > 0),
        "pg_catalog holds relations, so this is not the empty case"
    );
    Ok(())
}

/// A count wrapped in a wider expression is deliberately left alone.
///
/// `count(*) + 1` over an empty set is 1 in `PostgreSQL`, and coalescing the
/// join's NULL to 0 does not produce that. The fix is narrow on purpose, and
/// this records the shape it does not claim to handle so the limit is visible
/// rather than discovered.
#[tokio::test(flavor = "multi_thread")]
async fn a_count_inside_a_wider_expression_is_not_claimed() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let answer = single_count(
        &ctx,
        &format!(
            "SELECT (SELECT count(*) + 1 FROM pg_catalog.pg_class c \
             WHERE c.relnamespace = n.oid {MATCHES_NOTHING}) \
             FROM pg_catalog.pg_namespace n WHERE n.nspname = 'pg_catalog'"
        ),
    )
    .await?;
    assert_eq!(
        answer, None,
        "still NULL - PostgreSQL says 1, and this fix does not reach that shape"
    );
    Ok(())
}
