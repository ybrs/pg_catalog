//! `ARRAY(SELECT ... FROM unnest(<array>) x)` written against an outer column.
//!
//! psql's `\d+` reads a toast relation's storage options this way:
//!
//! ```sql
//! pg_catalog.array_to_string(
//!   c.reloptions || array(SELECT 'toast.' || x
//!                         FROM pg_catalog.unnest(tc.reloptions) x), ', ')
//! ```
//!
//! and `\dy` reads an event trigger's tags the same way. Both correlate through
//! a table function's ARGUMENT rather than a predicate, so there is no key to
//! decorrelate on, and every spelling that keeps the correlation dies at
//! physical planning with "Physical plan does not support logical expression
//! `OuterReferenceColumn`".
//!
//! The way out is to stop it being a sub-query: `ARRAY(SELECT f(x) FROM
//! unnest(a) x)` is a map over `a`, so writing it as an expression over `a`
//! removes the correlation rather than moving it. Two element expressions are
//! handled, the element alone and the element concatenated with a literal, and
//! the tests below pin both what is rewritten and what is deliberately not.
//!
//! The "left alone" tests matter as much as the others. This is a narrow
//! construct rewrite, and every shape it does not understand has to reach the
//! planner unchanged rather than be mangled into something that runs and
//! answers wrongly.

use arrow::array::Array;
use datafusion::error::Result as DFResult;
use datafusion_pg_catalog::replace::rewrite_array_over_unnest_to_array_expression;
use datafusion_pg_catalog::session::execute_sql;

mod common;
use common::base_ctx;

/// The rewritten form of `sql`, for the string-level assertions.
fn rewritten(sql: &str) -> String {
    rewrite_array_over_unnest_to_array_expression(sql).expect("the fixture SQL parses")
}

/// The single string value `sql` projects, or None when it is NULL.
async fn single_text(
    ctx: &datafusion::prelude::SessionContext,
    sql: &str,
) -> DFResult<Option<String>> {
    let (batches, _schema) = execute_sql(ctx, sql, None, None).await?;
    for batch in &batches {
        if batch.num_rows() > 0 {
            let column = arrow::array::as_string_array(batch.column(0));
            return Ok((!column.is_null(0)).then(|| column.value(0).to_string()));
        }
    }
    Ok(None)
}

/// A literal prefix concatenated onto each element becomes one scalar call.
///
/// All three FROM spellings mean the same thing and all three have to be
/// recognised: sqlparser parses the bare keyword as its own node, while the
/// schema-qualified form is an ordinary table function, and psql writes both.
#[test]
fn a_prefixed_element_becomes_a_call_over_the_array() {
    for sql in [
        "SELECT array(SELECT 'toast.' || x FROM unnest(tc.reloptions) x)",
        "SELECT array(SELECT 'toast.' || x FROM pg_catalog.unnest(tc.reloptions) x)",
        "SELECT array(SELECT 'toast.' || x FROM pg_catalog.unnest(tc.reloptions) AS t(x))",
    ] {
        assert_eq!(
            rewritten(sql),
            "SELECT pg_catalog.pg_wrap_array_elements('toast.', tc.reloptions, '')",
            "not rewritten: {sql}"
        );
    }
}

/// A literal suffix is recognised on the other side too.
#[test]
fn a_suffixed_element_becomes_a_call_over_the_array() {
    assert_eq!(
        rewritten("SELECT array(SELECT x || '!' FROM unnest(a.arr) x)"),
        "SELECT pg_catalog.pg_wrap_array_elements('', a.arr, '!')"
    );
}

/// The bare element becomes the array itself, not a call.
///
/// Deliberate: routing it through the wrap function would return `text[]` for
/// every input, silently retyping an array of anything else. This is `\dy`'s
/// shape, and it is also the one shape where the rewrite is exact.
#[test]
fn a_bare_element_becomes_the_array_itself() {
    assert_eq!(
        rewritten("SELECT array(select x from pg_catalog.unnest(evttags) as t(x))"),
        "SELECT evttags"
    );
}

/// Shapes this rewrite does not understand reach the planner unchanged.
///
/// Each of these changes which elements appear, or in what order, or computes
/// something a map over the array cannot express. Rewriting any of them would
/// produce a query that runs and answers wrongly, which is worse than the
/// planner's refusal.
#[test]
fn a_subquery_that_is_not_a_plain_map_is_left_alone() {
    for sql in [
        // A filter drops elements.
        "SELECT array(SELECT x FROM unnest(a.arr) x WHERE x <> 'k')",
        // A projection this rewrite has no expression for.
        "SELECT array(SELECT upper(x) FROM unnest(a.arr) x)",
        // An ORDER BY reorders them.
        "SELECT array(SELECT x FROM unnest(a.arr) x ORDER BY 1)",
        // Not an unnest at all.
        "SELECT array(SELECT c.relname FROM pg_class c WHERE c.oid = 1)",
        // A join multiplies them.
        "SELECT array(SELECT x FROM unnest(a.arr) x, pg_class c)",
    ] {
        let out = rewritten(sql);
        assert!(
            !out.contains("pg_wrap_array_elements"),
            "should have been left alone: {sql} -> {out}"
        );
    }
}

/// The wrap function renders each element between the prefix and the suffix.
#[tokio::test(flavor = "multi_thread")]
async fn the_wrap_function_renders_every_element() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let joined = single_text(
        &ctx,
        "SELECT pg_catalog.array_to_string(\
         pg_catalog.pg_wrap_array_elements('toast.', ARRAY['a', 'b'], '!'), ', ')",
    )
    .await?;
    assert_eq!(joined.as_deref(), Some("toast.a!, toast.b!"));
    Ok(())
}

/// A NULL array answers the empty array, the way `PostgreSQL` does.
///
/// `unnest(NULL)` produces no rows and `ARRAY(<no rows>)` is `{}`, so returning
/// NULL here would differ from `PostgreSQL` in a way that shows up as a NULL
/// column in `\d+` rather than an empty one.
#[tokio::test(flavor = "multi_thread")]
async fn a_null_array_wraps_to_the_empty_array() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let joined = single_text(
        &ctx,
        "SELECT pg_catalog.array_to_string(\
         pg_catalog.pg_wrap_array_elements('toast.', NULL, ''), ', ')",
    )
    .await?;
    assert_eq!(
        joined.as_deref(),
        Some(""),
        "an empty array joins to the empty string, not to NULL"
    );
    Ok(())
}

/// psql's `\d+` storage-options expression runs, correlated outer column and all.
///
/// This is the query shape the task existed for. Before the rewrite it failed
/// at physical planning; the assertion is that it now produces a row per
/// relation rather than that it merely stops erroring.
#[tokio::test(flavor = "multi_thread")]
async fn the_storage_options_expression_runs_against_an_outer_column() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let (batches, _schema) = execute_sql(
        &ctx,
        "SELECT c.relname, pg_catalog.array_to_string(c.reloptions || array(\
         SELECT 'toast.' || x FROM pg_catalog.unnest(tc.reloptions) x), ', ') \
         FROM pg_catalog.pg_class c \
         LEFT JOIN pg_catalog.pg_class tc ON c.reltoastrelid = tc.oid \
         WHERE c.relname = 'pg_class'",
        None,
        None,
    )
    .await?;
    let rows: usize = batches
        .iter()
        .map(arrow::array::RecordBatch::num_rows)
        .sum();
    assert_eq!(rows, 1, "pg_class is one relation and describes as one row");
    Ok(())
}

/// psql's `\dy` tags expression runs, though its outer column is unqualified.
///
/// Worth its own test because it is the reason the bare-element shape is
/// handled at all: the column is written `evttags`, not `e.evttags`, so nothing
/// that reasons about correlation by matching `alias.column` can see it. Turning
/// the sub-query into an expression sidesteps that question entirely.
#[tokio::test(flavor = "multi_thread")]
async fn the_event_trigger_tags_expression_runs_with_an_unqualified_column() -> DFResult<()> {
    let ctx = base_ctx().await?;
    execute_sql(
        &ctx,
        "SELECT e.evtname, pg_catalog.array_to_string(array(\
         select x from pg_catalog.unnest(evttags) as t(x)), ', ') \
         FROM pg_catalog.pg_event_trigger e",
        None,
        None,
    )
    .await?;
    Ok(())
}

/// The bare-element shape keeps the array's element type.
///
/// `proargtypes` is a list of integers, not of text. Routing it through the
/// wrap function would hand back `text[]`, which is why that shape returns the
/// array itself instead.
#[tokio::test(flavor = "multi_thread")]
async fn a_bare_element_keeps_the_element_type() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let (_batches, schema) = execute_sql(
        &ctx,
        "SELECT array(SELECT x FROM pg_catalog.unnest(proargtypes) x) \
         FROM pg_catalog.pg_proc LIMIT 1",
        None,
        None,
    )
    .await?;
    let arrow::datatypes::DataType::List(element) = schema.field(0).data_type() else {
        panic!("expected a list, got {:?}", schema.field(0).data_type());
    };
    assert_eq!(
        element.data_type(),
        &arrow::datatypes::DataType::Int64,
        "an array of oids stays an array of integers"
    );
    Ok(())
}
