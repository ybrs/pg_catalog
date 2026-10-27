//! A correlation written without the outer table's name, which is how psql
//! writes `\dp`.
//!
//! The CTE lifter finds a correlation by matching `alias.column` in a
//! sub-query's WHERE clause against the aliases the enclosing FROM clause
//! binds. `PostgreSQL` does not require the alias, and psql does not write it:
//! `\dp`'s policies column sends
//!
//! ```sql
//! ARRAY(SELECT ... || pg_catalog.array_to_string(ARRAY(
//!         SELECT rolname FROM pg_catalog.pg_roles WHERE oid = ANY (polroles)), ', ')
//!       FROM pg_catalog.pg_policy pol WHERE polrelid = c.oid)
//! ```
//!
//! where `polroles` belongs to `pg_policy`, the relation ENCLOSING the
//! sub-query over `pg_roles`. Read as written, that inner sub-query looks
//! uncorrelated - and an uncorrelated sub-query inside a lifted body is
//! deliberately not lifted (see `nested_array_subquery_lift.rs`), so it stayed
//! a multi-row scalar sub-query and the analyzer rejected the whole statement
//! with "Correlated scalar subquery must be aggregated to return at most one
//! row".
//!
//! Deciding that a bare `polroles` belongs to the outer relation needs to know
//! which relation declares which column, which `catalog_relation_columns`
//! answers from the embedded catalog. This file is what that decision is worth.
//!
//! THE ASSERTIONS COUNT ELEMENTS. A lift that fails to resolve says so loudly;
//! a lift that collapses rows returns a plausible string of the wrong length.
//! So every behavioural test here compares the number of elements in the array
//! against a count derived some other way - a plain grouped join, or the raw
//! array column read straight out of the catalog - rather than asserting that
//! the query ran.

use std::collections::HashMap;

use arrow::array::Array;
use datafusion::error::Result as DFResult;
use datafusion::prelude::SessionContext;
use datafusion_pg_catalog::scalar_to_cte::rewrite_subquery_as_cte;
use datafusion_pg_catalog::session::{execute_sql, rewrite_filters};

mod common;
use common::base_ctx;

/// The policies column of psql 17.4's `\dp`, verbatim apart from the `%s`
/// column titles the client substitutes.
///
/// Kept whole rather than reduced to its correlated core: the reduction is what
/// the tests below assert on, and this is the thing a user actually types.
const PSQL_LIST_PRIVILEGES_POLICIES: &str = r#"SELECT n.nspname as "Schema",
  c.relname as "Name",
  pg_catalog.array_to_string(ARRAY(
    SELECT polname
    || CASE WHEN NOT polpermissive THEN
       E' (RESTRICTIVE)'
       ELSE '' END
    || CASE WHEN polcmd != '*' THEN
           E' (' || polcmd::pg_catalog.text || E'):'
       ELSE E':'
       END
    || CASE WHEN polqual IS NOT NULL THEN
           E'\n  (u): ' || pg_catalog.pg_get_expr(polqual, polrelid)
       ELSE E''
       END
    || CASE WHEN polwithcheck IS NOT NULL THEN
           E'\n  (c): ' || pg_catalog.pg_get_expr(polwithcheck, polrelid)
       ELSE E''
       END    || CASE WHEN polroles <> '{0}' THEN
           E'\n  to: ' || pg_catalog.array_to_string(
               ARRAY(
                   SELECT rolname
                   FROM pg_catalog.pg_roles
                   WHERE oid = ANY (polroles)
                   ORDER BY 1
               ), E', ')
       ELSE E''
       END
    FROM pg_catalog.pg_policy pol
    WHERE polrelid = c.oid), E'\n')
    AS "Policies"
FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind IN ('r','v','m','S','f','p')"#;

/// A nested `ARRAY(SELECT ... ARRAY(SELECT ...))` whose inner correlation is
/// written WITHOUT the outer table's name.
///
/// The same shape `nested_array_subquery_lift.rs` covers, with one deliberate
/// difference: its inner sub-query correlates on `a.attrelid = c.oid`, and this
/// one writes `a.attrelid = oid`. `pg_attribute` declares no `oid` column and
/// `pg_class` does, so `PostgreSQL` resolves the bare name to the enclosing
/// relation, and so must the lifter.
const NESTED_ARRAY_CORRELATED_BY_A_BARE_COLUMN: &str =
    "SELECT n.nspname, pg_catalog.array_to_string(ARRAY(\
     SELECT c.relname || ':' || pg_catalog.array_to_string(ARRAY(\
       SELECT a.attname FROM pg_catalog.pg_attribute a WHERE a.attrelid = oid), ',') \
     FROM pg_catalog.pg_class c WHERE c.relnamespace = n.oid), '|') \
     FROM pg_catalog.pg_namespace n";

/// The `oid = ANY(<outer array column>)` correlation `\dp` writes, over
/// relations the fixture catalog actually populates.
///
/// `pg_policy` is empty in the embedded catalog, so the policies query proves
/// only that the statement plans. This reproduces its exact shape - a bare
/// array column belonging to the enclosing relation, matched with `= ANY` -
/// against `pg_proc.proargtypes`, where there are rows to count.
const ANY_CORRELATION_BY_A_BARE_ARRAY_COLUMN: &str =
    "SELECT p.proname, pg_catalog.array_to_string(ARRAY(\
     SELECT t.typname FROM pg_catalog.pg_type t WHERE t.oid = ANY (proargtypes)), ',') \
     FROM pg_catalog.pg_proc p WHERE p.proname IN \
     ('int48pl', 'texticregexeq', 'timestamp_pl_interval', 'numeric_power')";

/// The functions [`ANY_CORRELATION_BY_A_BARE_ARRAY_COLUMN`] selects, named here
/// so the expectation query below reads the same rows.
const ARGUMENT_TYPE_PROBE_FUNCTIONS: &[&str] = &[
    "int48pl",
    "texticregexeq",
    "timestamp_pl_interval",
    "numeric_power",
];

/// Run `sql` and return its first two columns as a name -> text map.
///
/// Every query here projects a name and one built-up string, so this is the
/// only result shape the file needs. A NULL string becomes the empty string,
/// which is what an array with no elements renders as.
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
/// The outer array of [`NESTED_ARRAY_CORRELATED_BY_A_BARE_COLUMN`] has to hold
/// this many elements. Deriving it without a sub-query is what makes the
/// comparison worth anything: a lifter that collapses rows agrees with itself
/// but not with this.
async fn relations_per_namespace(ctx: &SessionContext) -> DFResult<HashMap<String, usize>> {
    count_by_name(
        ctx,
        "SELECT n.nspname, count(*) FROM pg_catalog.pg_namespace n \
         JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid GROUP BY n.nspname",
    )
    .await
}

/// How many columns each relation holds, computed with a plain grouped join.
///
/// The inner array of [`NESTED_ARRAY_CORRELATED_BY_A_BARE_COLUMN`] - the one
/// whose correlation is the bare name - has to hold this many elements per
/// relation.
async fn columns_per_relation(ctx: &SessionContext) -> DFResult<HashMap<String, usize>> {
    count_by_name(
        ctx,
        "SELECT c.relname, count(*) FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid GROUP BY c.relname",
    )
    .await
}

/// Run a `name, count(*)` query and return its rows as a map.
async fn count_by_name(ctx: &SessionContext, sql: &str) -> DFResult<HashMap<String, usize>> {
    let (batches, _schema) = execute_sql(ctx, sql, None, None).await?;
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

/// How many DISTINCT type oids each probe function takes, read straight out of
/// `pg_proc.proargtypes`.
///
/// This is the oracle for [`ANY_CORRELATION_BY_A_BARE_ARRAY_COLUMN`], and it is
/// deliberately computed in Rust from the raw array column rather than by
/// another `= ANY` query: `oid = ANY(proargtypes)` matches one `pg_type` row per
/// DISTINCT oid, so a function taking `(int4, int4)` must yield one element and
/// one taking `(int4, int8)` two. A count that just used the array's length
/// would agree with a rewrite that lost the deduplication.
async fn distinct_argument_type_counts(ctx: &SessionContext) -> DFResult<HashMap<String, usize>> {
    let quoted = ARGUMENT_TYPE_PROBE_FUNCTIONS
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let (batches, _schema) = execute_sql(
        ctx,
        &format!(
            "SELECT p.proname, p.proargtypes FROM pg_catalog.pg_proc p \
             WHERE p.proname IN ({quoted})"
        ),
        None,
        None,
    )
    .await?;

    let mut out = HashMap::new();
    for batch in &batches {
        let names = arrow::array::as_string_array(batch.column(0));
        let argument_types = arrow::array::as_list_array(batch.column(1));
        for row in 0..batch.num_rows() {
            let oids = argument_types.value(row);
            // `_oid` columns are stored as Int64 lists (see db_table::map_pg_type):
            // oid values can exceed Int32.
            let oids = arrow::array::as_primitive_array::<arrow::datatypes::Int64Type>(&oids);
            let distinct: std::collections::HashSet<i64> = oids.iter().flatten().collect();
            out.insert(names.value(row).to_string(), distinct.len());
        }
    }
    Ok(out)
}

/// The number of elements `array_to_string` produced, for a `separator` that
/// cannot occur inside an element.
///
/// An empty string is zero elements, not one, which is what an array with no
/// rows renders as.
fn element_count(joined: &str, separator: char) -> usize {
    if joined.is_empty() {
        0
    } else {
        joined.split(separator).count()
    }
}

/// `\dp`'s policies column plans and answers, one row per relation.
///
/// The reported failure, end to end. The fixture catalog declares no policies,
/// so every value is empty - which is why this asserts the row shape only and
/// the element counts are asserted by the tests below, on relations that have
/// rows.
#[tokio::test(flavor = "multi_thread")]
async fn the_policies_column_of_list_privileges_answers_for_every_relation() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let expected_relations = count_by_name(
        &ctx,
        "SELECT 'relations', count(*) FROM pg_catalog.pg_class c \
         WHERE c.relkind IN ('r', 'v', 'm', 'S', 'f', 'p')",
    )
    .await?;
    let expected = expected_relations["relations"];
    assert!(
        expected > 0,
        "the fixture catalog holds relations of the kinds \\dp lists"
    );

    let (batches, _schema) = execute_sql(&ctx, PSQL_LIST_PRIVILEGES_POLICIES, None, None).await?;
    let rows: usize = batches
        .iter()
        .map(arrow::array::RecordBatch::num_rows)
        .sum();
    assert_eq!(
        rows, expected,
        "\\dp reports one row per relation, not a row per policy"
    );
    Ok(())
}

/// An `= ANY` correlation on a bare array column returns one element per
/// DISTINCT oid in that array.
///
/// The `\dp` shape with rows behind it. The count is the point: lifting it as
/// though it were uncorrelated would join every `pg_type` row to every
/// function, so the failure this guards against is an array that is far too
/// long rather than one that is missing.
#[tokio::test(flavor = "multi_thread")]
async fn an_any_correlation_on_a_bare_array_column_holds_one_element_per_distinct_oid(
) -> DFResult<()> {
    let ctx = base_ctx().await?;
    let expected = distinct_argument_type_counts(&ctx).await?;
    assert_eq!(
        expected.len(),
        ARGUMENT_TYPE_PROBE_FUNCTIONS.len(),
        "every probe function should exist in the fixture catalog: {expected:?}"
    );
    assert!(
        expected.values().any(|count| *count == 2),
        "at least one probe function takes two different types, or the \
         deduplication this asserts is untested: {expected:?}"
    );

    let actual = text_pairs(&ctx, ANY_CORRELATION_BY_A_BARE_ARRAY_COLUMN).await?;
    for (function, distinct_types) in &expected {
        let joined = actual
            .get(function)
            .unwrap_or_else(|| panic!("no row for function {function}"));
        assert_eq!(
            element_count(joined, ','),
            *distinct_types,
            "{function} takes {distinct_types} distinct argument types, so its \
             array needs that many elements, not {joined:?}"
        );
    }
    Ok(())
}

/// A nested array correlated by a bare column holds one element per inner row.
///
/// This is the `\dp` structure - a sub-query inside a LIFTED body, correlated
/// to the body's own relation without naming it. Before the qualifier was
/// resolved, this query failed outright; the risk in fixing it is not that it
/// fails again but that it succeeds with the wrong number of elements, since
/// the body's rows feed an aggregate.
#[tokio::test(flavor = "multi_thread")]
async fn a_nested_array_correlated_by_a_bare_column_holds_one_element_per_inner_row() -> DFResult<()>
{
    let ctx = base_ctx().await?;
    let expected = relations_per_namespace(&ctx).await?;
    assert!(
        expected.len() >= 3,
        "the fixture catalog should have several populated namespaces: {expected:?}"
    );

    let actual = text_pairs(&ctx, NESTED_ARRAY_CORRELATED_BY_A_BARE_COLUMN).await?;
    for (namespace, relation_count) in &expected {
        let joined = actual
            .get(namespace)
            .unwrap_or_else(|| panic!("no row for namespace {namespace}"));
        assert_eq!(
            element_count(joined, '|'),
            *relation_count,
            "namespace {namespace} holds {relation_count} relations, so its array \
             needs that many elements"
        );
    }
    Ok(())
}

/// Each element of that nested array holds its own relation's columns.
///
/// The outer count above would still pass if every element carried the same
/// wrongly shared value, which is what a cross join produces. This checks the
/// INNER array - the one whose correlation is the bare name - against the
/// column count computed by a plain grouped join.
#[tokio::test(flavor = "multi_thread")]
async fn each_element_of_that_nested_array_holds_its_own_relations_columns() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let expected = columns_per_relation(&ctx).await?;
    let actual = text_pairs(&ctx, NESTED_ARRAY_CORRELATED_BY_A_BARE_COLUMN).await?;

    let pg_catalog_elements = actual
        .get("pg_catalog")
        .expect("the pg_catalog namespace is always populated");
    let mut checked = 0_usize;
    for element in pg_catalog_elements.split('|') {
        let (relation, columns) = element
            .split_once(':')
            .unwrap_or_else(|| panic!("element {element:?} should read as relname:columns"));
        let expected_columns = expected
            .get(relation)
            .unwrap_or_else(|| panic!("no column count for relation {relation}"));
        assert_eq!(
            element_count(columns, ','),
            *expected_columns,
            "{relation} has {expected_columns} columns, so its element needs that \
             many names, not {columns:?}"
        );
        checked += 1;
    }
    assert!(
        checked > 10,
        "pg_catalog holds many relations, so this checked too few: {checked}"
    );
    Ok(())
}

/// Writing the qualifier by hand changes nothing.
///
/// The rewrite's whole claim is that it recovers the spelling the client left
/// out, so the two spellings have to reach the planner as the same statement.
/// Comparing them is a stronger assertion than matching either against a
/// literal, and it does not have to be rewritten when the lifter's output
/// formatting changes.
///
/// Run through the whole pipeline rather than the lifter alone: `ARRAY(...)`
/// only becomes a sub-query the lifter recognises once `rewrite_array_subquery`
/// has turned it into `pg_get_array(...)`, several passes earlier.
#[test]
fn a_bare_correlation_rewrites_exactly_as_the_qualified_one_does() -> DFResult<()> {
    let (bare, _aliases) = rewrite_filters(ANY_CORRELATION_BY_A_BARE_ARRAY_COLUMN)?;
    let (qualified, _aliases) = rewrite_filters(
        &ANY_CORRELATION_BY_A_BARE_ARRAY_COLUMN.replace("ANY (proargtypes)", "ANY (p.proargtypes)"),
    )?;
    assert_eq!(
        bare, qualified,
        "an unqualified correlation should reach the planner as the qualified one does"
    );
    assert!(
        bare.contains("__cte1"),
        "and both should be lifted into a CTE: {bare}"
    );
    Ok(())
}

/// A column the sub-query's OWN relation declares stays with that relation.
///
/// SQL resolves an unqualified name against the innermost scope that declares
/// it, and so must this. `oid` is declared by `pg_type` and by `pg_class`, so a
/// rewrite that reached for the enclosing relation first would silently
/// retarget the filter onto a different table's key.
#[test]
fn a_column_the_inner_relation_declares_is_not_taken_from_the_enclosing_query() {
    let rewritten = rewrite_subquery_as_cte(
        "SELECT c.relname, (SELECT t.typname FROM pg_catalog.pg_type t WHERE oid = 16) \
         FROM pg_catalog.pg_class c",
    );
    assert!(
        !rewritten.contains("c.oid = 16"),
        "oid is a pg_type column here, so it must not be read as pg_class's: {rewritten}"
    );
    assert!(
        rewritten.contains("oid = 16"),
        "and it must survive the rewrite unqualified: {rewritten}"
    );
}

/// A sub-query over a relation the embedded catalog does not declare is left
/// exactly as written.
///
/// The column map covers the catalog only - never a user table, and never a
/// view a lazy source supplies. Not knowing a relation's columns must therefore
/// mean "decline", not "that relation has no such column": the bias this whole
/// rewrite is built on is that failing to recognise a correlation leaves
/// today's behaviour, while wrongly deciding a name is an outer reference
/// produces a CTE that cannot resolve.
#[test]
fn a_subquery_over_an_unknown_relation_is_left_as_written() {
    let rewritten = rewrite_subquery_as_cte(
        "SELECT c.relname, (SELECT s.total FROM sales s WHERE relnamespace = 11) \
         FROM pg_catalog.pg_class c",
    );
    assert!(
        !rewritten.contains("c.relnamespace = 11"),
        "sales is not a relation this rewrite can resolve against, so the bare \
         column must be left alone: {rewritten}"
    );
}

/// An enclosing query that holds an unknown relation is left alone for the
/// same reason.
///
/// The guard above covers not knowing the INNER relation's columns; this covers
/// not knowing the OUTER's, which fails differently. Here `relname` could
/// belong to `sales` or to `pg_class` and nothing can tell which - so an
/// enclosing FROM clause with one unresolvable relation in it disqualifies the
/// WHOLE clause, rather than the resolvable relations in it going on to claim
/// columns that may not be theirs.
#[test]
fn a_subquery_enclosed_by_a_from_clause_holding_an_unknown_relation_is_left_as_written() {
    let rewritten = rewrite_subquery_as_cte(
        "SELECT s.region, (SELECT a.attname FROM pg_catalog.pg_attribute a \
         WHERE a.attname = relname) \
         FROM sales s JOIN pg_catalog.pg_class c ON c.oid = s.relation_oid",
    );
    assert!(
        !rewritten.contains("c.relname"),
        "sales may declare relname too, and nothing here can rule that out: {rewritten}"
    );
}

/// A column two enclosing relations both declare is left as written.
///
/// `oid` belongs to `pg_class` and to `pg_type`, and `pg_attribute` declares no
/// `oid` of its own - so the name is genuinely ambiguous, which is a question
/// about the query and not one this rewrite gets to answer. Picking either
/// relation would silently join on a different table's key.
#[test]
fn a_column_two_enclosing_relations_declare_is_left_as_written() {
    let rewritten = rewrite_subquery_as_cte(
        "SELECT c.relname, (SELECT a.attname FROM pg_catalog.pg_attribute a \
         WHERE a.attrelid = oid) \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_type t ON t.typrelid = c.oid",
    );
    assert!(
        rewritten.contains("a.attrelid = oid"),
        "oid is declared by both enclosing relations, so the sub-query's \
         predicate has to survive unqualified: {rewritten}"
    );
}
