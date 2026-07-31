//! A correlated sub-query whose aggregate is `string_agg`.
//!
//! The scalar-to-CTE lift turns `(SELECT agg(...) FROM t WHERE t.k = outer.k)`
//! into a CTE that projects the join key beside the aggregate, which needs a
//! GROUP BY over that key. The rewriter decides whether to add one by looking
//! the function name up in a list of aggregates - and `string_agg` was not on
//! it. So the CTE came out as
//!
//! ```sql
//! SELECT string_agg(attname, ', ') AS col, attrelid FROM pg_attribute
//! ```
//!
//! with no GROUP BY, and planning failed with "column ... must appear in the
//! GROUP BY clause". `count` and `max` were on the list and worked, which is
//! why this went unnoticed: it looked like a correlation problem rather than a
//! missing name.
//!
//! psql's `\d <table>` reads a publication's column list with `string_agg`, so
//! this was one of the two things keeping `\d` from working at all.
//!
//! The list now holds `PostgreSQL`'s general-purpose and statistical
//! aggregates rather than only the ones some query has needed so far. The
//! tests below check the shape end to end and the values it produces, because
//! a wrongly grouped CTE can also return the right number of rows with the
//! wrong contents.

use datafusion::error::Result as DFResult;
use datafusion::prelude::SessionContext;
use datafusion_pg_catalog::scalar_to_cte::rewrite_subquery_as_cte;
use datafusion_pg_catalog::session::execute_sql;

mod common;
use common::base_ctx;

/// The single text value `sql` projects, or None when it is NULL or absent.
///
/// Casts to `Utf8` rather than downcasting, because `string_agg` does not
/// return the same string type as a plain column and a downcast would panic on
/// the difference rather than report it.
async fn single_text(ctx: &SessionContext, sql: &str) -> DFResult<Option<String>> {
    use arrow::array::Array;

    let (batches, _schema) = execute_sql(ctx, sql, None, None).await?;
    for batch in &batches {
        if batch.num_rows() > 0 {
            let column = arrow::compute::cast(batch.column(0), &arrow::datatypes::DataType::Utf8)?;
            if column.is_null(0) {
                return Ok(None);
            }
            let column = arrow::array::as_string_array(&column);
            return Ok(Some(column.value(0).to_string()));
        }
    }
    Ok(None)
}

/// The lifted CTE groups by the join key when the aggregate is `string_agg`.
#[test]
fn a_correlated_string_agg_is_grouped_by_its_join_key() {
    let rewritten = rewrite_subquery_as_cte(
        "SELECT c.relname, (SELECT string_agg(attname, ', ') FROM pg_attribute \
         WHERE attrelid = c.oid) FROM pg_class c",
    );
    assert!(
        rewritten.contains("GROUP BY attrelid"),
        "the CTE projects attrelid beside the aggregate, so it has to group by it: {rewritten}"
    );
}

/// The aggregates that already worked still do.
///
/// The fix widened a list; this pins that widening it did not disturb the
/// entries that were on it.
#[test]
fn the_aggregates_that_already_grouped_still_group() {
    for aggregate in ["count(*)", "max(attname)", "min(attname)"] {
        let rewritten = rewrite_subquery_as_cte(&format!(
            "SELECT c.relname, (SELECT {aggregate} FROM pg_attribute WHERE attrelid = c.oid) \
             FROM pg_class c"
        ));
        assert!(
            rewritten.contains("GROUP BY attrelid"),
            "{aggregate} should still be grouped: {rewritten}"
        );
    }
}

/// A projection with no aggregate at all is still not grouped.
///
/// The list is only consulted to decide whether a GROUP BY is needed; a
/// sub-query that selects a plain column must not gain one.
#[test]
fn a_subquery_without_an_aggregate_is_not_grouped() {
    let rewritten = rewrite_subquery_as_cte(
        "SELECT c.relname, (SELECT attname FROM pg_attribute WHERE attrelid = c.oid) \
         FROM pg_class c",
    );
    assert!(
        !rewritten.contains("GROUP BY"),
        "nothing here is aggregated: {rewritten}"
    );
}

/// A correlated `string_agg` runs and answers per outer row.
#[tokio::test(flavor = "multi_thread")]
async fn a_correlated_string_agg_answers_per_outer_row() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let joined = single_text(
        &ctx,
        "SELECT (SELECT string_agg(attname, ', ') FROM pg_catalog.pg_attribute \
         WHERE attrelid = c.oid) \
         FROM pg_catalog.pg_class c WHERE c.relname = 'pg_class'",
    )
    .await?
    .expect("pg_class has columns");
    assert!(
        joined.contains("relname"),
        "the aggregate should list pg_class's own columns, got {joined}"
    );
    Ok(())
}

/// It answers the same thing the sub-query answers on its own.
///
/// The value check that matters: a CTE grouped by the wrong key still returns
/// one row per outer row, but with another relation's columns folded in.
#[tokio::test(flavor = "multi_thread")]
async fn the_correlated_answer_matches_the_uncorrelated_one() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let correlated = single_text(
        &ctx,
        "SELECT (SELECT string_agg(attname, ', ') FROM pg_catalog.pg_attribute \
         WHERE attrelid = c.oid) \
         FROM pg_catalog.pg_class c WHERE c.relname = 'pg_class'",
    )
    .await?;
    let alone = single_text(
        &ctx,
        "SELECT string_agg(attname, ', ') FROM pg_catalog.pg_attribute \
         WHERE attrelid = (SELECT oid FROM pg_catalog.pg_class WHERE relname = 'pg_class')",
    )
    .await?;
    assert_eq!(
        correlated, alone,
        "the correlated form must aggregate exactly pg_class's own attributes"
    );
    Ok(())
}

/// `string_agg` answers to its `pg_catalog` name, which is the one psql writes.
#[tokio::test(flavor = "multi_thread")]
async fn string_agg_answers_to_its_qualified_name() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let joined = single_text(
        &ctx,
        "SELECT pg_catalog.string_agg(relname, ',') FROM pg_catalog.pg_class \
         WHERE relname = 'pg_class'",
    )
    .await?;
    assert_eq!(joined.as_deref(), Some("pg_class"));
    Ok(())
}
