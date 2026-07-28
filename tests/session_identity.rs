//! `current_user` and friends report the role of the connection asking, not a
//! value fixed when the catalog was built.
//!
//! The catalog's views are planned once, at startup, before any client exists.
//! A view body containing `CURRENT_USER` therefore cannot carry a per-connection
//! value in its plan - what it carries is a reference to one UDF instance, and
//! that instance has to work out the answer when it is called. These tests pin
//! the two halves of that: the value comes from the session running the query,
//! and it still does when the call is buried inside a view planned long before
//! the session existed.
//!
//! Before this, the three functions read a process-global slot that riffq never
//! wrote, so every client was told it was `postgres`.

use datafusion::error::Result as DFResult;
use datafusion::prelude::SessionContext;
use datafusion_pg_catalog::session::{set_session_user, ClientOpts};
use datafusion_pg_catalog::user_functions::{register_current_database, register_session_identity};

/// A context carrying `ClientOpts`, the way the real catalog builds one.
fn context_with_client_opts() -> SessionContext {
    let config = datafusion::execution::context::SessionConfig::new()
        .with_option_extension(ClientOpts::default());
    SessionContext::new_with_config(config)
}

/// The single value a one-row, one-column text query returns.
async fn single_text(ctx: &SessionContext, sql: &str) -> DFResult<String> {
    let batches = ctx.sql(sql).await?.collect().await?;
    let column = arrow::compute::cast(batches[0].column(0), &arrow::datatypes::DataType::Utf8)?;
    let array = column
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .expect("a text column");
    Ok(array.value(0).to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_identity_functions_report_the_recorded_role() -> DFResult<()> {
    let ctx = context_with_client_opts();
    register_session_identity(&ctx)?;
    set_session_user(&ctx, "alice")?;

    // The spellings a client can actually write. sqlparser treats current_user
    // and session_user as keywords, so they take no parentheses and cannot be
    // schema-qualified; current_role is not a keyword, so it needs the
    // parentheses that the other two reject. All three quirks are the parser's
    // and predate per-connection identity.
    //
    // The pg_catalog-qualified aliases are registered even though no client can
    // type them: the router decides whether a bare call belongs to the catalog
    // by looking up "pg_catalog.<name>" in the function registry
    // (router.rs function_is_catalog), so without them a plain SELECT
    // current_user would be handed to the host instead of answered here.
    for sql in [
        "SELECT current_user",
        "SELECT session_user",
        "SELECT current_role()",
    ] {
        assert_eq!(single_text(&ctx, sql).await?, "alice", "{sql}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_current_database_reports_the_sessions_catalog() -> DFResult<()> {
    // One context serves one database and is named after it, so the session's
    // default catalog IS its database. This replaced rewriting the SQL text of
    // every view body that called current_database().
    let config = datafusion::execution::context::SessionConfig::new()
        .with_default_catalog_and_schema("sales", "public")
        .with_option_extension(ClientOpts::default());
    let ctx = SessionContext::new_with_config(config);
    register_current_database(&ctx)?;

    assert_eq!(
        single_text(&ctx, "SELECT current_database()").await?,
        "sales"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_current_database_inside_a_view_body_reports_that_database() -> DFResult<()> {
    // A view body may call current_database() directly now. Each database has its
    // own context, so each one's view reports its own name - which is what the
    // deleted text rewriting was faking by substituting a literal before
    // planning, and what it got wrong the moment two databases shared a context.
    for database in ["sales", "hr"] {
        let config = datafusion::execution::context::SessionConfig::new()
            .with_default_catalog_and_schema(database, "public")
            .with_option_extension(ClientOpts::default());
        let ctx = SessionContext::new_with_config(config);
        register_current_database(&ctx)?;
        ctx.sql("CREATE VIEW where_am_i AS SELECT current_database() AS db")
            .await?
            .collect()
            .await?;

        assert_eq!(
            single_text(&ctx, "SELECT db FROM where_am_i").await?,
            database,
            "{database}'s view must report {database}"
        );
        // And directly, so a mismatch between the two is visible rather than
        // both being wrong in the same direction.
        assert_eq!(
            single_text(&ctx, "SELECT current_database()").await?,
            database
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_the_catalog_qualified_aliases_are_registered() -> DFResult<()> {
    // Not reachable by typing them - see above - but the router looks them up
    // by name to decide that a bare current_user is the catalog's to answer.
    let ctx = context_with_client_opts();
    register_session_identity(&ctx)?;

    register_current_database(&ctx)?;
    let registered = ctx.state().scalar_functions().clone();

    for name in [
        "pg_catalog.current_user",
        "pg_catalog.session_user",
        "pg_catalog.current_role",
        "pg_catalog.current_database",
    ] {
        assert!(
            registered.contains_key(name),
            "{name} must stay registered or the router stops treating \
             a bare call as the catalog's to answer"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_session_that_was_never_told_reports_postgres() -> DFResult<()> {
    // Every consumer that does not authenticate anyone keeps the behaviour it
    // had before per-connection identity existed.
    let ctx = context_with_client_opts();
    register_session_identity(&ctx)?;
    assert_eq!(single_text(&ctx, "SELECT current_user").await?, "postgres");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_two_connections_over_one_base_report_their_own_roles() -> DFResult<()> {
    // The shape riffq and the standalone server both use: one base context with
    // the catalog in it, and a cheap per-connection clone over the top. The
    // clone owns its config, so one connection's role must not reach the other.
    let base = context_with_client_opts();
    register_session_identity(&base)?;

    let alice = SessionContext::new_with_state(base.state().clone());
    let bob = SessionContext::new_with_state(base.state().clone());
    set_session_user(&alice, "alice")?;
    set_session_user(&bob, "bob")?;

    assert_eq!(single_text(&alice, "SELECT current_user").await?, "alice");
    assert_eq!(single_text(&bob, "SELECT current_user").await?, "bob");
    assert_eq!(
        single_text(&base, "SELECT current_user").await?,
        "postgres",
        "neither connection may write back into the base they were cloned from"
    );

    // And a role set after the clone exists is still picked up, since nothing
    // is captured at planning time.
    set_session_user(&alice, "carol")?;
    assert_eq!(single_text(&alice, "SELECT current_user").await?, "carol");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_view_planned_before_the_connection_reports_its_role() -> DFResult<()> {
    // The case the whole design turns on. The view is created on the base
    // context, before either connection exists, so its plan predates them both.
    let base = context_with_client_opts();
    register_session_identity(&base)?;
    base.sql("CREATE VIEW whoami AS SELECT current_user AS role")
        .await?
        .collect()
        .await?;

    let alice = SessionContext::new_with_state(base.state().clone());
    let bob = SessionContext::new_with_state(base.state().clone());
    set_session_user(&alice, "alice")?;
    set_session_user(&bob, "bob")?;

    assert_eq!(
        single_text(&alice, "SELECT role FROM whoami").await?,
        "alice",
        "a view planned before the connection must still report its role"
    );
    assert_eq!(single_text(&bob, "SELECT role FROM whoami").await?, "bob");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_the_role_is_not_folded_away_at_planning_time() -> DFResult<()> {
    // Stable rather than Immutable volatility. An Immutable nullary function is
    // fair game for constant folding, which would bake the planning session's
    // role into the plan - and the plan is shared by every connection.
    let base = context_with_client_opts();
    register_session_identity(&base)?;
    set_session_user(&base, "planner")?;
    base.sql("CREATE VIEW whoami AS SELECT current_user AS role")
        .await?
        .collect()
        .await?;
    // Plan and run it once as the planning role, so any folding that is going
    // to happen has happened.
    assert_eq!(
        single_text(&base, "SELECT role FROM whoami").await?,
        "planner"
    );

    let later = SessionContext::new_with_state(base.state().clone());
    set_session_user(&later, "alice")?;
    assert_eq!(
        single_text(&later, "SELECT role FROM whoami").await?,
        "alice",
        "the planning session's role must not survive into another session's query"
    );
    Ok(())
}
