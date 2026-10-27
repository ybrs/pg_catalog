//! Session GUCs over the wire path: `SET` spellings landing in `ClientOpts`
//! and `current_setting` reading them back (task 27).
//!
//! Every behavior asserted here was measured against the bundled
//! `PostgreSQL` 17.4 (`./run-postgres.sh`, port 5434) on 2026-08-11:
//!
//! - `SET TIME ZONE 'x'` is an alias for `SET timezone TO 'x'` and answers a
//!   `SET` command tag;
//! - GUC names match case-insensitively (`SET SEARCH_PATH` works, and
//!   `current_setting('TimeZone')` equals `current_setting('timezone')`);
//! - `current_setting` of an unknown parameter raises
//!   `unrecognized configuration parameter "<name>"` with the name in the
//!   caller's casing, answers NULL instead when `missing_ok` is true, and
//!   answers NULL for a NULL name;
//! - `SET LOCAL` outside a transaction warns and changes nothing, so its
//!   classification acknowledges without applying.

use arrow::array::{Array, StringArray};
use bytes::Bytes;
use datafusion::error::Result as DFResult;
use datafusion::execution::context::SessionContext;
use datafusion_pg_catalog::session::execute_sql;
use datafusion_pg_catalog::{classify_set_statement, SetStatementDisposition};
use pgwire::api::Type;

mod common;
use common::base_ctx;

/// The single text value a one-row, one-column result carries, or None for
/// SQL NULL.
async fn single_text_value(ctx: &SessionContext, sql: &str) -> DFResult<Option<String>> {
    let (batches, _) = execute_sql(ctx, sql, None, None).await?;
    let column = batches[0].column(0);
    let strings = column
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("a text result");
    if strings.is_null(0) {
        Ok(None)
    } else {
        Ok(Some(strings.value(0).to_string()))
    }
}

/// Every spelling of a session SET lands in `ClientOpts` and reads back
/// through both `SHOW`-backing state and `current_setting`.
#[tokio::test(flavor = "multi_thread")]
async fn set_spellings_land_in_the_session_and_read_back() -> DFResult<()> {
    let ctx = base_ctx().await?;

    // The plain assignment.
    execute_sql(&ctx, "SET search_path = myschema", None, None).await?;
    assert_eq!(
        single_text_value(&ctx, "SELECT current_setting('search_path')").await?,
        Some("myschema".to_string())
    );

    // Upper-case GUC name, as PostgreSQL accepts it.
    execute_sql(&ctx, "SET APPLICATION_NAME = 'reporting'", None, None).await?;
    assert_eq!(
        single_text_value(&ctx, "SELECT current_setting('application_name')").await?,
        Some("reporting".to_string())
    );

    // The TIME ZONE alias, and the case-insensitive read-back PostgreSQL
    // gives every GUC name.
    execute_sql(&ctx, "SET TIME ZONE 'Europe/Istanbul'", None, None).await?;
    assert_eq!(
        single_text_value(&ctx, "SELECT current_setting('TimeZone')").await?,
        Some("Europe/Istanbul".to_string())
    );

    // SET timezone TO reaches the same variable as SET TIME ZONE.
    execute_sql(&ctx, "SET timezone TO 'America/Bogota'", None, None).await?;
    assert_eq!(
        single_text_value(&ctx, "SELECT current_setting('timezone')").await?,
        Some("America/Bogota".to_string())
    );

    // SET TIME ZONE DEFAULT resets to the emulation's fixed default.
    execute_sql(&ctx, "SET TIME ZONE DEFAULT", None, None).await?;
    assert_eq!(
        single_text_value(&ctx, "SELECT current_setting('timezone')").await?,
        Some("UTC".to_string())
    );

    Ok(())
}

/// `current_setting` answers exactly as `PostgreSQL` does at its edges: the
/// `pg_settings` snapshot for parameters the session does not carry live, the
/// measured error for unknown names, NULL under `missing_ok`, and NULL in
/// NULL out.
#[tokio::test(flavor = "multi_thread")]
async fn current_setting_answers_postgresqls_edges() -> DFResult<()> {
    let ctx = base_ctx().await?;

    // A parameter no session variable carries comes from the pg_settings
    // snapshot; max_connections is 100 there as on a stock server.
    assert_eq!(
        single_text_value(&ctx, "SELECT current_setting('max_connections')").await?,
        Some("100".to_string())
    );

    // session_authorization reads the authenticated role.
    assert_eq!(
        single_text_value(&ctx, "SELECT current_setting('session_authorization')").await?,
        Some("postgres".to_string())
    );

    // Unknown parameter: PostgreSQL's exact message, name in the caller's
    // casing.
    let error = execute_sql(&ctx, "SELECT current_setting('No_Such_Thing')", None, None)
        .await
        .expect_err("an unknown parameter is an error");
    assert!(
        error
            .to_string()
            .contains("unrecognized configuration parameter \"No_Such_Thing\""),
        "got: {error}"
    );

    // missing_ok turns that error into NULL.
    assert_eq!(
        single_text_value(&ctx, "SELECT current_setting('No_Such_Thing', true)").await?,
        None
    );

    // And a known parameter with missing_ok false still answers.
    assert_eq!(
        single_text_value(&ctx, "SELECT current_setting('max_connections', false)").await?,
        Some("100".to_string())
    );

    // NULL name answers NULL rather than erroring.
    assert_eq!(
        single_text_value(&ctx, "SELECT current_setting(NULL)").await?,
        None
    );

    Ok(())
}

/// The wire-side routing decision: which SET statements the embedding server
/// answers itself, which it acknowledges without applying, and which stay
/// the host's.
#[tokio::test(flavor = "multi_thread")]
async fn set_statements_classify_by_session_ownership() {
    use SetStatementDisposition::{AcknowledgeWithoutApplying, ApplyToSession, NotASessionSet};

    // The variables the session stores, in the spellings clients use.
    assert_eq!(
        classify_set_statement("SET search_path = public"),
        ApplyToSession
    );
    assert_eq!(
        classify_set_statement("SET SEARCH_PATH TO a, b"),
        ApplyToSession
    );
    assert_eq!(
        classify_set_statement("SET pg_catalog.search_path = public"),
        ApplyToSession
    );
    assert_eq!(
        classify_set_statement("SET TIME ZONE 'UTC'"),
        ApplyToSession
    );
    assert_eq!(
        classify_set_statement("SET application_name = 'app'"),
        ApplyToSession
    );
    // Drivers set extra_float_digits at connect; it is accepted and ignored.
    assert_eq!(
        classify_set_statement("SET extra_float_digits = 3"),
        ApplyToSession
    );

    // LOCAL scopes to a transaction this crate does not model: PostgreSQL
    // outside a transaction warns and changes nothing, so acknowledge only.
    assert_eq!(
        classify_set_statement("SET LOCAL search_path = other"),
        AcknowledgeWithoutApplying
    );
    assert_eq!(
        classify_set_statement("SET LOCAL TIME ZONE 'UTC'"),
        AcknowledgeWithoutApplying
    );

    // Not the session's: unknown variables stay the host's to answer, the
    // session user is the host's authentication to assert, and non-SET
    // statements were never SETs at all.
    assert_eq!(
        classify_set_statement("SET some_host_variable = 1"),
        NotASessionSet
    );
    assert_eq!(
        classify_set_statement("SET LOCAL some_host_variable = 1"),
        NotASessionSet
    );
    assert_eq!(
        classify_set_statement("SET session_user = 'admin'"),
        NotASessionSet
    );
    assert_eq!(
        classify_set_statement("SET SESSION AUTHORIZATION admin"),
        NotASessionSet
    );
    assert_eq!(classify_set_statement("SELECT 1"), NotASessionSet);
    assert_eq!(
        classify_set_statement("SET search_path = a; SET search_path = b"),
        NotASessionSet
    );
    assert_eq!(classify_set_statement("not sql at all"), NotASessionSet);
}

/// A client cannot rewrite its own identity with `SET session_user`.
///
/// The classifier leaves that statement to the host, and the executor must
/// not apply it either: before this was closed, `SET session_user =
/// 'admin'` through the library path changed what `current_user`,
/// `session_user` and `current_setting('session_authorization')` answered
/// on `pg_catalog`'s own server, which never classifies.
#[tokio::test(flavor = "multi_thread")]
async fn set_session_user_through_sql_is_rejected_and_changes_nothing() -> DFResult<()> {
    let ctx = base_ctx().await?;

    let error = execute_sql(&ctx, "SET session_user = 'admin'", None, None)
        .await
        .expect_err("SET session_user must not be applicable through SQL");
    assert!(
        error.to_string().contains("unknown key session_user"),
        "got: {error}"
    );
    assert_eq!(
        single_text_value(&ctx, "SELECT current_user").await?,
        Some("postgres".to_string())
    );
    assert_eq!(
        single_text_value(&ctx, "SELECT session_user").await?,
        Some("postgres".to_string())
    );
    assert_eq!(
        single_text_value(&ctx, "SELECT current_setting('session_authorization')").await?,
        Some("postgres".to_string())
    );

    Ok(())
}

/// A multi-value `SET search_path` executes and reads back as the list.
///
/// `PostgreSQL` treats `TO a, b` as one variable holding a list, while the
/// planner's own SET accepts a single value - so this statement used to be
/// classified `ApplyToSession` and then fail to plan.
#[tokio::test(flavor = "multi_thread")]
async fn multi_value_search_path_set_executes_and_reads_back() -> DFResult<()> {
    let ctx = base_ctx().await?;

    execute_sql(&ctx, "SET search_path TO a, b", None, None).await?;
    assert_eq!(
        single_text_value(&ctx, "SELECT current_setting('search_path')").await?,
        Some("a, b".to_string())
    );

    execute_sql(&ctx, "SET search_path = c, d", None, None).await?;
    assert_eq!(
        single_text_value(&ctx, "SELECT current_setting('search_path')").await?,
        Some("c, d".to_string())
    );

    Ok(())
}

/// `SET LOCAL` through the executor answers without applying.
///
/// The classifier already said `AcknowledgeWithoutApplying`; a path that
/// never classifies - `pg_catalog`'s own server - used to reach the planner
/// and fail with "SET with scope modifiers is not supported" instead.
#[tokio::test(flavor = "multi_thread")]
async fn set_local_through_sql_answers_without_applying() -> DFResult<()> {
    let ctx = base_ctx().await?;

    execute_sql(&ctx, "SET search_path = kept", None, None).await?;
    execute_sql(&ctx, "SET LOCAL search_path = discarded", None, None).await?;
    assert_eq!(
        single_text_value(&ctx, "SELECT current_setting('search_path')").await?,
        Some("kept".to_string())
    );

    execute_sql(&ctx, "SET LOCAL TIME ZONE 'Europe/Istanbul'", None, None).await?;
    assert_eq!(
        single_text_value(&ctx, "SELECT current_setting('timezone')").await?,
        Some("UTC".to_string())
    );

    Ok(())
}

/// A `SET` whose value arrives as a bound parameter applies that value.
///
/// Drivers that prepare their statements send `SET application_name =
/// $1`; the placeholder used to reach the planner unbound and fail with
/// "Unsupported value Placeholder".
#[tokio::test(flavor = "multi_thread")]
async fn parameterized_set_applies_the_bound_value() -> DFResult<()> {
    let ctx = base_ctx().await?;

    let params = Some(vec![Some(Bytes::from("reporting"))]);
    let types = Some(vec![Type::TEXT]);
    execute_sql(&ctx, "SET application_name = $1", params, types).await?;
    assert_eq!(
        single_text_value(&ctx, "SELECT current_setting('application_name')").await?,
        Some("reporting".to_string())
    );

    // A placeholder with nothing bound is a reported error, not a panic
    // and not a silently kept previous value presented as success.
    let error = execute_sql(&ctx, "SET application_name = $1", None, None)
        .await
        .expect_err("an unbound SET parameter must be reported");
    assert!(
        error.to_string().contains("no parameters were bound"),
        "got: {error}"
    );
    assert_eq!(
        single_text_value(&ctx, "SELECT current_setting('application_name')").await?,
        Some("reporting".to_string())
    );

    Ok(())
}

/// `SET <variable> = DEFAULT` restores the value a fresh connection has.
#[tokio::test(flavor = "multi_thread")]
async fn set_default_restores_the_connection_default() -> DFResult<()> {
    let ctx = base_ctx().await?;

    execute_sql(&ctx, "SET search_path = myschema", None, None).await?;
    execute_sql(&ctx, "SET search_path = DEFAULT", None, None).await?;
    assert_eq!(
        single_text_value(&ctx, "SELECT current_setting('search_path')").await?,
        Some("\"$user\", public".to_string())
    );

    Ok(())
}

/// A `SET` applied on one connection's context is invisible on another's,
/// because each connection clones the base context and `ClientOpts` clones
/// with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_set_on_one_connection_does_not_leak_to_another() -> DFResult<()> {
    let base = base_ctx().await?;
    let first = SessionContext::new_with_state(base.state().clone());
    let second = SessionContext::new_with_state(base.state().clone());

    execute_sql(&first, "SET TIME ZONE 'Asia/Tokyo'", None, None).await?;

    assert_eq!(
        single_text_value(&first, "SELECT current_setting('timezone')").await?,
        Some("Asia/Tokyo".to_string())
    );
    assert_eq!(
        single_text_value(&second, "SELECT current_setting('timezone')").await?,
        Some("UTC".to_string()),
        "the second connection must keep its own default"
    );

    Ok(())
}
