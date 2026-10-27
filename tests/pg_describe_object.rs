//! `pg_describe_object` renders catalog objects the way `PostgreSQL` does.
//!
//! The function is what psql's `\dx+` prints - one description per `pg_depend`
//! extension row - and it did not exist, so the command failed outright.
//! Every expectation here is the bundled `PostgreSQL` 17.4's own output for the
//! same call, captured live; the four extension-membership strings are
//! byte-for-byte what `\dx+ plpgsql` prints there.

use arrow::array::Array;
use datafusion::error::Result as DFResult;
use datafusion_pg_catalog::session::execute_sql;

mod common;
use common::base_ctx;

/// Every text value of the first column, in row order.
async fn text_column(
    ctx: &datafusion::prelude::SessionContext,
    sql: &str,
) -> DFResult<Vec<String>> {
    let (batches, _schema) = execute_sql(ctx, sql, None, None).await?;
    let mut out = Vec::new();
    for batch in &batches {
        let cast = arrow::compute::cast(batch.column(0), &arrow::datatypes::DataType::Utf8)?;
        let array = arrow::array::as_string_array(&cast);
        for row in 0..batch.num_rows() {
            out.push(if array.is_null(row) {
                "NULL".to_string()
            } else {
                array.value(row).to_string()
            });
        }
    }
    Ok(out)
}

/// The exact query psql 17.4's `\dx+` sends, answering the exact strings the
/// real server prints for the plpgsql extension.
#[tokio::test(flavor = "multi_thread")]
async fn the_dx_plus_query_answers_postgresqls_exact_strings() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let descriptions = text_column(
        &ctx,
        "SELECT pg_catalog.pg_describe_object(classid, objid, 0) AS \"Object description\" \
         FROM pg_catalog.pg_depend \
         WHERE refclassid = 'pg_catalog.pg_extension'::pg_catalog.regclass \
         AND refobjid = '12761' AND deptype = 'e' ORDER BY 1",
    )
    .await?;
    assert_eq!(
        descriptions,
        vec![
            "function plpgsql_call_handler()",
            "function plpgsql_inline_handler(internal)",
            "function plpgsql_validator(oid)",
            "language plpgsql",
        ],
        "byte-for-byte what psql prints against the real server"
    );
    Ok(())
}

/// One rendering per covered class, each measured against the live server.
///
/// The values assert the FORMAT - relkind labels, argument types included for
/// functions, search-path-aware qualification - not just that something came
/// back.
#[tokio::test(flavor = "multi_thread")]
async fn each_covered_class_renders_as_postgresql_does() -> DFResult<()> {
    let ctx = base_ctx().await?;
    for (call, expected) in [
        ("1259, 1259, 0", "table pg_class"),
        // objsubid > 0 names the column of the relation.
        ("1259, 1259, 1", "column oid of table pg_class"),
        ("1247, 23, 0", "type integer"),
        ("2615, 2200, 0", "schema public"),
        ("2612, 12765, 0", "language plpgsql"),
        ("3079, 12761, 0", "extension plpgsql"),
        // Argument types render through format_type: avg(bigint), not
        // avg(int8).
        ("1255, 2100, 0", "function avg(bigint)"),
    ] {
        let rendered = text_column(
            &ctx,
            &format!("SELECT pg_catalog.pg_describe_object({call})"),
        )
        .await?;
        assert_eq!(rendered, vec![expected], "pg_describe_object({call})");
    }
    Ok(())
}

/// A covered class with an OID that matches nothing answers NULL, and an
/// uncovered class errors naming it - both exactly `PostgreSQL`'s answers, so
/// a coverage gap here fails the way the real server fails.
#[tokio::test(flavor = "multi_thread")]
async fn missing_objects_answer_null_and_uncovered_classes_error() -> DFResult<()> {
    let ctx = base_ctx().await?;
    assert_eq!(
        text_column(
            &ctx,
            "SELECT pg_catalog.pg_describe_object(1259, 999999999, 0)"
        )
        .await?,
        vec!["NULL"]
    );

    let error = execute_sql(
        &ctx,
        "SELECT pg_catalog.pg_describe_object(99999, 1, 0)",
        None,
        None,
    )
    .await
    .expect_err("an uncovered class is an error, as PostgreSQL answers it");
    assert!(
        error
            .to_string()
            .contains("unsupported object class: 99999"),
        "{error}"
    );
    Ok(())
}
