//! Can one shared context serve per-connection catalog rows?
//!
//! The catalog is currently either flattened into a single context (the lazy
//! path, where every database's schemas are visible at once and so cannot all
//! use PostgreSQL's canonical oids) or rebuilt per database (the eager path,
//! which costs a full context each). Unifying them on a single shared context
//! depends on one mechanism: a table provider reading the connected database
//! from session config at scan time, so each connection sees only its own
//! database's rows.
//!
//! That is easy to believe for a direct table scan. The load-bearing question
//! is whether it still holds through a VIEW, because the catalog's 136 views
//! are planned once at startup and shared by every connection. If DataFusion
//! resolved anything about those scans at plan time rather than scan time, a
//! view would freeze whichever session created it and the whole design would
//! collapse back to per-database contexts.
//!
//! These tests answer that with the real machinery: a provider that reads a
//! ClientOpts value at scan time, a view planned once over it, and contexts
//! cloned per connection the way riffq clones them.

use std::sync::Arc;

use arrow::array::StringArray;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use async_trait::async_trait;
use datafusion::catalog::Session;
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::error::Result as DFResult;
use datafusion::execution::context::{SessionConfig, SessionContext};
use datafusion::logical_expr::Expr;
use datafusion::physical_plan::ExecutionPlan;
use datafusion_pg_catalog::session::ClientOpts;

/// A table whose single row is whatever the session's `search_path` says.
///
/// `search_path` stands in for "the connected database": it is an existing
/// per-session ClientOpts value, so this exercises the real config plumbing
/// rather than a mechanism invented for the test.
#[derive(Debug)]
struct SessionScopedTable {
    schema: SchemaRef,
}

impl SessionScopedTable {
    fn new() -> Self {
        Self {
            schema: Arc::new(Schema::new(vec![Field::new("scope", DataType::Utf8, false)])),
        }
    }
}

#[async_trait]
impl TableProvider for SessionScopedTable {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> DFResult<Arc<dyn ExecutionPlan>> {
        // The whole point: the rows are decided here, from the session doing
        // the scanning, not when the table or any view over it was created.
        let scope = state
            .config_options()
            .extensions
            .get::<ClientOpts>()
            .map(|opts| opts.search_path.clone())
            .unwrap_or_else(|| "<no client opts>".to_string());

        let batch = RecordBatch::try_new(
            self.schema.clone(),
            vec![Arc::new(StringArray::from(vec![scope]))],
        )?;
        let mem = MemTable::try_new(self.schema.clone(), vec![vec![batch]])?;
        mem.scan(state, projection, filters, limit).await
    }
}

/// Build a context holding the scoped table and a view over it.
///
/// The view is created once here, mirroring startup, and then shared by every
/// cloned connection below.
async fn base_context_with_view() -> DFResult<SessionContext> {
    let config = SessionConfig::new().with_option_extension(ClientOpts::default());
    let ctx = SessionContext::new_with_config(config);
    ctx.register_table("scoped_rows", Arc::new(SessionScopedTable::new()))?;
    ctx.sql("CREATE VIEW scoped_view AS SELECT scope FROM scoped_rows")
        .await?
        .collect()
        .await?;
    Ok(ctx)
}

/// Clone a context the way riffq clones one per connection, and point the
/// clone at `scope`.
async fn connection_with_scope(base: &SessionContext, scope: &str) -> DFResult<SessionContext> {
    let ctx = SessionContext::new_with_state(base.state().clone());
    ctx.sql(&format!("SET pg_catalog.search_path = '{scope}'"))
        .await?
        .collect()
        .await?;
    Ok(ctx)
}

/// Read the single `scope` value a query returns.
async fn scope_of(ctx: &SessionContext, sql: &str) -> DFResult<String> {
    let batches = ctx.sql(sql).await?.collect().await?;
    let batch = batches.first().expect("one batch");
    let column = batch
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("utf8 column");
    Ok(column.value(0).to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_direct_scan_sees_the_scanning_session() -> DFResult<()> {
    let base = base_context_with_view().await?;
    let first = connection_with_scope(&base, "db_one").await?;
    let second = connection_with_scope(&base, "db_two").await?;

    assert_eq!(scope_of(&first, "SELECT scope FROM scoped_rows").await?, "db_one");
    assert_eq!(scope_of(&second, "SELECT scope FROM scoped_rows").await?, "db_two");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_view_planned_once_still_sees_the_scanning_session() -> DFResult<()> {
    // The decisive case. The view was planned in the base context before
    // either connection existed; if its scans were bound to that session, both
    // connections would read the base's value instead of their own.
    let base = base_context_with_view().await?;
    let first = connection_with_scope(&base, "db_one").await?;
    let second = connection_with_scope(&base, "db_two").await?;

    assert_eq!(scope_of(&first, "SELECT scope FROM scoped_view").await?, "db_one");
    assert_eq!(scope_of(&second, "SELECT scope FROM scoped_view").await?, "db_two");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_connections_do_not_leak_scope_into_each_other() -> DFResult<()> {
    // Cloned contexts must own their config; if the clone shared the base's
    // options, setting one connection's database would change every other
    // connection's view of the catalog.
    let base = base_context_with_view().await?;
    let first = connection_with_scope(&base, "db_one").await?;
    let _second = connection_with_scope(&base, "db_two").await?;

    // Re-read the first after the second has been set.
    assert_eq!(scope_of(&first, "SELECT scope FROM scoped_view").await?, "db_one");

    // And the base itself is unaffected by either connection.
    let base_scope = scope_of(&base, "SELECT scope FROM scoped_view").await?;
    assert_ne!(base_scope, "db_one");
    assert_ne!(base_scope, "db_two");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_scope_change_is_visible_without_replanning_the_view() -> DFResult<()> {
    // A connection that switches database mid-session must see the new one
    // through the same shared view plan.
    let base = base_context_with_view().await?;
    let ctx = connection_with_scope(&base, "db_one").await?;
    assert_eq!(scope_of(&ctx, "SELECT scope FROM scoped_view").await?, "db_one");

    ctx.sql("SET pg_catalog.search_path = 'db_three'")
        .await?
        .collect()
        .await?;
    assert_eq!(scope_of(&ctx, "SELECT scope FROM scoped_view").await?, "db_three");
    Ok(())
}
