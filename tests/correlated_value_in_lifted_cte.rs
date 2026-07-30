//! A correlated *value* -- an outer column used as data rather than as a join
//! predicate -- must survive being lifted into a CTE.
//!
//! `scalar_to_cte` models correlation as equality predicates lifted out of the
//! sub-query's WHERE and turned into the LEFT JOIN's ON clause. An outer column
//! used anywhere else in the sub-query (as a table function's argument, as an
//! array subscript, inside a projection expression) is not a predicate, so
//! nothing carried it across: the body kept naming an alias that the CTE, once
//! detached, could no longer see.
//!
//! psql's `\d+ <table>` is the query that found this. Its storage-options
//! column reads a toast relation's `reloptions` through `unnest`, correlated to
//! the outer `tc` alias:
//!
//! ```sql
//! pg_catalog.array_to_string(
//!   c.reloptions || array(SELECT 'toast.' || x
//!                         FROM pg_catalog.unnest(tc.reloptions) x), ', ')
//! ```
//!
//! The rewriter now recognises that shape and declines to lift, leaving the
//! sub-query for the planner rather than emitting a CTE that cannot resolve.
//! Separately, a set-returning function's output column is named after its
//! table alias, as `PostgreSQL` names it.
//!
//! Most of what follows guards the correlated sub-queries that ALREADY worked.
//! A lifted CTE naming a column it cannot see fails loudly and is easy to
//! notice; one that joins on a non-unique key returns the wrong number of rows
//! silently. The row-count and value assertions below are aimed at the silent
//! failure, which is the one a change to this rewriter is likely to introduce.

use datafusion::error::Result as DFResult;
use datafusion::prelude::SessionContext;
use datafusion_pg_catalog::scalar_to_cte::rewrite_subquery_as_cte;
use datafusion_pg_catalog::session::execute_sql;

mod common;
use common::base_ctx;

/// Run `sql` and return how many rows it produced.
async fn row_count(ctx: &SessionContext, sql: &str) -> DFResult<usize> {
    let (batches, _schema) = execute_sql(ctx, sql, None, None).await?;
    Ok(batches
        .iter()
        .map(arrow::array::RecordBatch::num_rows)
        .sum())
}

/// The number of rows in a table, as a baseline a correlated sub-query over it
/// must not change.
async fn table_row_count(ctx: &SessionContext, table: &str) -> DFResult<usize> {
    row_count(ctx, &format!("SELECT 1 FROM pg_catalog.{table}")).await
}

/// A sub-query that would still name an outer alias once detached is left
/// alone, not turned into a CTE that cannot resolve it.
///
/// This is psql's storage-options shape. Declining is not a fix on its own --
/// the planner then rejects the correlated sub-query for its own reasons -- but
/// the rewriter must not be the thing that emits unresolvable SQL.
#[test]
fn a_subquery_naming_an_outer_alias_as_a_value_is_not_lifted() {
    let rewritten = rewrite_subquery_as_cte(
        "SELECT c.relname, \
         array(SELECT 'toast.' || x FROM unnest(tc.reloptions) x) \
         FROM pg_class c LEFT JOIN pg_class tc ON (c.reltoastrelid = tc.oid)",
    );
    assert!(
        !rewritten.contains("__cte"),
        "the sub-query reads tc.reloptions, which a CTE could not see: {rewritten}"
    );
}

/// A correlated sub-query whose outer reference IS an equality predicate is
/// still lifted; declining above must not have turned the rewriter off.
#[test]
fn a_subquery_correlated_by_a_predicate_is_still_lifted() {
    let rewritten = rewrite_subquery_as_cte(
        "SELECT n.nspname, (SELECT max(c.relname) FROM pg_class c \
         WHERE c.relnamespace = n.oid) FROM pg_namespace n",
    );
    assert!(
        rewritten.contains("__cte1"),
        "an equality-correlated sub-query should still become a CTE: {rewritten}"
    );
}

/// A set-returning function's single column answers to the table alias, the way
/// `PostgreSQL` names it.
///
/// `FROM unnest(...) x` makes `x` both the relation and its column in
/// `PostgreSQL`; `DataFusion` names the column after the expression instead, so
/// every query written the `PostgreSQL` way failed to bind its own reference.
#[tokio::test(flavor = "multi_thread")]
async fn a_set_returning_function_column_answers_to_its_alias() -> DFResult<()> {
    let ctx = base_ctx().await?;

    let unnested = row_count(
        &ctx,
        "SELECT x FROM pg_catalog.unnest(ARRAY['toast.a', 'toast.b']) x",
    )
    .await?;
    assert_eq!(unnested, 2, "one row per array element");

    let series = row_count(&ctx, "SELECT s FROM pg_catalog.generate_series(1, 3) s").await?;
    assert_eq!(series, 3, "one row per generated value");

    Ok(())
}

/// An explicit column list on the alias is left as written.
///
/// The rewrite above only supplies a name where `PostgreSQL` would imply one; a
/// query that names its own column must keep that name.
#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_column_alias_is_not_overwritten() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let rows = row_count(
        &ctx,
        "SELECT element FROM pg_catalog.unnest(ARRAY['a', 'b']) AS x (element)",
    )
    .await?;
    assert_eq!(rows, 2, "the query's own column name must still bind");
    Ok(())
}

/// An ARRAY(sub-query) over many rows works when it is not correlated.
///
/// Records the boundary the test above sits on: gathering many rows into an
/// array is supported, and it is the correlation that the planner objects to.
#[tokio::test(flavor = "multi_thread")]
async fn an_uncorrelated_array_subquery_over_many_rows_works() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let rows = row_count(
        &ctx,
        "SELECT array(SELECT n.nspname FROM pg_catalog.pg_namespace n)",
    )
    .await?;
    assert_eq!(rows, 1, "an array of every namespace name is one row");
    Ok(())
}

/// A correlated scalar sub-query returns exactly one row per outer row, whether
/// or not the sub-query matches anything.
///
/// This is the row-multiplication guard: if the rewriter pulls the outer
/// relation into the CTE and joins back on a key that is not unique, the outer
/// query gains duplicate rows and every count below moves.
#[tokio::test(flavor = "multi_thread")]
async fn a_correlated_scalar_keeps_one_row_per_outer_row() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let namespaces = table_row_count(&ctx, "pg_namespace").await?;

    // Correlated and matching: every namespace has classes.
    let matching = row_count(
        &ctx,
        "SELECT n.nspname, (SELECT max(c.relname) FROM pg_catalog.pg_class c \
         WHERE c.relnamespace = n.oid) FROM pg_catalog.pg_namespace n",
    )
    .await?;
    assert_eq!(
        matching, namespaces,
        "a correlated scalar added or lost rows"
    );

    // Correlated and matching nothing: still one row per outer row, with NULL.
    let empty = row_count(
        &ctx,
        "SELECT n.nspname, (SELECT max(c.relname) FROM pg_catalog.pg_class c \
         WHERE c.relnamespace = n.oid AND c.relname = '__no_such_relation__') \
         FROM pg_catalog.pg_namespace n",
    )
    .await?;
    assert_eq!(
        empty, namespaces,
        "an empty correlated scalar changed the row count"
    );

    Ok(())
}

/// Two correlated scalars in one projection stay one row per outer row.
///
/// Each lifts its own CTE and its own LEFT JOIN, so this is where a
/// row-multiplying join shows up twice over.
#[tokio::test(flavor = "multi_thread")]
async fn two_correlated_scalars_keep_one_row_per_outer_row() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let namespaces = table_row_count(&ctx, "pg_namespace").await?;

    let rows = row_count(
        &ctx,
        "SELECT n.nspname, \
         (SELECT max(c.relname) FROM pg_catalog.pg_class c WHERE c.relnamespace = n.oid), \
         (SELECT min(t.typname) FROM pg_catalog.pg_type t WHERE t.typnamespace = n.oid) \
         FROM pg_catalog.pg_namespace n",
    )
    .await?;
    assert_eq!(
        rows, namespaces,
        "two correlated scalars changed the row count"
    );
    Ok(())
}

/// A correlated scalar's VALUES are what the sub-query computes, not just the
/// right number of rows.
///
/// A join that multiplied rows inside the CTE would leave the count intact and
/// the aggregate wrong, so the count guards above are not enough on their own.
///
/// Uses `max()` rather than `count()`: the LEFT JOIN this rewrite builds hands an
/// outer row with no match a NULL, which is right for `max()` and wrong for
/// `count()` (`PostgreSQL` counts an empty set as 0). That difference is real but
/// separate -- see task 256 -- and using `max()` keeps this test measuring row
/// multiplication rather than the empty-set value.
#[tokio::test(flavor = "multi_thread")]
async fn a_correlated_scalar_computes_the_same_value_as_a_grouped_join() -> DFResult<()> {
    let ctx = base_ctx().await?;

    let correlated = "SELECT n.nspname, \
         (SELECT max(c.relname) FROM pg_catalog.pg_class c WHERE c.relnamespace = n.oid) AS latest \
         FROM pg_catalog.pg_namespace n ORDER BY n.nspname";
    let grouped = "SELECT n.nspname, max(c.relname) AS latest \
         FROM pg_catalog.pg_namespace n \
         LEFT JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid \
         GROUP BY n.nspname ORDER BY n.nspname";

    let (correlated_batches, _) = execute_sql(&ctx, correlated, None, None).await?;
    let (grouped_batches, _) = execute_sql(&ctx, grouped, None, None).await?;

    let render = |batches: &[arrow::array::RecordBatch]| {
        arrow::util::pretty::pretty_format_batches(batches)
            .expect("batches render")
            .to_string()
    };
    assert_eq!(
        render(&correlated_batches),
        render(&grouped_batches),
        "the correlated aggregate disagrees with the same aggregate as a grouped join"
    );
    Ok(())
}

/// An inner alias that shadows an outer one is the sub-query's own, not a
/// correlated reference.
///
/// psql's `\d <table>` writes exactly this: an inner `c` for `pg_collation`
/// under an outer `c` for `pg_class`. Treating the inner `c` as correlated would
/// drag the wrong relation into the CTE.
#[tokio::test(flavor = "multi_thread")]
async fn an_inner_alias_shadowing_an_outer_one_is_not_correlated() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let classes = row_count(
        &ctx,
        "SELECT 1 FROM pg_catalog.pg_class c WHERE c.relname = 'pg_class'",
    )
    .await?;

    let rows = row_count(
        &ctx,
        "SELECT c.relname, \
         (SELECT max(c.collname) FROM pg_catalog.pg_collation c) \
         FROM pg_catalog.pg_class c WHERE c.relname = 'pg_class'",
    )
    .await?;
    assert_eq!(
        rows, classes,
        "a shadowed inner alias changed the row count"
    );
    Ok(())
}
