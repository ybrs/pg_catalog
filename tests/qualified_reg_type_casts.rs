//! A literal cast to `regclass` or `regtype`, written the way psql writes it.
//!
//! `PostgreSQL` accepts both `'pg_class'::regclass` and the schema-qualified
//! `'pg_class'::pg_catalog.regclass`, and psql writes the qualified form:
//! `\dx` filters on `'pg_catalog.pg_extension'::pg_catalog.regclass` and
//! `\df n` on `'pg_catalog.trigger'::pg_catalog.regtype`.
//!
//! Both failed. The qualified spelling was flattened to TEXT before the passes
//! that resolve such a literal to an OID could see it, so the planner was left
//! comparing the string `'pg_catalog.pg_extension'` against an integer column
//! and gave up in `simplify_expressions`.
//!
//! The flattening is right for everything else, which is the subtlety here and
//! the reason the last two tests exist: `\d` casts a COLUMN with
//! `conrelid::pg_catalog.regclass`, where no name lookup applies and the
//! planner rejects the type outright. So the rule is about the OPERAND, not the
//! type - a string literal is a name to resolve, anything else is a value to
//! re-type. Narrowing it any other way broke `\d`, `\d+`, `\dX` and `\dP` while
//! fixing `\dx`.

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
            return Ok(Some(
                arrow::array::as_primitive_array::<arrow::datatypes::Int64Type>(&column).value(0),
            ));
        }
    }
    Ok(None)
}

/// Every spelling of a relation-name cast resolves to the same OID.
///
/// Two axes vary independently and both have to work: the TYPE may be bare or
/// `pg_catalog`-qualified, and so may the NAME inside the literal.
#[tokio::test(flavor = "multi_thread")]
async fn a_relation_name_resolves_however_the_cast_is_spelled() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let expected = single_oid(&ctx, "SELECT 'pg_class'::regclass")
        .await?
        .expect("pg_class is in the catalog");
    for sql in [
        "SELECT 'pg_class'::pg_catalog.regclass",
        "SELECT 'pg_catalog.pg_class'::regclass",
        "SELECT 'pg_catalog.pg_class'::pg_catalog.regclass",
    ] {
        assert_eq!(
            single_oid(&ctx, sql).await?,
            Some(expected),
            "{sql} should resolve to the same oid as the bare spelling"
        );
    }
    Ok(())
}

/// And a type-name cast resolves to that type's OID.
#[tokio::test(flavor = "multi_thread")]
async fn a_type_name_resolves_however_the_cast_is_spelled() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let expected = single_oid(&ctx, "SELECT 'trigger'::regtype")
        .await?
        .expect("the trigger type is in the catalog");
    for sql in [
        "SELECT 'trigger'::pg_catalog.regtype",
        "SELECT 'pg_catalog.trigger'::regtype",
        "SELECT 'pg_catalog.trigger'::pg_catalog.regtype",
    ] {
        assert_eq!(
            single_oid(&ctx, sql).await?,
            Some(expected),
            "{sql} should resolve to the same oid as the bare spelling"
        );
    }
    Ok(())
}

/// The resolved OID is the one the catalog actually holds.
///
/// The tests above only prove the spellings agree with each other; they would
/// pass just as well if every one of them resolved to the same wrong number.
#[tokio::test(flavor = "multi_thread")]
async fn the_resolved_oids_are_the_catalogs_own() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let relation = single_oid(
        &ctx,
        "SELECT oid FROM pg_catalog.pg_class WHERE relname = 'pg_class'",
    )
    .await?;
    assert_eq!(
        single_oid(&ctx, "SELECT 'pg_catalog.pg_class'::pg_catalog.regclass").await?,
        relation
    );

    let kind = single_oid(
        &ctx,
        "SELECT oid FROM pg_catalog.pg_type WHERE typname = 'trigger'",
    )
    .await?;
    assert_eq!(
        single_oid(&ctx, "SELECT 'pg_catalog.trigger'::pg_catalog.regtype").await?,
        kind
    );
    Ok(())
}

/// psql's `\dx` filter runs.
#[tokio::test(flavor = "multi_thread")]
async fn the_extension_description_join_psql_writes_runs() -> DFResult<()> {
    let ctx = base_ctx().await?;
    execute_sql(
        &ctx,
        "SELECT e.extname, d.description \
         FROM pg_catalog.pg_extension e \
         LEFT JOIN pg_catalog.pg_description d \
           ON d.objoid = e.oid \
          AND d.classoid = 'pg_catalog.pg_extension'::pg_catalog.regclass",
        None,
        None,
    )
    .await?;
    Ok(())
}

/// psql's `\df n` filter runs.
#[tokio::test(flavor = "multi_thread")]
async fn the_function_return_type_filter_psql_writes_runs() -> DFResult<()> {
    let ctx = base_ctx().await?;
    execute_sql(
        &ctx,
        "SELECT p.proname FROM pg_catalog.pg_proc p \
         WHERE p.prorettype <> 'pg_catalog.trigger'::pg_catalog.regtype",
        None,
        None,
    )
    .await?;
    Ok(())
}

/// A COLUMN cast to a qualified object-identifier type still plans.
///
/// The regression guard. Resolving names is only right for literals; a column
/// has no name to look up, and leaving `pg_catalog.regclass` on one reaches the
/// planner as "Unsupported SQL type `pg_catalog.regclass`". `\d`, `\d+`, `\dX`
/// and `\dP` all cast a column this way, and all four broke when this rule was
/// written about the type instead of the operand.
#[tokio::test(flavor = "multi_thread")]
async fn a_column_cast_to_a_qualified_identifier_type_still_plans() -> DFResult<()> {
    let ctx = base_ctx().await?;
    for sql in [
        "SELECT conrelid::pg_catalog.regclass FROM pg_catalog.pg_constraint LIMIT 1",
        "SELECT c.oid::pg_catalog.regclass FROM pg_catalog.pg_class c LIMIT 1",
        "SELECT atttypid::pg_catalog.regtype FROM pg_catalog.pg_attribute LIMIT 1",
    ] {
        execute_sql(&ctx, sql, None, None)
            .await
            .unwrap_or_else(|error| panic!("{sql} should plan: {error}"));
    }
    Ok(())
}
