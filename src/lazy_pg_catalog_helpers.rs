use std::sync::Arc;

use async_trait::async_trait;
use datafusion::catalog::Session;
use datafusion::datasource::provider::TableProviderFilterPushDown;
use datafusion::datasource::TableProvider;
use datafusion::datasource::TableType;
use datafusion::error::Result as DFResult;
use datafusion::execution::context::SessionContext;
use datafusion::logical_expr::dml::InsertOp;
use datafusion::logical_expr::Expr;
use datafusion::physical_plan::ExecutionPlan;

use crate::lazy_catalog::{
    register_lazy_catalog, CatalogTable, ColumnSpec, DatabaseDef, LazyCatalogOptions,
    LazyCatalogSource, RelationDef, SchemaDef,
};

/// A table provider wrapper that, on scan, invokes a user-supplied callback
/// to fetch database names and ensures they are registered in pg_database
/// before delegating to the underlying table.
///
/// This enables lazy population of `pg_catalog.pg_database`.
pub struct LazyDatabaseProvider {
    // Keep inner only for schema/type metadata; scans are generated from callback rows.
    inner: Arc<dyn TableProvider>,
    fetcher: Arc<dyn Fn() -> Vec<LazyDatabaseRow> + Send + Sync>,
}

impl std::fmt::Debug for LazyDatabaseProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyDatabaseProvider").finish()
    }
}

impl LazyDatabaseProvider {
    /// Construct a legacy single-table lazy provider over `inner`.
    ///
    /// Retained for backward compatibility. `register_user_database_with_callback`
    /// now delegates to the generic [`crate::lazy_catalog::register_lazy_catalog`]
    /// mechanism (which merges with built-in rows), so this constructor is no
    /// longer wired into the default path.
    #[allow(dead_code)]
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
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> DFResult<Arc<dyn ExecutionPlan>> {
        use arrow::array::{new_null_array, ArrayRef, BooleanBuilder, Int32Builder, StringBuilder};
        use datafusion::datasource::MemTable;

        // Build RecordBatch from callback rows without touching underlying table.
        let rows = (self.fetcher)();

        let mut oid_b = Int32Builder::new();
        let mut datname_b = StringBuilder::new();
        let mut datdba_b = Int32Builder::new();
        let mut encoding_b = Int32Builder::new();
        let mut datlocprovider_b = StringBuilder::new();
        let mut datistemplate_b = BooleanBuilder::new();
        let mut datallowconn_b = BooleanBuilder::new();
        let mut dathasloginevt_b = BooleanBuilder::new();
        let mut datconnlimit_b = Int32Builder::new();
        let mut datfrozenxid_b = StringBuilder::new();
        let mut datminmxid_b = StringBuilder::new();
        let mut dattablespace_b = Int32Builder::new();
        let mut datcollate_b = StringBuilder::new();
        let mut datctype_b = StringBuilder::new();
        let mut datlocale_b = StringBuilder::new();
        let mut daticurules_b = StringBuilder::new();
        let mut datcollversion_b = StringBuilder::new();

        for r in &rows {
            match r.oid {
                Some(v) => oid_b.append_value(v),
                None => oid_b.append_null(),
            }
            datname_b.append_value(&r.datname);
            datdba_b.append_value(r.datdba);
            encoding_b.append_value(r.encoding.unwrap_or(6));
            if let Some(c) = r.datlocprovider {
                datlocprovider_b.append_value(&c.to_string());
            } else {
                datlocprovider_b.append_null();
            }
            datistemplate_b.append_value(r.datistemplate.unwrap_or(false));
            datallowconn_b.append_value(r.datallowconn.unwrap_or(true));
            dathasloginevt_b.append_value(r.dathasloginevt.unwrap_or(false));
            datconnlimit_b.append_value(r.datconnlimit.unwrap_or(-1));
            datfrozenxid_b
                .append_value(&r.datfrozenxid.clone().unwrap_or_else(|| "726".to_string()));
            datminmxid_b.append_value(&r.datminmxid.clone().unwrap_or_else(|| "1".to_string()));
            dattablespace_b.append_value(r.dattablespace.unwrap_or(1663));
            datcollate_b.append_value(&r.datcollate.clone().unwrap_or_else(|| "C".to_string()));
            datctype_b.append_value(&r.datctype.clone().unwrap_or_else(|| "C".to_string()));
            if let Some(v) = &r.datlocale {
                datlocale_b.append_value(v);
            } else {
                datlocale_b.append_null();
            }
            if let Some(v) = &r.daticurules {
                daticurules_b.append_value(v);
            } else {
                daticurules_b.append_null();
            }
            if let Some(v) = &r.datcollversion {
                datcollversion_b.append_value(v);
            } else {
                datcollversion_b.append_null();
            }
        }

        let schema = self.inner.schema();
        let mut arrays: Vec<ArrayRef> = Vec::new();
        for field in schema.fields() {
            let name = field.name().as_str();
            let arr: ArrayRef = match name {
                "oid" => Arc::new(oid_b.finish()),
                "datname" => Arc::new(datname_b.finish()),
                "datdba" => Arc::new(datdba_b.finish()),
                "encoding" => Arc::new(encoding_b.finish()),
                "datlocprovider" => Arc::new(datlocprovider_b.finish()),
                "datistemplate" => Arc::new(datistemplate_b.finish()),
                "datallowconn" => Arc::new(datallowconn_b.finish()),
                "dathasloginevt" => Arc::new(dathasloginevt_b.finish()),
                "datconnlimit" => Arc::new(datconnlimit_b.finish()),
                "datfrozenxid" => Arc::new(datfrozenxid_b.finish()),
                "datminmxid" => Arc::new(datminmxid_b.finish()),
                "dattablespace" => Arc::new(dattablespace_b.finish()),
                "datcollate" => Arc::new(datcollate_b.finish()),
                "datctype" => Arc::new(datctype_b.finish()),
                "datlocale" => Arc::new(datlocale_b.finish()),
                "daticurules" => Arc::new(daticurules_b.finish()),
                "datcollversion" => Arc::new(datcollversion_b.finish()),
                _ => new_null_array(field.data_type(), rows.len()),
            };
            arrays.push(arr);
        }

        let batch = arrow::record_batch::RecordBatch::try_new(schema.clone(), arrays)?;
        let mem = MemTable::try_new(schema, vec![vec![batch]])?;
        mem.scan(state, projection, &[], None).await
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

/// A [`LazyCatalogSource`] that only contributes databases, adapting the legacy
/// `Fn() -> Vec<LazyDatabaseRow>` callback to the generic lazy catalog trait.
///
/// Schemas/relations/columns are intentionally empty: this source exists solely
/// to back `pg_catalog.pg_database`.
struct DatabaseOnlySource {
    /// The user-supplied callback producing database rows.
    fetch: Arc<dyn Fn() -> Vec<LazyDatabaseRow> + Send + Sync>,
}

impl LazyCatalogSource for DatabaseOnlySource {
    /// Yield the databases produced by the wrapped callback.
    fn databases(&self, callback: &mut dyn FnMut(Vec<DatabaseDef>)) -> DFResult<()> {
        callback((self.fetch)());
        Ok(())
    }

    /// No schemas are contributed by this source.
    fn schemas(&self, _database: &str, _callback: &mut dyn FnMut(Vec<SchemaDef>)) -> DFResult<()> {
        Ok(())
    }

    /// No relations are contributed by this source.
    fn relations(
        &self,
        _database: &str,
        _schema: &str,
        _callback: &mut dyn FnMut(Vec<RelationDef>),
    ) -> DFResult<()> {
        Ok(())
    }

    /// No columns are contributed by this source.
    fn columns(
        &self,
        _database: &str,
        _schema: &str,
        _relation: &str,
        _callback: &mut dyn FnMut(Vec<ColumnSpec>),
    ) -> DFResult<()> {
        Ok(())
    }
}

/// Register a lazy callback so that queries scanning `pg_catalog.pg_database` will
/// invoke `fetch_databases` to populate rows just-in-time.
///
/// The callback returns rich `LazyDatabaseRow` entries; only `datname` and
/// `datdba` are mandatory; missing fields default to PostgreSQL-compatible
/// values (e.g., `encoding=6` UTF8, `datistemplate=false`, `datallowconn=true`).
///
/// This is now a thin shim over [`register_lazy_catalog`]: the callback rows are
/// **merged** with the built-in `pg_database` rows (postgres/template0/template1)
/// rather than replacing them, and they are re-pulled fresh on every scan.
pub async fn register_user_database_with_callback(
    ctx: &SessionContext,
    fetch_databases: Arc<dyn Fn() -> Vec<LazyDatabaseRow> + Send + Sync>,
) -> DFResult<()> {
    let source: Arc<dyn LazyCatalogSource> = Arc::new(DatabaseOnlySource {
        fetch: fetch_databases,
    });
    register_lazy_catalog(
        ctx,
        source,
        LazyCatalogOptions::with_tables(vec![CatalogTable::PgDatabase]),
    )
    .await
}

/// Insert a single database row into `pg_catalog.pg_database` if missing.
/// this is for illustration purposes for now, we'll remove it later
async fn insert_database_row(ctx: &SessionContext, row: &LazyDatabaseRow) -> DFResult<()> {
    // Skip if exists
    let df = ctx
        .sql("SELECT 1 FROM pg_catalog.pg_database WHERE datname=$name")
        .await?
        .with_param_values(vec![(
            "name",
            datafusion::common::ScalarValue::from(row.datname.clone()),
        )])?;
    if df.count().await? > 0 {
        return Ok(());
    }

    // Determine OID
    let oid_val: i64 = if let Some(oid) = row.oid {
        oid as i64
    } else {
        let getiddf = ctx
            .sql("select max(oid)+1 from pg_catalog.pg_database")
            .await?;
        let batches = getiddf.collect().await?;
        let array = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap();
        array.value(0)
    };

    // Prepare values with defaults
    fn esc(s: &str) -> String {
        s.replace('\'', "''")
    }
    let datname = esc(&row.datname);
    let datdba = row.datdba;
    let encoding = row.encoding.unwrap_or(6);
    let datistemplate = row.datistemplate.unwrap_or(false);
    let datallowconn = row.datallowconn.unwrap_or(true);
    let datconnlimit = row.datconnlimit.unwrap_or(-1);
    let datfrozenxid = row
        .datfrozenxid
        .clone()
        .unwrap_or_else(|| "726".to_string());
    let datminmxid = row.datminmxid.clone().unwrap_or_else(|| "1".to_string());
    let dattablespace = row.dattablespace.unwrap_or(1663);
    let datcollate = row.datcollate.clone().unwrap_or_else(|| "C".to_string());
    let datctype = row.datctype.clone().unwrap_or_else(|| "C".to_string());
    let datacl = row.datacl.clone();
    let datacl_sql = if let Some(items) = datacl {
        format!(
            "ARRAY[{}]",
            items
                .into_iter()
                .map(|s| format!("'{}'", esc(&s)))
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        "NULL".to_string()
    };

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
