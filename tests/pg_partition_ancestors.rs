//! `pg_partition_ancestors`, which answers the relation it was handed.
//!
//! `PostgreSQL` declares it `setof regclass`: a partition's ancestor relations,
//! INCLUDING the relation itself. This catalog models no partitioning, so no
//! relation has an ancestor and the entire answer is that one row.
//!
//! It is registered as a SCALAR function for that reason. psql calls it in a
//! SELECT list, where a table function cannot be reached at all:
//!
//! ```sql
//! WHERE confrelid IN (SELECT pg_catalog.pg_partition_ancestors('16385')
//!                     UNION ALL VALUES ('16385'::pg_catalog.regclass))
//! ```
//!
//! A set-returning function that always returns exactly one row is
//! indistinguishable from a scalar one. That equivalence holds only while no
//! partitioning is modelled, which is what the last test here pins: if
//! partitioned relations are ever added, this has to become a real
//! set-returning function.
//!
//! psql 18's `\d <table>` is the query that wanted it; 17.4's does not call it.

use datafusion::error::Result as DFResult;
use datafusion::prelude::SessionContext;
use datafusion_pg_catalog::session::execute_sql;

mod common;
use common::base_ctx;

/// The single integer `sql` projects, or None when it is NULL.
async fn single_oid(ctx: &SessionContext, sql: &str) -> DFResult<Option<i64>> {
    use arrow::array::Array;

    let (batches, _schema) = execute_sql(ctx, sql, None, None).await?;
    for batch in &batches {
        if batch.num_rows() > 0 {
            let column = arrow::compute::cast(batch.column(0), &arrow::datatypes::DataType::Int64)?;
            if column.is_null(0) {
                return Ok(None);
            }
            let column = arrow::array::as_primitive_array::<arrow::datatypes::Int64Type>(&column);
            return Ok(Some(column.value(0)));
        }
    }
    Ok(None)
}

/// A relation is its own only ancestor, whichever way the OID is spelled.
///
/// psql substitutes the OID it already resolved and passes it as a string, so
/// the string spelling is the one that matters in practice; the integer form is
/// here because any other caller would write it that way.
#[tokio::test(flavor = "multi_thread")]
async fn a_relation_is_its_own_ancestor() -> DFResult<()> {
    let ctx = base_ctx().await?;
    for sql in [
        "SELECT pg_catalog.pg_partition_ancestors('16385')",
        "SELECT pg_catalog.pg_partition_ancestors(16385)",
        "SELECT pg_partition_ancestors('16385')",
    ] {
        assert_eq!(
            single_oid(&ctx, sql).await?,
            Some(16385),
            "{sql} should answer the relation it was handed"
        );
    }
    Ok(())
}

/// It answers exactly one row, which is what lets a scalar stand in for a
/// set-returning function here.
#[tokio::test(flavor = "multi_thread")]
async fn it_answers_exactly_one_row() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let (batches, _schema) = execute_sql(
        &ctx,
        "SELECT pg_catalog.pg_partition_ancestors('16385')",
        None,
        None,
    )
    .await?;
    let rows: usize = batches
        .iter()
        .map(arrow::array::RecordBatch::num_rows)
        .sum();
    assert_eq!(rows, 1);
    Ok(())
}

/// A value that is not an OID answers NULL rather than failing the query.
///
/// The rest of this catalog reads an object reference it cannot resolve as
/// nothing rather than as an error, and one unresolvable row must not take the
/// rows beside it down with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_value_that_is_not_an_oid_answers_null() -> DFResult<()> {
    let ctx = base_ctx().await?;
    assert_eq!(
        single_oid(
            &ctx,
            "SELECT pg_catalog.pg_partition_ancestors('not an oid')"
        )
        .await?,
        None
    );
    assert_eq!(
        single_oid(&ctx, "SELECT pg_catalog.pg_partition_ancestors(NULL)").await?,
        None
    );
    Ok(())
}

/// psql 18's foreign-key lookup plans and runs as written.
///
/// The whole query rather than the function alone, because the function is
/// called from a SELECT list inside an IN sub-query with a UNION ALL - the
/// position that ruled out a table function in the first place.
#[tokio::test(flavor = "multi_thread")]
async fn the_foreign_key_lookup_psql_18_writes_runs() -> DFResult<()> {
    let ctx = base_ctx().await?;
    execute_sql(
        &ctx,
        "SELECT conname, conrelid, pg_catalog.pg_get_constraintdef(oid, true) AS condef \
         FROM pg_catalog.pg_constraint c \
         WHERE confrelid IN (SELECT pg_catalog.pg_partition_ancestors('16385') \
                             UNION ALL VALUES (16385)) \
           AND contype = 'f' AND conparentid = 0 \
         ORDER BY conname",
        None,
        None,
    )
    .await?;
    Ok(())
}

/// No relation in this catalog is a partition.
///
/// The assumption the scalar registration rests on, asserted rather than
/// assumed: the day a partitioned relation appears here, this test fails and
/// says that `pg_partition_ancestors` has to become set-returning.
#[tokio::test(flavor = "multi_thread")]
async fn this_catalog_models_no_partitioning() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let (batches, _schema) = execute_sql(
        &ctx,
        "SELECT relname FROM pg_catalog.pg_class WHERE relispartition = true",
        None,
        None,
    )
    .await?;
    let partitions: usize = batches
        .iter()
        .map(arrow::array::RecordBatch::num_rows)
        .sum();
    assert_eq!(
        partitions, 0,
        "a partitioned relation means pg_partition_ancestors can return more \
         than one row, and a scalar function can no longer stand in for it"
    );
    Ok(())
}
