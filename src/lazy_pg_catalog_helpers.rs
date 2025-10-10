use std::sync::Arc;

use async_trait::async_trait;
use datafusion::catalog::Session;
use datafusion::datasource::provider::TableProviderFilterPushDown;
use datafusion::datasource::TableProvider;
use datafusion::datasource::TableType;
use datafusion::error::{DataFusionError, Result as DFResult};
use datafusion::execution::context::SessionContext;
use datafusion::logical_expr::dml::InsertOp;
use datafusion::logical_expr::Expr;
use datafusion::physical_plan::ExecutionPlan;

/// A table provider wrapper that, on scan, invokes a user-supplied callback
/// to fetch database names and ensures they are registered in pg_database
/// before delegating to the underlying table.
///
/// This enables lazy population of `pg_catalog.pg_database`.
pub struct LazyDatabaseProvider {
    inner: Arc<dyn TableProvider>,
    fetcher: Arc<dyn Fn() -> Vec<LazyDatabaseRow> + Send + Sync>,
}

impl std::fmt::Debug for LazyDatabaseProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyDatabaseProvider").finish()
    }
}

impl LazyDatabaseProvider {
    fn new(
        inner: Arc<dyn TableProvider>,
        fetcher: Arc<dyn Fn() -> Vec<LazyDatabaseRow> + Send + Sync>,
    ) -> Self {
        Self { inner, fetcher }
    }
}

#[async_trait]
impl TableProvider for LazyDatabaseProvider {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn schema(&self) -> arrow::datatypes::SchemaRef {
        self.inner.schema()
    }

    fn table_type(&self) -> TableType {
        self.inner.table_type()
    }

    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> DFResult<Vec<TableProviderFilterPushDown>> {
        self.inner.supports_filters_pushdown(filters)
    }

    async fn scan(
        &self,
        state: &dyn datafusion::catalog::Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> DFResult<Arc<dyn ExecutionPlan>> {
        // Attempt to acquire a SessionContext to run registrations.
        if let Some(ctx) = state.as_any().downcast_ref::<SessionContext>() {
            // Fetch database rows lazily and register each.
            let rows = (self.fetcher)();
            for row in rows {
                insert_database_row(ctx, &row).await?;
            }
        }
        // Delegate to the underlying provider.
        self.inner.scan(state, projection, filters, limit).await
    }

    async fn insert_into(
        &self,
        state: &dyn Session,
        input: Arc<dyn ExecutionPlan>,
        insert_op: InsertOp,
    ) -> DFResult<Arc<dyn ExecutionPlan>> {
        // Forward inserts to the underlying provider so that SQL-based updates work.
        self.inner.insert_into(state, input, insert_op).await
    }

    // No custom delete/statistics; rely on inner provider behavior.
}

/// A single database row for lazy population of `pg_catalog.pg_database`.
///
/// Columns follow PostgreSQL semantics and types closely. `oid`, `datname`,
/// and `datdba` are mandatory (TODO: Make `datdba` optional in the future),
/// the rest are optional and will fall back to sensible defaults.
///
/// Column reference:
/// | Column             | Type        | Description                                                                                                                         |
/// | ------------------ | ----------- | ----------------------------------------------------------------------------------------------------------------------------------- |
/// | oid                | oid         | Unique object identifier for the database (used internally and by catalog joins).                                                   |
/// | datname            | name        | Name of the database.                                                                                                               |
/// | datdba             | oid         | OID of the owner (role) — references `pg_authid.oid`.                                                                               |
/// | encoding           | int4        | Database character encoding (integer code). Use `pg_encoding_to_char(encoding)` to see the name.                                    |
/// | datlocprovider     | char        | Locale provider: 'c' (C library), 'i' (ICU).                                                                                        |
/// | datistemplate      | bool        | If true, database can be used as a template.                                                                                        |
/// | datallowconn       | bool        | If false, new connections are disallowed.                                                                                           |
/// | dathasloginevt     | bool        | If true, generate login events.                                                                                                     |
/// | datconnlimit       | int4        | Max concurrent connections (-1 = no limit).                                                                                         |
/// | datfrozenxid       | xid         | Oldest transaction ID considered frozen (stringified).                                                                              |
/// | datminmxid         | xid         | Minimum multixact ID to keep (stringified).                                                                                         |
/// | dattablespace      | oid         | Default tablespace OID.                                                                                                             |
/// | datcollate         | text        | LC_COLLATE setting.                                                                                                                 |
/// | datctype           | text        | LC_CTYPE setting.                                                                                                                   |
/// | datlocale          | text        | ICU locale identifier (if applicable).                                                                                              |
/// | daticurules        | text        | ICU collation rules (if applicable).                                                                                                |
/// | datcollversion     | text        | Collation version used when the database was created.                                                                               |
/// | datacl             | aclitem[]   | Access privileges for roles (GRANT/REVOKE).                                                                                         |
#[derive(Clone, Debug)]
pub struct LazyDatabaseRow {
    pub oid: Option<i32>,
    pub datname: String,
    pub datdba: i32, // TODO: make optional in future
    pub encoding: Option<i32>,
    pub datlocprovider: Option<char>,
    pub datistemplate: Option<bool>,
    pub datallowconn: Option<bool>,
    pub dathasloginevt: Option<bool>,
    pub datconnlimit: Option<i32>,
    pub datfrozenxid: Option<String>,
    pub datminmxid: Option<String>,
    pub dattablespace: Option<i32>,
    pub datcollate: Option<String>,
    pub datctype: Option<String>,
    pub datlocale: Option<String>,
    pub daticurules: Option<String>,
    pub datcollversion: Option<String>,
    pub datacl: Option<Vec<String>>,
}

impl LazyDatabaseRow {
    pub fn new(datname: impl Into<String>, datdba: i32) -> Self {
        Self {
            oid: None,
            datname: datname.into(),
            datdba,
            encoding: None,
            datlocprovider: None,
            datistemplate: None,
            datallowconn: None,
            dathasloginevt: None,
            datconnlimit: None,
            datfrozenxid: None,
            datminmxid: None,
            dattablespace: None,
            datcollate: None,
            datctype: None,
            datlocale: None,
            daticurules: None,
            datcollversion: None,
            datacl: None,
        }
    }
}

/// Register a lazy callback so that queries scanning `pg_catalog.pg_database` will
/// invoke `fetch_databases` to populate rows just-in-time.
///
/// The callback returns rich `LazyDatabaseRow` entries; only `datname` and
/// `datdba` are mandatory; missing fields default to PostgreSQL-compatible
/// values (e.g., `encoding=6` UTF8, `datistemplate=false`, `datallowconn=true`).
pub async fn register_user_database_with_callback(
    ctx: &SessionContext,
    fetch_databases: Arc<dyn Fn() -> Vec<LazyDatabaseRow> + Send + Sync>,
) -> DFResult<()> {
    set_database_fetcher(fetch_databases.clone());

    // Try to wrap the provider now so that subsequent scans are lazy.
    let state = ctx.state();
    let options = state.config_options();
    let default_catalog = &options.catalog.default_catalog;

    if let Some(catalog) = ctx.catalog(default_catalog) {
        if let Some(schema) = catalog.schema("pg_catalog") {
            if let Some(current) = schema.table("pg_database").await? {
                let wrapped: Arc<dyn TableProvider> =
                    Arc::new(LazyDatabaseProvider::new(current.clone(), fetch_databases));
                // Try to deregister existing then register our wrapper. If deregister is not
                // supported, fall back to just registering (may error in that case).
                let _ = schema.deregister_table("pg_database");
                let _ = schema.register_table("pg_database".to_string(), wrapped);
            }
        }
    }
    Ok(())
}

use once_cell::sync::Lazy;
use std::sync::Mutex;

static DB_FETCHER: Lazy<Mutex<Option<Arc<dyn Fn() -> Vec<LazyDatabaseRow> + Send + Sync>>>> =
    Lazy::new(|| Mutex::new(None));

fn set_database_fetcher(fetcher: Arc<dyn Fn() -> Vec<LazyDatabaseRow> + Send + Sync>) {
    let mut guard = DB_FETCHER.lock().unwrap();
    *guard = Some(fetcher);
}

/// If a fetcher is registered, call it and ensure `pg_database` has
/// corresponding rows registered via the existing helper.
pub async fn maybe_refresh_pg_database(ctx: &SessionContext) -> DFResult<()> {
    let fetcher = { DB_FETCHER.lock().unwrap().clone() };
    if let Some(f) = fetcher {
        for row in (f)() {
            let _ = insert_database_row(ctx, &row).await;
        }
    }
    Ok(())
}

/// Wrap a table provider for `pg_catalog.pg_database` with a lazy provider if a
/// fetcher is registered; otherwise return the original provider.
pub fn wrap_pg_database_provider_if_lazy(
    inner: Arc<dyn TableProvider>,
) -> Arc<dyn TableProvider> {
    let fetcher = { DB_FETCHER.lock().unwrap().clone() };
    if let Some(f) = fetcher {
        Arc::new(LazyDatabaseProvider::new(inner, f)) as Arc<dyn TableProvider>
    } else {
        inner
    }
}

/// Return current database list from the registered fetcher, if any.
pub fn current_database_rows() -> Option<Vec<LazyDatabaseRow>> {
    DB_FETCHER.lock().unwrap().as_ref().map(|f| (f)())
}

/// Insert a single database row into `pg_catalog.pg_database` if missing.
async fn insert_database_row(ctx: &SessionContext, row: &LazyDatabaseRow) -> DFResult<()> {
    // Skip if exists
    let df = ctx
        .sql("SELECT 1 FROM pg_catalog.pg_database WHERE datname=$name")
        .await?
        .with_param_values(vec![("name", datafusion::common::ScalarValue::from(row.datname.clone()))])?;
    if df.count().await? > 0 {
        return Ok(());
    }

    // Determine OID
    let oid_val: i64 = if let Some(oid) = row.oid { oid as i64 } else {
        let getiddf = ctx.sql("select max(oid)+1 from pg_catalog.pg_database").await?;
        let batches = getiddf.collect().await?;
        let array = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap();
        array.value(0)
    };

    // Prepare values with defaults
    fn esc(s: &str) -> String { s.replace('\'', "''") }
    let datname = esc(&row.datname);
    let datdba = row.datdba;
    let encoding = row.encoding.unwrap_or(6);
    let datistemplate = row.datistemplate.unwrap_or(false);
    let datallowconn = row.datallowconn.unwrap_or(true);
    let datconnlimit = row.datconnlimit.unwrap_or(-1);
    let datfrozenxid = row.datfrozenxid.clone().unwrap_or_else(|| "726".to_string());
    let datminmxid = row.datminmxid.clone().unwrap_or_else(|| "1".to_string());
    let dattablespace = row.dattablespace.unwrap_or(1663);
    let datcollate = row.datcollate.clone().unwrap_or_else(|| "C".to_string());
    let datctype = row.datctype.clone().unwrap_or_else(|| "C".to_string());
    let datacl = row.datacl.clone();
    let datacl_sql = if let Some(items) = datacl { format!("ARRAY[{}]", items.into_iter().map(|s| format!("'{}'", esc(&s))).collect::<Vec<_>>().join(", ")) } else { "NULL".to_string() };

    let sql = format!(
        "INSERT INTO pg_catalog.pg_database (
            oid, datname, datdba, encoding,
            datistemplate, datallowconn, datconnlimit,
            datfrozenxid, datminmxid, dattablespace,
            datcollate, datctype, datacl
        ) VALUES (
            {oid}, '{datname}', {datdba}, {encoding},
            {datistemplate}, {datallowconn}, {datconnlimit},
            '{datfrozenxid}', '{datminmxid}', {dattablespace},
            '{datcollate}', '{datctype}', {datacl}
        )",
        oid = oid_val,
        datname = datname,
        datdba = datdba,
        encoding = encoding,
        datistemplate = if datistemplate { "true" } else { "false" },
        datallowconn = if datallowconn { "true" } else { "false" },
        datconnlimit = datconnlimit,
        datfrozenxid = esc(&datfrozenxid),
        datminmxid = esc(&datminmxid),
        dattablespace = dattablespace,
        datcollate = esc(&datcollate),
        datctype = esc(&datctype),
        datacl = datacl_sql,
    );

    ctx.sql(&sql).await?.collect().await?;
    Ok(())
}
