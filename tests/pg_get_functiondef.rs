//! `pg_get_functiondef` renders the definitions psql's `\sf` prints.
//!
//! The catalog's `pg_proc` snapshot carries `prosrc` and every flag column
//! captured from a real server, so the definitions rendered here are the REAL
//! ones - for an internal function the body `PostgreSQL` prints IS the
//! `prosrc` symbol. Every expected string below is the bundled `PostgreSQL`
//! 17.4's own `\sf` output, captured live and asserted byte for byte, so a
//! drift in flag order, argument rendering, or type spelling fails loudly.
//!
//! The chosen shapes cover the renderer's branches: plain stable (`now`),
//! `LEAKPROOF` placement (`texteq`), OUT arguments with names, an array type
//! and a `ROWS` estimate (`aclexplode`), the flags line OMITTED entirely
//! (`set_config`), `WINDOW` plus `SUPPORT` (`row_number`), and a quoted
//! pseudo-type with `VARIADIC` (`concat`).

use arrow::array::Array;
use datafusion::error::Result as DFResult;
use datafusion_pg_catalog::session::execute_sql;

mod common;
use common::base_ctx;

/// The definition of the function `name` with `nargs` arguments, resolved by
/// name the way a client would, or None for a NULL answer.
async fn definition_of(
    ctx: &datafusion::prelude::SessionContext,
    name: &str,
    nargs: i32,
) -> DFResult<Option<String>> {
    let (batches, _schema) = execute_sql(
        ctx,
        &format!(
            "SELECT pg_catalog.pg_get_functiondef((SELECT min(CAST(oid AS BIGINT)) \
             FROM pg_catalog.pg_proc WHERE proname = '{name}' AND pronargs = {nargs}))"
        ),
        None,
        None,
    )
    .await?;
    let cast = arrow::compute::cast(batches[0].column(0), &arrow::datatypes::DataType::Utf8)?;
    let array = arrow::array::as_string_array(&cast);
    Ok((!array.is_null(0)).then(|| array.value(0).to_string()))
}

/// Each covered shape renders byte for byte what the real server prints.
#[tokio::test(flavor = "multi_thread")]
async fn each_shape_renders_postgresqls_exact_output() -> DFResult<()> {
    let ctx = base_ctx().await?;
    for (name, nargs, expected) in [
        (
            "now",
            0,
            "CREATE OR REPLACE FUNCTION pg_catalog.now()\n \
             RETURNS timestamp with time zone\n \
             LANGUAGE internal\n \
             STABLE PARALLEL SAFE STRICT\n\
             AS $function$now$function$\n",
        ),
        (
            "texteq",
            2,
            "CREATE OR REPLACE FUNCTION pg_catalog.texteq(text, text)\n \
             RETURNS boolean\n \
             LANGUAGE internal\n \
             IMMUTABLE PARALLEL SAFE STRICT LEAKPROOF\n\
             AS $function$texteq$function$\n",
        ),
        (
            "aclexplode",
            1,
            "CREATE OR REPLACE FUNCTION pg_catalog.aclexplode(acl aclitem[], \
             OUT grantor oid, OUT grantee oid, OUT privilege_type text, \
             OUT is_grantable boolean)\n \
             RETURNS SETOF record\n \
             LANGUAGE internal\n \
             STABLE PARALLEL SAFE STRICT ROWS 10\n\
             AS $function$aclexplode$function$\n",
        ),
        // Every flag at its default: the flags line is omitted entirely.
        (
            "set_config",
            3,
            "CREATE OR REPLACE FUNCTION pg_catalog.set_config(text, text, boolean)\n \
             RETURNS text\n \
             LANGUAGE internal\n\
             AS $function$set_config_by_name$function$\n",
        ),
        (
            "row_number",
            0,
            "CREATE OR REPLACE FUNCTION pg_catalog.row_number()\n \
             RETURNS bigint\n \
             LANGUAGE internal\n \
             WINDOW IMMUTABLE PARALLEL SAFE SUPPORT window_row_number_support\n\
             AS $function$window_row_number$function$\n",
        ),
        (
            "concat",
            1,
            "CREATE OR REPLACE FUNCTION pg_catalog.concat(VARIADIC \"any\")\n \
             RETURNS text\n \
             LANGUAGE internal\n \
             STABLE PARALLEL SAFE\n\
             AS $function$text_concat$function$\n",
        ),
    ] {
        assert_eq!(
            definition_of(&ctx, name, nargs).await?.as_deref(),
            Some(expected),
            "{name}"
        );
    }
    Ok(())
}

/// The edges answer exactly as `PostgreSQL` answers them, each measured live:
/// a missing OID is NULL, an aggregate is a refused named error, and a
/// new-style SQL function whose body the snapshot does not carry is NULL
/// rather than a fabricated definition.
#[tokio::test(flavor = "multi_thread")]
async fn missing_aggregates_and_uncaptured_bodies_answer_as_postgresql_does() -> DFResult<()> {
    let ctx = base_ctx().await?;

    let (batches, _schema) = execute_sql(
        &ctx,
        "SELECT pg_catalog.pg_get_functiondef(999999999)",
        None,
        None,
    )
    .await?;
    assert!(
        batches[0].column(0).is_null(0),
        "a missing oid answers NULL, as PostgreSQL answers it"
    );

    let error = execute_sql(
        &ctx,
        "SELECT pg_catalog.pg_get_functiondef(2100)",
        None,
        None,
    )
    .await
    .expect_err("2100 is avg(int8), an aggregate");
    assert!(
        error
            .to_string()
            .contains("\"avg\" is an aggregate function"),
        "{error}"
    );

    // pg_sleep_for is a new-style SQL function: its RETURN body is a
    // prosqlbody node tree the snapshot does not deparse, so no faithful
    // definition exists and the answer must be visibly missing - never a
    // wrong-but-plausible body.
    assert_eq!(
        definition_of(&ctx, "pg_sleep_for", 1).await?,
        None,
        "an uncaptured body answers NULL rather than an invented definition"
    );
    Ok(())
}
