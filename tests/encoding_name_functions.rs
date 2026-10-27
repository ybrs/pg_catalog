//! Both directions of `PostgreSQL`'s fixed encoding name <-> id table.
//!
//! psql's `\dO` filters collations with
//! `collencoding IN (-1, pg_char_to_encoding(getdatabaseencoding()))`, and the
//! name-to-id direction did not exist at all, so the command failed outright.
//! The id-to-name direction existed but was broken twice over: its
//! `Exact(Int32)` signature rejected every integer literal (SQL literals are
//! `Int64`), and its body ignored the argument and returned the constant "UTF8"
//! for every input - so a round-trip test against it would have passed
//! vacuously, which is why the expectations here were MEASURED against the
//! bundled `PostgreSQL` 17.4 rather than written from the implementation.

use datafusion::error::Result as DFResult;
use datafusion_pg_catalog::session::execute_sql;

mod common;
use common::base_ctx;

/// The single text value `sql` returns (casting non-text through Arrow).
async fn single_text(ctx: &datafusion::prelude::SessionContext, sql: &str) -> DFResult<String> {
    let (batches, _schema) = execute_sql(ctx, sql, None, None).await?;
    let cast = arrow::compute::cast(batches[0].column(0), &arrow::datatypes::DataType::Utf8)?;
    let array = arrow::array::as_string_array(&cast);
    Ok(array.value(0).to_string())
}

/// The measured id -> name table, spot-checked at the edges `PostgreSQL` 17.4
/// actually answers: 0 and 41 are the table's ends, 42 and 999 are past it
/// and answer the EMPTY STRING, not an error.
#[tokio::test(flavor = "multi_thread")]
async fn encoding_ids_resolve_to_postgresqls_names() -> DFResult<()> {
    let ctx = base_ctx().await?;
    for (id, expected) in [
        (0, "SQL_ASCII"),
        (6, "UTF8"),
        (8, "LATIN1"),
        (41, "SHIFT_JIS_2004"),
        (42, ""),
        (999, ""),
        (-1, ""),
    ] {
        assert_eq!(
            single_text(
                &ctx,
                &format!("SELECT pg_catalog.pg_encoding_to_char({id})")
            )
            .await?,
            expected,
            "id {id}"
        );
    }
    Ok(())
}

/// An integer LITERAL is accepted - the defect that made the function
/// unusable: SQL literals are `Int64` and the old `Exact(Int32)` signature
/// refused them, so this call was a coercion error before.
#[tokio::test(flavor = "multi_thread")]
async fn an_integer_literal_is_accepted() -> DFResult<()> {
    let ctx = base_ctx().await?;
    assert_eq!(
        single_text(&ctx, "SELECT pg_encoding_to_char(6)").await?,
        "UTF8"
    );
    Ok(())
}

/// Names resolve case-insensitively, and an unknown name answers -1 - both
/// measured `PostgreSQL` behaviours, not conventions.
#[tokio::test(flavor = "multi_thread")]
async fn encoding_names_resolve_case_insensitively_or_answer_minus_one() -> DFResult<()> {
    let ctx = base_ctx().await?;
    for (name, expected) in [
        ("UTF8", "6"),
        ("utf8", "6"),
        ("LATIN1", "8"),
        ("nonsense", "-1"),
    ] {
        assert_eq!(
            single_text(
                &ctx,
                &format!("SELECT pg_catalog.pg_char_to_encoding('{name}')")
            )
            .await?,
            expected,
            "name {name}"
        );
    }
    Ok(())
}

/// The two directions round-trip through every id the table holds, and the
/// exact composition psql's \dO writes resolves to this database's encoding.
#[tokio::test(flavor = "multi_thread")]
async fn the_directions_round_trip_and_the_list_collations_composition_resolves() -> DFResult<()> {
    let ctx = base_ctx().await?;
    // Every named id maps back to itself. Driven through SQL so the test
    // exercises the registered functions, not the table behind them.
    let (batches, _schema) = execute_sql(
        &ctx,
        "SELECT count(*) FROM (SELECT n FROM generate_series(0, 41) AS t(n) \
         WHERE pg_catalog.pg_char_to_encoding(pg_catalog.pg_encoding_to_char(n)) <> n)",
        None,
        None,
    )
    .await?;
    let cast = arrow::compute::cast(batches[0].column(0), &arrow::datatypes::DataType::Int64)?;
    let mismatches = arrow::array::as_primitive_array::<arrow::datatypes::Int64Type>(&cast);
    assert_eq!(mismatches.value(0), 0, "every id survives the round trip");

    // The exact predicate composition from psql's \dO.
    assert_eq!(
        single_text(
            &ctx,
            "SELECT pg_catalog.pg_char_to_encoding(pg_catalog.getdatabaseencoding())"
        )
        .await?,
        "6",
        "the database reports UTF8, id 6"
    );
    Ok(())
}
