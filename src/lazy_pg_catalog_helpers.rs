use std::sync::Arc;

use datafusion::error::Result as DFResult;
use datafusion::execution::context::SessionContext;

use crate::lazy_catalog::{
    register_lazy_catalog, CatalogTable, ColumnSpec, DatabaseDef, LazyCatalogOptions,
    LazyCatalogSource, RelationDef, SchemaDef,
};

/// A single database row for lazy population of `pg_catalog.pg_database`.
///
/// Columns follow PostgreSQL semantics and types closely. `oid`, `datname`,
/// and `datdba` are mandatory (TODO: Make `datdba` optional in the future);
/// the rest are optional and will fall back to sensible defaults.
///
/// Every object in a catalog has an OID by definition, so `oid` is a plain
/// `i32`: a NULL `pg_database.oid` must be unrepresentable, not merely
/// discouraged.
///
/// Column reference:
/// | Column             | Type        | Description                                                                                                                         |
/// | ------------------ | ----------- | ----------------------------------------------------------------------------------------------------------------------------------- |
/// | oid                | oid         | Unique object identifier for the database (used internally and by catalog joins).                                                   |
/// | datname            | name        | Name of the database.                                                                                                               |
/// | datdba             | oid         | OID of the owner (role) - references `pg_authid.oid`.                                                                               |
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
/// | daticurules        | text        | ICU collation rules (if applicable).                                                                                               |
/// | datcollversion     | text        | Collation version used when the database was created.                                                                               |
/// | datacl             | aclitem[]   | Access privileges for roles (GRANT/REVOKE).                                                                                         |
#[derive(Clone, Debug)]
pub struct LazyDatabaseRow {
    pub oid: i32,
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
    /// Construct a database row from its mandatory `oid`, `datname`, and `datdba`,
    /// leaving every optional field unset (each falls back to a
    /// PostgreSQL-compatible default when the row is materialized).
    pub fn new(oid: i32, datname: impl Into<String>, datdba: i32) -> Self {
        Self {
            oid,
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
/// The callback returns rich `LazyDatabaseRow` entries; only `oid`, `datname`,
/// and `datdba` are mandatory; missing fields default to PostgreSQL-compatible
/// values (e.g., `encoding=6` UTF8, `datistemplate=false`, `datallowconn=true`).
///
/// This is a thin shim over [`register_lazy_catalog`]: the callback rows are
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
