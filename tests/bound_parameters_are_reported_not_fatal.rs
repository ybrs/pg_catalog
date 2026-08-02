//! Binding a parameter must never take the connection down.
//!
//! The parameter decoder unwrapped every width and every UTF-8 decode and
//! called `panic!` for any type it did not know. Since the panic unwinds inside
//! the per-connection task, the client saw "server closed the connection
//! unexpectedly" - and it did not take malformed input to get there. Probed
//! against a live server with psycopg, which sends these without being asked:
//!
//! ```text
//! float8   connection dropped
//! bool     connection dropped
//! bytea    connection dropped
//! ```
//!
//! float8 and bool are bound properly now. Everything the catalog cannot carry
//! - date, uuid, numeric, and bytea, which has no encoder arm to bring it back
//! - is a reported error naming the parameter and its type.
//!
//! These tests go through `rewrite_and_execute_sql`, the entry point the server
//! calls with the wire bytes, because the contract being tested is "what
//! happens to a client's parameter", not the shape of a private helper.

use bytes::Bytes;
use datafusion::error::Result as DFResult;
use datafusion_pg_catalog::session::rewrite_and_execute_sql;
use pgwire::api::Type;

mod common;
use common::base_ctx;

/// Run `sql` with one bound parameter.
async fn execute_with_parameter(
    sql: &str,
    value: Option<Bytes>,
    declared: Type,
) -> DFResult<Vec<arrow::array::RecordBatch>> {
    let ctx = base_ctx().await?;
    let (batches, _schema) =
        rewrite_and_execute_sql(&ctx, sql, Some(vec![value]), Some(vec![declared])).await?;
    Ok(batches)
}

/// The types psycopg sends for an ordinary Python value all bind.
#[tokio::test(flavor = "multi_thread")]
async fn the_types_a_client_sends_without_being_asked_all_bind() -> DFResult<()> {
    let cases: Vec<(&str, Bytes, Type)> = vec![
        ("int2", Bytes::from(1i16.to_be_bytes().to_vec()), Type::INT2),
        ("int4", Bytes::from(1i32.to_be_bytes().to_vec()), Type::INT4),
        ("int8", Bytes::from(1i64.to_be_bytes().to_vec()), Type::INT8),
        (
            "float4",
            Bytes::from(1.5f32.to_be_bytes().to_vec()),
            Type::FLOAT4,
        ),
        (
            "float8",
            Bytes::from(1.5f64.to_be_bytes().to_vec()),
            Type::FLOAT8,
        ),
        ("bool", Bytes::from(vec![1u8]), Type::BOOL),
        ("text", Bytes::from("abc".as_bytes().to_vec()), Type::TEXT),
        (
            "oid",
            Bytes::from(1259u32.to_be_bytes().to_vec()),
            Type::OID,
        ),
    ];
    for (name, value, declared) in cases {
        execute_with_parameter("SELECT $1", Some(value), declared)
            .await
            .unwrap_or_else(|error| panic!("{name} should bind, got {error}"));
    }
    Ok(())
}

/// A NULL binds for each of those types too, carrying the right type with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_null_binds_for_every_supported_type() -> DFResult<()> {
    for declared in [
        Type::INT2,
        Type::INT4,
        Type::INT8,
        Type::OID,
        Type::FLOAT4,
        Type::FLOAT8,
        Type::BOOL,
        Type::TEXT,
    ] {
        execute_with_parameter("SELECT $1", None, declared.clone())
            .await
            .unwrap_or_else(|error| panic!("a NULL {declared} should bind, got {error}"));
    }
    Ok(())
}

/// A type the catalog cannot carry is reported, naming the parameter and type.
#[tokio::test(flavor = "multi_thread")]
async fn an_unbindable_type_is_reported_with_its_position() -> DFResult<()> {
    for declared in [Type::DATE, Type::UUID, Type::NUMERIC, Type::BYTEA] {
        let outcome =
            execute_with_parameter("SELECT $1", Some(Bytes::from(vec![0u8])), declared.clone())
                .await;
        let error = outcome
            .err()
            .unwrap_or_else(|| panic!("{declared} should be reported, not bound"));
        let message = error.to_string();
        assert!(
            message.contains("$1") && message.contains(&declared.to_string()),
            "the error should name the parameter and its type, got {message}"
        );
    }
    Ok(())
}

/// Bytes of the wrong width are reported, and the message says what to suspect.
///
/// Nothing reads Bind's per-parameter format codes, so a client sending a
/// parameter in text format rather than binary lands here - a text `1` is one
/// byte where a binary int4 is four. "Invalid width" alone would send the
/// reader hunting in the wrong place, so the message names the likely cause.
#[tokio::test(flavor = "multi_thread")]
async fn the_wrong_number_of_bytes_is_reported_not_fatal() -> DFResult<()> {
    let outcome = execute_with_parameter(
        "SELECT $1",
        Some(Bytes::from("1".as_bytes().to_vec())),
        Type::INT4,
    )
    .await;
    let message = outcome
        .expect_err("a one-byte int4 should be reported")
        .to_string();
    assert!(
        message.contains("$1") && message.contains("text format"),
        "the error should name the parameter and the likely cause, got {message}"
    );
    Ok(())
}

/// Text that is not UTF-8 is reported rather than aborting.
#[tokio::test(flavor = "multi_thread")]
async fn invalid_utf8_in_a_text_parameter_is_reported() -> DFResult<()> {
    let outcome =
        execute_with_parameter("SELECT $1", Some(Bytes::from(vec![0xff, 0xfe])), Type::TEXT).await;
    let message = outcome
        .expect_err("invalid UTF-8 should be reported")
        .to_string();
    assert!(
        message.contains("$1") && message.contains("UTF-8"),
        "the error should name the parameter and the cause, got {message}"
    );
    Ok(())
}
