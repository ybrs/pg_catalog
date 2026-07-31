//! psql's "walk an array's indexes and compare each element" construct.
//!
//! `\d <table>` lists a publication's columns like this:
//!
//! ```sql
//! FROM pg_catalog.generate_series(0, pg_catalog.array_upper(pr.prattrs, 1)) s,
//!      pg_catalog.pg_attribute
//! WHERE attrelid = pr.prrelid AND attnum = prattrs[s]
//! ```
//!
//! `DataFusion`'s `generate_series` demands literal bounds, and
//! `array_upper(pr.prattrs, 1)` is a per-row value, so this is rejected at
//! planning. A table function returns one static provider, so nothing about
//! decorrelation reaches it - the construct has to go.
//!
//! It says nothing more than "attnum is one of the values in prattrs", so it
//! becomes `attnum = ANY(pr.prattrs)` with the series FROM item dropped. The
//! array is taken from the BOUND rather than from the subscript because psql
//! qualifies the bound (`pr.prattrs`) and writes the subscript bare
//! (`prattrs`); the qualified spelling is the one that still resolves once the
//! sub-query is lifted into a CTE.
//!
//! The tests that assert nothing happens carry the weight here. This rewrite
//! deletes a FROM item, so a shape it misreads does not fail loudly - it
//! silently drops a join.

use datafusion::error::Result as DFResult;
use datafusion_pg_catalog::replace::rewrite_array_index_iteration_to_any_comparison;
use datafusion_pg_catalog::session::execute_sql;

mod common;
use common::base_ctx;

/// The rewritten form of `sql`.
fn rewritten(sql: &str) -> String {
    rewrite_array_index_iteration_to_any_comparison(sql).expect("the fixture SQL parses")
}

/// psql's construct becomes a membership test over the qualified array.
#[test]
fn the_publication_column_lookup_becomes_an_any_comparison() {
    assert_eq!(
        rewritten(
            "SELECT string_agg(attname, ', ') \
             FROM pg_catalog.generate_series(0, pg_catalog.array_upper(pr.prattrs::pg_catalog.int2[], 1)) s, \
                  pg_catalog.pg_attribute \
             WHERE attrelid = pr.prrelid AND attnum = prattrs[s]"
        ),
        "SELECT string_agg(attname, ', ') FROM pg_catalog.pg_attribute \
         WHERE attrelid = pr.prrelid AND attnum = ANY(pr.prattrs)"
    );
}

/// The bound may read `array_length`, which an earlier pass rewrites it to.
///
/// The pass order puts this rewrite first, so `array_upper` is what it
/// normally sees; accepting both means a reordering of the pipeline does not
/// silently switch this off.
#[test]
fn an_array_length_bound_is_recognised_too() {
    assert_eq!(
        rewritten(
            "SELECT string_agg(attname, ', ') \
             FROM generate_series(0, array_length(pr.prattrs, 1)) s, pg_attribute \
             WHERE attnum = prattrs[s]"
        ),
        "SELECT string_agg(attname, ', ') FROM pg_attribute \
         WHERE attnum = ANY(pr.prattrs)"
    );
}

/// A series used for anything besides the subscript is left in place.
///
/// The series column is a real value there, so dropping the FROM item would
/// change what the query returns rather than merely how it is computed.
#[test]
fn a_series_referenced_elsewhere_is_left_alone() {
    let sql = "SELECT s, x FROM generate_series(0, array_upper(a.arr, 1)) s, t WHERE x = arr[s]";
    assert_eq!(rewritten(sql), sql, "the projection still needs s");
}

/// Shapes that are not this construct are left untouched.
#[test]
fn other_generate_series_shapes_are_left_alone() {
    for sql in [
        // A dimension other than the first is not a flat list of elements.
        "SELECT x FROM generate_series(0, array_upper(a.arr, 2)) s, t WHERE x = arr[s]",
        // No subscript comparison to replace.
        "SELECT x FROM generate_series(0, array_upper(a.arr, 1)) s, t WHERE x > 3",
        // Dropping the only FROM item would leave nothing to select from.
        "SELECT x FROM generate_series(0, 5) s WHERE x = arr[s]",
        // A literal bound plans as written and needs no help.
        "SELECT x FROM generate_series(0, 5) s, t WHERE x > 3",
    ] {
        assert_eq!(rewritten(sql), sql, "should have been left alone: {sql}");
    }
}

/// The rewritten shape plans and runs against the catalog.
///
/// The string assertions above say the SQL is what was intended; this says the
/// planner accepts it, which is the whole point of the exercise.
#[tokio::test(flavor = "multi_thread")]
async fn the_rewritten_publication_lookup_plans() -> DFResult<()> {
    let ctx = base_ctx().await?;
    execute_sql(
        &ctx,
        "SELECT (SELECT string_agg(attname, ', ') \
         FROM pg_catalog.generate_series(0, pg_catalog.array_upper(pr.prattrs::pg_catalog.int2[], 1)) s, \
              pg_catalog.pg_attribute \
         WHERE attrelid = pr.prrelid AND attnum = prattrs[s]) \
         FROM pg_catalog.pg_publication_rel pr",
        None,
        None,
    )
    .await?;
    Ok(())
}

/// A membership test over an array column returns the right rows.
///
/// Guards the meaning the rewrite relies on: `= ANY(<array>)` has to select
/// every row whose value appears in the array, and only those.
#[tokio::test(flavor = "multi_thread")]
async fn an_any_comparison_selects_the_named_elements() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let (batches, _schema) = execute_sql(
        &ctx,
        "SELECT attname FROM pg_catalog.pg_attribute \
         WHERE attrelid = (SELECT oid FROM pg_catalog.pg_class WHERE relname = 'pg_class') \
           AND attnum = ANY(ARRAY[1, 2])",
        None,
        None,
    )
    .await?;
    let rows: usize = batches
        .iter()
        .map(arrow::array::RecordBatch::num_rows)
        .sum();
    assert_eq!(rows, 2, "two attnums were named, so two columns come back");
    Ok(())
}
