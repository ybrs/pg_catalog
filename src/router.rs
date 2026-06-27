//! Query router for catalog-aware SQL execution.
//!
//! The functions in this module examine SQL statements and determine whether
//! they reference objects in the internal `pg_catalog` or
//! `information_schema`. Queries touching those schemas are executed directly
//! using [`execute_sql`], while all other statements are delegated to a caller
//! provided handler.
//!
//! The primary entry point is [`dispatch_query`]. Library users can call this
//! helper instead of executing SQL directly when they want catalog queries to
//! be handled automatically.

use std::sync::Arc;

use arrow::datatypes::Schema;
use arrow::record_batch::RecordBatch;
use datafusion::execution::context::SessionContext;
use datafusion::execution::FunctionRegistry;
use log::debug;

use crate::session::{execute_sql, ClientOpts};
use bytes::Bytes;
use pgwire::api::Type;

use sqlparser::ast::*;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

/// Split and normalise a `search_path` option.
///
/// The returned vector always contains `pg_catalog` as the first element
/// to mirror PostgreSQL behaviour when it is not explicitly listed.
fn parse_search_path(path: &str) -> Vec<String> {
    let mut parts: Vec<String> = path
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_matches('"').to_string())
        .collect();
    if !parts.iter().any(|s| s.eq_ignore_ascii_case("pg_catalog")) {
        parts.insert(0, "pg_catalog".to_string());
    }
    parts
}

/// Check if a table exists within the provided catalog and schema.
fn table_exists(ctx: &SessionContext, catalog: &str, schema: &str, table: &str) -> bool {
    ctx.catalog(catalog)
        .and_then(|c| c.schema(schema))
        .map(|s| s.table_exist(table))
        .unwrap_or(false)
}

/// Prepend the given schema to an [`ObjectName`] if it is unqualified.
fn qualify_table_name(name: &mut ObjectName, schema: &str) {
    if name.0.len() == 1 {
        let ident = name.0.remove(0);
        name.0.push(ObjectNamePart::Identifier(Ident::new(schema)));
        name.0.push(ident);
    }
}

/// Recursively qualify any catalog tables found in the given `TableFactor`.
fn qualify_factor(ctx: &SessionContext, factor: &mut TableFactor) {
    match factor {
        TableFactor::Table { name, .. } => {
            if let Some(schema) = resolve_schema(ctx, name) {
                if schema.eq_ignore_ascii_case("pg_catalog")
                    || schema.eq_ignore_ascii_case("information_schema")
                {
                    qualify_table_name(name, &schema);
                }
            }
        }
        TableFactor::Derived { subquery, .. } => qualify_query(ctx, subquery),
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => qualify_table_with_joins(ctx, table_with_joins),
        _ => {}
    }
}

/// Walks a `TableWithJoins` and qualifies catalog table names in all joins.
fn qualify_table_with_joins(ctx: &SessionContext, table_with_joins: &mut TableWithJoins) {
    qualify_factor(ctx, &mut table_with_joins.relation);
    for join in &mut table_with_joins.joins {
        qualify_factor(ctx, &mut join.relation);
    }
}

/// Qualify catalog tables referenced within a `SetExpr`.
fn qualify_setexpr(ctx: &SessionContext, expr: &mut SetExpr) {
    match expr {
        SetExpr::Select(select) => {
            for twj in &mut select.from {
                qualify_table_with_joins(ctx, twj);
            }
        }
        SetExpr::Query(q) => qualify_query(ctx, q),
        SetExpr::SetOperation { left, right, .. } => {
            qualify_setexpr(ctx, left);
            qualify_setexpr(ctx, right);
        }
        _ => {}
    }
}

/// Apply catalog qualification rules to a parsed [`Query`].
fn qualify_query(ctx: &SessionContext, query: &mut Query) {
    qualify_setexpr(ctx, &mut query.body);
    if let Some(with) = &mut query.with {
        for cte in &mut with.cte_tables {
            qualify_query(ctx, &mut cte.query);
        }
    }
}

/// Parse the SQL string and fully qualify catalog table references.
fn qualify_catalog_tables(ctx: &SessionContext, sql: &str) -> datafusion::error::Result<String> {
    let dialect = PostgreSqlDialect {};
    let mut statements = Parser::parse_sql(&dialect, sql).map_err(|e| {
        datafusion::error::DataFusionError::Plan(format!("failed to parse SQL: {e}"))
    })?;
    for stmt in &mut statements {
        if let Statement::Query(q) = stmt {
            qualify_query(ctx, q);
        }
    }
    Ok(statements
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("; "))
}

/// Resolve the schema for the provided table [`ObjectName`] taking the session
/// configuration and search path into account.
fn resolve_schema(ctx: &SessionContext, name: &ObjectName) -> Option<String> {
    let state = ctx.state();
    let options = state.config_options();
    let default_catalog = &options.catalog.default_catalog;
    let default_schema = &options.catalog.default_schema;
    let search_path = options
        .extensions
        .get::<ClientOpts>()
        .map(|opts| parse_search_path(&opts.search_path))
        .unwrap_or_else(|| vec!["pg_catalog".to_string(), default_schema.clone()]);

    let parts: Vec<String> = name
        .0
        .iter()
        .filter_map(|p| p.as_ident().map(|i| i.value.clone()))
        .collect();
    match parts.as_slice() {
        [catalog, schema, table, ..] => {
            if table_exists(ctx, catalog, schema, table) {
                Some(schema.clone())
            } else {
                None
            }
        }
        [schema, table] => {
            if table_exists(ctx, default_catalog, schema, table) {
                Some(schema.clone())
            } else {
                None
            }
        }
        [table] => {
            for sp in &search_path {
                let schema_name = if sp == "$user" { default_schema } else { sp };
                if table_exists(ctx, default_catalog, schema_name, table) {
                    return Some(schema_name.to_string());
                }
            }
            None
        }
        _ => None,
    }
}

/// Returns `true` when `schema` names one of the catalog schemas that this
/// router special-cases (`pg_catalog` or `information_schema`), case-insensitively.
fn schema_is_catalog(schema: &str) -> bool {
    schema.eq_ignore_ascii_case("pg_catalog") || schema.eq_ignore_ascii_case("information_schema")
}

/// Returns `true` when `full` (a `schema.function` name) is registered on `ctx`
/// as any kind of function: scalar UDF, aggregate, table function, or window UDF.
fn function_registered(ctx: &SessionContext, full: &str) -> bool {
    ctx.udf(full).is_ok()
        || ctx.udaf(full).is_ok()
        || ctx.table_function(full).is_ok()
        || ctx.udwf(full).is_ok()
}

/// Determine if the object belongs to `pg_catalog` or `information_schema`.
fn object_is_catalog(ctx: &SessionContext, name: &ObjectName) -> bool {
    resolve_schema(ctx, name)
        .map(|schema| schema_is_catalog(&schema))
        .unwrap_or(false)
}

/// Determine if the function belongs to `pg_catalog` or `information_schema`.
fn function_is_catalog(ctx: &SessionContext, name: &ObjectName) -> bool {
    let parts: Vec<String> = name
        .0
        .iter()
        .filter_map(|p| p.as_ident().map(|i| i.value.clone()))
        .collect();

    match parts.as_slice() {
        [schema, func] | [_, schema, func] => {
            schema_is_catalog(schema) && function_registered(ctx, &format!("{schema}.{func}"))
        }
        [func] => ["pg_catalog", "information_schema"]
            .iter()
            .any(|schema| function_registered(ctx, &format!("{schema}.{func}"))),
        _ => false,
    }
}

/// Returns `true` if the table factor contains catalog tables.
fn factor_has_catalog(ctx: &SessionContext, factor: &TableFactor) -> bool {
    match factor {
        TableFactor::Table { name, .. } => object_is_catalog(ctx, name),
        TableFactor::Derived { subquery, .. } => query_has_catalog(ctx, subquery),
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => table_with_joins_contains_catalog(ctx, table_with_joins),
        _ => false,
    }
}

/// Check whether any table in a join tree references catalog tables.
fn table_with_joins_contains_catalog(
    ctx: &SessionContext,
    table_with_joins: &TableWithJoins,
) -> bool {
    if factor_has_catalog(ctx, &table_with_joins.relation) {
        return true;
    }
    for join in &table_with_joins.joins {
        if factor_has_catalog(ctx, &join.relation) {
            return true;
        }
    }
    false
}

/// Determine if a `SetExpr` references catalog tables.
fn setexpr_has_catalog(ctx: &SessionContext, expr: &SetExpr) -> bool {
    match expr {
        SetExpr::Select(select) => {
            for twj in &select.from {
                if table_with_joins_contains_catalog(ctx, twj) {
                    return true;
                }
            }
            false
        }
        SetExpr::Query(query) => query_has_catalog(ctx, query),
        SetExpr::SetOperation { left, right, .. } => {
            setexpr_has_catalog(ctx, left) || setexpr_has_catalog(ctx, right)
        }
        _ => false,
    }
}

/// Recursively inspect a [`Query`] for catalog table references.
fn query_has_catalog(ctx: &SessionContext, query: &Query) -> bool {
    if setexpr_has_catalog(ctx, &query.body) {
        return true;
    }
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            if query_has_catalog(ctx, &cte.query) {
                return true;
            }
        }
    }
    false
}

/// Inspect a [`Statement`] and return true if it touches catalog tables.
fn statement_has_catalog(ctx: &SessionContext, stmt: &Statement) -> bool {
    use sqlparser::ast::{visit_expressions, Expr};
    use std::ops::ControlFlow;

    if matches!(stmt, Statement::Query(q) if query_has_catalog(ctx, q)) {
        return true;
    }

    let mut found = false;
    let _ = visit_expressions(stmt, |expr| {
        if let Expr::Function(func) = expr {
            if function_is_catalog(ctx, &func.name) {
                found = true;
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    });

    found
}

/// Parse the SQL string and check whether it references catalog tables.
fn is_catalog_query(ctx: &SessionContext, sql: &str) -> datafusion::error::Result<bool> {
    let dialect = PostgreSqlDialect {};
    let statements = Parser::parse_sql(&dialect, sql).map_err(|e| {
        datafusion::error::DataFusionError::Plan(format!("failed to parse SQL: {e}"))
    })?;
    Ok(statements.iter().any(|s| statement_has_catalog(ctx, s)))
}

/// Dispatch `sql` either to the internal catalog handler or to a caller supplied
/// handler.
///
/// When `sql` references `pg_catalog`/`information_schema`, it is executed with
/// [`execute_sql`]. Otherwise the provided `handler` is awaited with the
/// unmodified SQL.
pub async fn dispatch_query<'a, F, Fut>(
    ctx: &'a SessionContext,
    sql: &'a str,
    params: Option<Vec<Option<Bytes>>>,
    param_types: Option<Vec<Type>>,
    handler: F,
) -> datafusion::error::Result<(Vec<RecordBatch>, Arc<Schema>)>
where
    F: Fn(&'a SessionContext, &'a str, Option<Vec<Option<Bytes>>>, Option<Vec<Type>>) -> Fut,
    Fut: std::future::Future<Output = datafusion::error::Result<(Vec<RecordBatch>, Arc<Schema>)>>,
{
    if is_catalog_query(ctx, sql)? {
        debug!("is_catalog_query True");
        let qualified_sql = qualify_catalog_tables(ctx, sql)?;
        execute_sql(ctx, &qualified_sql, params, param_types).await
    } else {
        debug!("is_catalog_query False");
        handler(ctx, sql, params, param_types).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::register_table::register_table;
    use crate::user_functions::register_version_fn;
    use arrow::datatypes::DataType;
    use std::sync::{Arc, Mutex};

    #[tokio::test(flavor = "multi_thread")]
    async fn test_dispatch_user_table_calls_handler() -> datafusion::error::Result<()> {
        let ctx = SessionContext::new();
        register_table(
            &ctx,
            "crm",
            "crm",
            "users",
            vec![("id", DataType::Int32, false)],
        )?;

        let called = Arc::new(Mutex::new(false));
        let called_clone = called.clone();
        let handler = move |_ctx: &SessionContext, _sql: &str, _p, _t| {
            let called_clone = called_clone.clone();
            async move {
                *called_clone.lock().unwrap() = true;
                Ok((Vec::new(), Arc::new(Schema::empty())))
            }
        };

        let _ = dispatch_query(&ctx, "SELECT * FROM users", None, None, handler).await?;
        assert!(*called.lock().unwrap());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_dispatch_catalog_query_internal() -> datafusion::error::Result<()> {
        let ctx = SessionContext::new();
        register_table(
            &ctx,
            "datafusion",
            "pg_catalog",
            "pg_class",
            vec![("oid", DataType::Int32, false)],
        )?;
        let called = Arc::new(Mutex::new(false));
        let called_clone = called.clone();
        let handler = move |_ctx: &SessionContext, _sql: &str, _p, _t| {
            let called_clone = called_clone.clone();
            async move {
                *called_clone.lock().unwrap() = true;
                Ok((Vec::new(), Arc::new(Schema::empty())))
            }
        };

        let _ = dispatch_query(
            &ctx,
            "SELECT * FROM pg_catalog.pg_class",
            None,
            None,
            handler,
        )
        .await?;
        assert!(!*called.lock().unwrap());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_dispatch_unqualified_catalog_query_internal() -> datafusion::error::Result<()> {
        let ctx = SessionContext::new();
        register_table(
            &ctx,
            "datafusion",
            "pg_catalog",
            "pg_class",
            vec![("oid", DataType::Int32, false)],
        )?;

        let called = Arc::new(Mutex::new(false));
        let called_clone = called.clone();
        let handler = move |_ctx: &SessionContext, _sql: &str, _p, _t| {
            let called_clone = called_clone.clone();
            async move {
                *called_clone.lock().unwrap() = true;
                Ok((Vec::new(), Arc::new(Schema::empty())))
            }
        };

        let _ = dispatch_query(&ctx, "SELECT * FROM pg_class", None, None, handler).await?;
        assert!(!*called.lock().unwrap());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_dispatch_user_table_precedence() -> datafusion::error::Result<()> {
        let mut config = datafusion::execution::context::SessionConfig::new()
            .with_default_catalog_and_schema("datafusion", "public")
            .with_option_extension(ClientOpts::default());

        if let Some(opts) = config.options_mut().extensions.get_mut::<ClientOpts>() {
            opts.search_path = "crm, pg_catalog".to_string();
        }

        let ctx = SessionContext::new_with_config(config);
        register_table(
            &ctx,
            "datafusion",
            "pg_catalog",
            "pg_class",
            vec![("oid", DataType::Int32, false)],
        )?;
        register_table(
            &ctx,
            "datafusion",
            "crm",
            "pg_class",
            vec![("id", DataType::Int32, false)],
        )?;

        let called = Arc::new(Mutex::new(false));
        let called_clone = called.clone();
        let handler = move |_ctx: &SessionContext, _sql: &str, _p, _t| {
            let called_clone = called_clone.clone();
            async move {
                *called_clone.lock().unwrap() = true;
                Ok((Vec::new(), Arc::new(Schema::empty())))
            }
        };

        let _ = dispatch_query(&ctx, "SELECT * FROM pg_class", None, None, handler).await?;
        assert!(*called.lock().unwrap());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_dispatch_with_params_passes_to_handler() -> datafusion::error::Result<()> {
        let ctx = SessionContext::new();
        register_table(
            &ctx,
            "crm",
            "crm",
            "users",
            vec![("id", DataType::Int32, false)],
        )?;

        let captured: Arc<Mutex<Option<Vec<Option<Bytes>>>>> = Arc::new(Mutex::new(None));
        let captured_clone = captured.clone();
        let handler = move |_ctx: &SessionContext, _sql: &str, params, _types| {
            let captured_clone = captured_clone.clone();
            async move {
                *captured_clone.lock().unwrap() = params;
                Ok((Vec::new(), Arc::new(Schema::empty())))
            }
        };

        let params = vec![Some(Bytes::from("1"))];
        let types = vec![Type::INT4];
        let _ = dispatch_query(
            &ctx,
            "SELECT * FROM users WHERE id=$1",
            Some(params.clone()),
            Some(types),
            handler,
        )
        .await?;
        assert_eq!(*captured.lock().unwrap(), Some(params));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_dispatch_catalog_function_internal() -> datafusion::error::Result<()> {
        let ctx = SessionContext::new();
        register_version_fn(&ctx)?;

        let called = Arc::new(Mutex::new(false));
        let called_clone = called.clone();
        let handler = move |_ctx: &SessionContext, _sql: &str, _p, _t| {
            let called_clone = called_clone.clone();
            async move {
                *called_clone.lock().unwrap() = true;
                Ok((Vec::new(), Arc::new(Schema::empty())))
            }
        };

        let _ = dispatch_query(&ctx, "SELECT pg_catalog.version()", None, None, handler).await?;
        assert!(!*called.lock().unwrap());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_dispatch_unqualified_catalog_function_internal() -> datafusion::error::Result<()>
    {
        let ctx = SessionContext::new();
        register_version_fn(&ctx)?;

        let called = Arc::new(Mutex::new(false));
        let called_clone = called.clone();
        let handler = move |_ctx: &SessionContext, _sql: &str, _p, _t| {
            let called_clone = called_clone.clone();
            async move {
                *called_clone.lock().unwrap() = true;
                Ok((Vec::new(), Arc::new(Schema::empty())))
            }
        };

        let _ = dispatch_query(&ctx, "SELECT version()", None, None, handler).await?;
        assert!(!*called.lock().unwrap());
        Ok(())
    }
}
