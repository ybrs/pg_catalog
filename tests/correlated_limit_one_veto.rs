//! A correlated `LIMIT 1` sub-query that already aggregates must be left alone.
//!
//! `rewrite_correlated_limit_one_subquery_to_max` turns
//! `(SELECT x FROM t WHERE t.k = o.k LIMIT 1)` into a grouped `max(x)`, which is
//! only valid when `x` is not already an aggregate - `max(count(*))` is not a
//! thing. `contains_aggregate_call` is the veto that spots that case.
//!
//! It used to carry its own nine-name list while the CTE lifter carried a
//! thirty-name one, and a name in neither behaved differently from a name in
//! both. Measured against a live server, with the same query shape and only the
//! aggregate changed:
//!
//! ```text
//! count(*)              answers          (listed)
//! string_agg(x, ',')    answers          (listed)
//! stddev(x)             fails to plan    (not listed)
//! var_pop(x)            fails to plan    (not listed)
//! bit_or(x)             fails to plan    (not listed)
//! ```
//!
//! Adding a single name to the list turned that name's query from failing to
//! answering, which is what identified the list rather than the aggregate as
//! the cause. Both rewriters now share one list.
//!
//! The asymmetry worth remembering: a name missing from this VETO breaks a
//! working query, while a name missing from the TRIGGER that drives
//! lateral-aggregate decorrelation only leaves a rewrite undone. The two lists
//! are not interchangeable even though both answer "is this an aggregate".

use datafusion::error::Result as DFResult;
use datafusion_pg_catalog::session::execute_sql;

mod common;
use common::base_ctx;

/// Every aggregate this catalog knows survives a correlated `LIMIT 1`.
///
/// The list deliberately spans the ones that were already handled and the ones
/// that were not, so the test would have failed before the lists were shared.
#[tokio::test(flavor = "multi_thread")]
async fn a_correlated_limit_one_subquery_over_any_aggregate_plans() -> DFResult<()> {
    let ctx = base_ctx().await?;
    for aggregate in [
        // Were in the veto's own list.
        "count(*)",
        "max(c.relname)",
        "min(c.relname)",
        "string_agg(c.relname, ',')",
        "bool_or(c.relhasindex)",
        // Were not, and failed to plan because of it.
        "stddev(c.relnatts)",
        "var_pop(c.relnatts)",
        "var_samp(c.relnatts)",
        "bit_or(c.relnatts)",
        "bit_and(c.relnatts)",
    ] {
        let sql = format!(
            "SELECT n.nspname, (SELECT {aggregate} FROM pg_catalog.pg_class c \
             WHERE c.relnamespace = n.oid LIMIT 1) \
             FROM pg_catalog.pg_namespace n WHERE n.nspname = 'pg_catalog'"
        );
        execute_sql(&ctx, &sql, None, None)
            .await
            .unwrap_or_else(|error| panic!("{aggregate} should plan: {error}"));
    }
    Ok(())
}

/// A correlated `LIMIT 1` over a plain column is still rewritten.
///
/// The veto exists to hold the rewrite back; widening it must not switch the
/// rewrite off for the shape it was written for.
#[tokio::test(flavor = "multi_thread")]
async fn a_correlated_limit_one_subquery_over_a_plain_column_still_answers() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let (batches, _schema) = execute_sql(
        &ctx,
        "SELECT n.nspname, (SELECT c.relname FROM pg_catalog.pg_class c \
         WHERE c.relnamespace = n.oid LIMIT 1) \
         FROM pg_catalog.pg_namespace n WHERE n.nspname = 'pg_catalog'",
        None,
        None,
    )
    .await?;
    let rows: usize = batches
        .iter()
        .map(arrow::array::RecordBatch::num_rows)
        .sum();
    assert_eq!(
        rows, 1,
        "one namespace was asked for, so one row comes back"
    );
    Ok(())
}
