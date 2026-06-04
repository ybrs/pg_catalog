//! Lazy, callback-driven catalog definitions.
//!
//! Instead of eagerly pre-registering every database/schema/relation/column,
//! a consumer implements [`LazyCatalogSource`] once and registers it with
//! [`register_lazy_catalog`]. From then on, every scan of a catalog table
//! re-invokes the source, builds Arrow rows from whatever it returns, and serves
//! them *merged* with the built-in system rows captured at registration time.
//!
//! The contract here is deliberately backend-agnostic and connection-free: it
//! speaks only in catalog concepts (database/schema/relation/column names and
//! OIDs). What backs a source — an embedded SQL engine, a network service, a
//! file, an in-memory `Vec`, or nothing — is entirely opaque to `pg_catalog`.
//!
//! OIDs are supplied by the source and written through verbatim; `pg_catalog`
//! never invents, derives, caches, or remembers them. Keeping OIDs stable and
//! consistent across calls (so cross-table joins resolve) is the source's job.

use std::sync::Arc;

use arrow::datatypes::SchemaRef;
use arrow::record_batch::RecordBatch;
use async_trait::async_trait;
use datafusion::catalog::Session;
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::error::{DataFusionError, Result as DFResult};
use datafusion::execution::context::SessionContext;
use datafusion::logical_expr::Expr;
use datafusion::physical_plan::{collect, ExecutionPlan};
use serde_json::{json, Value};

use crate::lazy_pg_catalog_helpers::LazyDatabaseRow;
use crate::pg_catalog_helpers::oid_to_type_names;
use crate::session::rows_to_record_batch;

/// A single catalog row: a `column name -> JSON value` map, matching the shape
/// consumed by [`rows_to_record_batch`]. Columns not present default to NULL.
pub type Row = std::collections::BTreeMap<String, Value>;

/// One user database fed into `pg_catalog.pg_database`.
///
/// This is an alias for [`LazyDatabaseRow`] so the rich optional metadata
/// already modeled there (encoding, collation, template flags, ...) is reused
/// verbatim. Only `datname`/`datdba` are mandatory; the rest default to
/// PostgreSQL-compatible values. The `oid` is user-supplied.
pub type DatabaseDef = LazyDatabaseRow;

/// One user schema fed into `pg_catalog.pg_namespace`. `oid` is user-supplied.
#[derive(Clone, Debug)]
pub struct SchemaDef {
    /// The schema's `pg_namespace.oid`. Must be stable and consistent so
    /// `pg_class.relnamespace` joins resolve against it.
    pub oid: i32,
    /// The schema name (`pg_namespace.nspname`).
    pub name: String,
    /// Owning role OID (`pg_namespace.nspowner`); defaults to 10 when `None`.
    pub owner_oid: Option<i32>,
}

impl SchemaDef {
    /// Construct a schema definition from an OID and name, leaving the owner
    /// unset (it will default to role OID 10).
    pub fn new(oid: i32, name: impl Into<String>) -> Self {
        Self {
            oid,
            name: name.into(),
            owner_oid: None,
        }
    }
}

/// What kind of relation a [`RelationDef`] describes, selecting
/// `pg_class.relkind` and the `information_schema.tables.table_type` label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelationKind {
    /// An ordinary table (`relkind = 'r'`).
    Table,
    /// A view (`relkind = 'v'`).
    View,
    /// A materialized view (`relkind = 'm'`).
    MaterializedView,
}

impl RelationKind {
    /// The single-character `pg_class.relkind` code for this relation kind.
    pub fn relkind(&self) -> &'static str {
        match self {
            RelationKind::Table => "r",
            RelationKind::View => "v",
            RelationKind::MaterializedView => "m",
        }
    }

    /// The `information_schema.tables.table_type` label for this relation kind.
    pub fn table_type(&self) -> &'static str {
        match self {
            RelationKind::Table => "BASE TABLE",
            RelationKind::View => "VIEW",
            RelationKind::MaterializedView => "MATERIALIZED VIEW",
        }
    }
}

/// One user relation fed into `pg_class` (plus a composite rowtype in `pg_type`).
///
/// `oid` is the `pg_class.oid`; `reltype_oid` is the rowtype's `pg_type.oid`.
/// Both are user-supplied and written through verbatim.
#[derive(Clone, Debug)]
pub struct RelationDef {
    /// The relation's `pg_class.oid`.
    pub oid: i32,
    /// The rowtype's `pg_type.oid`, written to `pg_class.reltype`.
    pub reltype_oid: i32,
    /// The relation name (`pg_class.relname`).
    pub name: String,
    /// Whether this is a table/view/materialized view.
    pub kind: RelationKind,
}

impl RelationDef {
    /// Construct a `Table` relation definition from its OID, rowtype OID, and
    /// name. Use the struct literal directly for views/materialized views.
    pub fn table(oid: i32, reltype_oid: i32, name: impl Into<String>) -> Self {
        Self {
            oid,
            reltype_oid,
            name: name.into(),
            kind: RelationKind::Table,
        }
    }
}

/// One column fed into `pg_attribute` (plus `information_schema.columns`).
///
/// `attrelid` comes from the owning [`RelationDef::oid`]; `attnum` from the
/// column's ordinal position. The column's type is given as a `pg_type` OID the
/// source chooses (e.g. 23 for int4), so `pg_catalog` need not know the source's
/// type system.
#[derive(Clone, Debug)]
pub struct ColumnSpec {
    /// The column name (`pg_attribute.attname`).
    pub name: String,
    /// The column's `pg_type` OID, written to `pg_attribute.atttypid`.
    pub type_oid: i32,
    /// Whether the column admits NULLs (`pg_attribute.attnotnull` is its negation).
    pub nullable: bool,
}

impl ColumnSpec {
    /// Construct a column specification from a name, `pg_type` OID, and
    /// nullability.
    pub fn new(name: impl Into<String>, type_oid: i32, nullable: bool) -> Self {
        Self {
            name: name.into(),
            type_oid,
            nullable,
        }
    }
}

/// Abstract source of *user* catalog metadata, backend-agnostic and
/// connection-free. Each method takes a `callback` and calls it with the objects
/// it found. How the implementor produces them (SQL engine, service, file,
/// in-memory, or empty) is opaque to `pg_catalog`. Built-in system rows are added
/// by the layer, so implementors return ONLY their own objects.
///
/// Errors are returned as `DataFusionError` and propagate to the client — a
/// source must never fail silently. A method with nothing to contribute simply
/// returns `Ok(())` without invoking its callback (or invokes it with an empty
/// vector).
pub trait LazyCatalogSource: Send + Sync {
    /// User databases -> `pg_catalog.pg_database`.
    fn databases(&self, callback: &mut dyn FnMut(Vec<DatabaseDef>)) -> DFResult<()>;

    /// User schemas in `database` -> `pg_catalog.pg_namespace`.
    fn schemas(&self, database: &str, callback: &mut dyn FnMut(Vec<SchemaDef>)) -> DFResult<()>;

    /// User relations in `database`.`schema` -> `pg_class` + `pg_type`.
    fn relations(
        &self,
        database: &str,
        schema: &str,
        callback: &mut dyn FnMut(Vec<RelationDef>),
    ) -> DFResult<()>;

    /// Columns of `database`.`schema`.`relation`, in ordinal order ->
    /// `pg_attribute` + `information_schema.columns`.
    fn columns(
        &self,
        database: &str,
        schema: &str,
        relation: &str,
        callback: &mut dyn FnMut(Vec<ColumnSpec>),
    ) -> DFResult<()>;
}

/// Identifies which catalog table a [`LazyCatalogTableProvider`] serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogTable {
    /// `pg_catalog.pg_database`.
    PgDatabase,
    /// `pg_catalog.pg_namespace`.
    PgNamespace,
    /// `pg_catalog.pg_class`.
    PgClass,
    /// `pg_catalog.pg_type`.
    PgType,
    /// `pg_catalog.pg_attribute`.
    PgAttribute,
    /// `information_schema.tables`.
    InformationSchemaTables,
    /// `information_schema.columns`.
    InformationSchemaColumns,
    /// `information_schema.schemata`.
    InformationSchemaSchemata,
}

impl CatalogTable {
    /// The `(schema_name, table_name)` this catalog table lives under, used to
    /// look up and replace its provider during registration.
    pub fn location(&self) -> (&'static str, &'static str) {
        match self {
            CatalogTable::PgDatabase => ("pg_catalog", "pg_database"),
            CatalogTable::PgNamespace => ("pg_catalog", "pg_namespace"),
            CatalogTable::PgClass => ("pg_catalog", "pg_class"),
            CatalogTable::PgType => ("pg_catalog", "pg_type"),
            CatalogTable::PgAttribute => ("pg_catalog", "pg_attribute"),
            CatalogTable::InformationSchemaTables => ("information_schema", "tables"),
            CatalogTable::InformationSchemaColumns => ("information_schema", "columns"),
            CatalogTable::InformationSchemaSchemata => ("information_schema", "schemata"),
        }
    }
}

// --- internal helpers that drive the source's callbacks -------------------

/// Pull the full list of databases from `source`, accumulating across any number
/// of callback invocations.
fn fetch_databases(source: &dyn LazyCatalogSource) -> DFResult<Vec<DatabaseDef>> {
    let mut out = Vec::new();
    source.databases(&mut |rows| out.extend(rows))?;
    Ok(out)
}

/// Pull the schemas of `database` from `source`.
fn fetch_schemas(source: &dyn LazyCatalogSource, database: &str) -> DFResult<Vec<SchemaDef>> {
    let mut out = Vec::new();
    source.schemas(database, &mut |rows| out.extend(rows))?;
    Ok(out)
}

/// Pull the relations of `database`.`schema` from `source`.
fn fetch_relations(
    source: &dyn LazyCatalogSource,
    database: &str,
    schema: &str,
) -> DFResult<Vec<RelationDef>> {
    let mut out = Vec::new();
    source.relations(database, schema, &mut |rows| out.extend(rows))?;
    Ok(out)
}

/// Pull the columns of `database`.`schema`.`relation` from `source`.
fn fetch_columns(
    source: &dyn LazyCatalogSource,
    database: &str,
    schema: &str,
    relation: &str,
) -> DFResult<Vec<ColumnSpec>> {
    let mut out = Vec::new();
    source.columns(database, schema, relation, &mut |rows| out.extend(rows))?;
    Ok(out)
}

// --- pure row builders (oids taken straight from the source objects) ------

/// Build the JSON object value for a list-typed column, or NULL when absent.
fn acl_value(acl: &Option<Vec<String>>) -> Value {
    match acl {
        Some(items) => Value::Array(items.iter().map(|s| json!(s)).collect()),
        None => Value::Null,
    }
}

/// Build one `pg_catalog.pg_database` row from a [`DatabaseDef`], filling unset
/// optional fields with PostgreSQL-compatible defaults.
pub fn build_pg_database_row(def: &DatabaseDef) -> Row {
    let mut row = Row::new();
    row.insert(
        "oid".to_string(),
        def.oid.map(|v| json!(v)).unwrap_or(Value::Null),
    );
    row.insert("datname".to_string(), json!(def.datname));
    row.insert("datdba".to_string(), json!(def.datdba));
    row.insert("encoding".to_string(), json!(def.encoding.unwrap_or(6)));
    row.insert(
        "datlocprovider".to_string(),
        def.datlocprovider
            .map(|c| json!(c.to_string()))
            .unwrap_or(Value::Null),
    );
    row.insert(
        "datistemplate".to_string(),
        json!(def.datistemplate.unwrap_or(false)),
    );
    row.insert(
        "datallowconn".to_string(),
        json!(def.datallowconn.unwrap_or(true)),
    );
    row.insert(
        "dathasloginevt".to_string(),
        json!(def.dathasloginevt.unwrap_or(false)),
    );
    row.insert(
        "datconnlimit".to_string(),
        json!(def.datconnlimit.unwrap_or(-1)),
    );
    row.insert(
        "datfrozenxid".to_string(),
        json!(def
            .datfrozenxid
            .clone()
            .unwrap_or_else(|| "726".to_string())),
    );
    row.insert(
        "datminmxid".to_string(),
        json!(def.datminmxid.clone().unwrap_or_else(|| "1".to_string())),
    );
    row.insert(
        "dattablespace".to_string(),
        json!(def.dattablespace.unwrap_or(1663)),
    );
    row.insert(
        "datcollate".to_string(),
        json!(def.datcollate.clone().unwrap_or_else(|| "C".to_string())),
    );
    row.insert(
        "datctype".to_string(),
        json!(def.datctype.clone().unwrap_or_else(|| "C".to_string())),
    );
    row.insert(
        "datlocale".to_string(),
        def.datlocale
            .clone()
            .map(|v| json!(v))
            .unwrap_or(Value::Null),
    );
    row.insert(
        "daticurules".to_string(),
        def.daticurules
            .clone()
            .map(|v| json!(v))
            .unwrap_or(Value::Null),
    );
    row.insert(
        "datcollversion".to_string(),
        def.datcollversion
            .clone()
            .map(|v| json!(v))
            .unwrap_or(Value::Null),
    );
    row.insert("datacl".to_string(), acl_value(&def.datacl));
    row
}

/// Build one `pg_catalog.pg_namespace` row from a [`SchemaDef`].
pub fn build_pg_namespace_row(def: &SchemaDef) -> Row {
    let mut row = Row::new();
    row.insert("oid".to_string(), json!(def.oid));
    row.insert("nspname".to_string(), json!(def.name));
    row.insert("nspowner".to_string(), json!(def.owner_oid.unwrap_or(10)));
    row.insert("nspacl".to_string(), Value::Null);
    row
}

/// Build one `pg_catalog.pg_class` row from a [`RelationDef`] and the OID of its
/// owning schema.
pub fn build_pg_class_row(def: &RelationDef, namespace_oid: i32) -> Row {
    let mut row = Row::new();
    row.insert("oid".to_string(), json!(def.oid));
    row.insert("relname".to_string(), json!(def.name));
    row.insert("relnamespace".to_string(), json!(namespace_oid));
    row.insert("reltype".to_string(), json!(def.reltype_oid));
    row.insert("relkind".to_string(), json!(def.kind.relkind()));
    row.insert("reltuples".to_string(), json!(0));
    row.insert("relispartition".to_string(), json!(false));
    row
}

/// Build one `pg_catalog.pg_type` row describing a relation's composite rowtype.
pub fn build_pg_type_rowtype_row(def: &RelationDef, namespace_oid: i32) -> Row {
    let mut row = Row::new();
    row.insert("oid".to_string(), json!(def.reltype_oid));
    row.insert("typname".to_string(), json!(def.name));
    row.insert("typnamespace".to_string(), json!(namespace_oid));
    row.insert("typrelid".to_string(), json!(def.oid));
    row.insert("typlen".to_string(), json!(-1));
    row.insert("typtype".to_string(), json!("c"));
    row.insert("typcategory".to_string(), json!("C"));
    row
}

/// Build the `pg_catalog.pg_attribute` rows for a relation's columns. `attrelid`
/// is the owning relation's OID; `attnum` is the 1-based ordinal position.
pub fn build_pg_attribute_rows(attrelid: i32, columns: &[ColumnSpec]) -> Vec<Row> {
    columns
        .iter()
        .enumerate()
        .map(|(idx, col)| {
            let mut row = Row::new();
            row.insert("attrelid".to_string(), json!(attrelid));
            row.insert("attname".to_string(), json!(col.name));
            row.insert("atttypid".to_string(), json!(col.type_oid));
            row.insert("attnum".to_string(), json!((idx + 1) as i32));
            row.insert("atttypmod".to_string(), json!(-1));
            row.insert("attnotnull".to_string(), json!(!col.nullable));
            row.insert("attisdropped".to_string(), json!(false));
            row
        })
        .collect()
}

/// Build one `information_schema.tables` row for a relation.
pub fn build_info_tables_row(catalog: &str, schema: &str, def: &RelationDef) -> Row {
    let mut row = Row::new();
    row.insert("table_catalog".to_string(), json!(catalog));
    row.insert("table_schema".to_string(), json!(schema));
    row.insert("table_name".to_string(), json!(def.name));
    row.insert("table_type".to_string(), json!(def.kind.table_type()));
    row.insert("is_insertable_into".to_string(), json!("YES"));
    row.insert("is_typed".to_string(), json!("NO"));
    row
}

/// Build the `information_schema.columns` rows for a relation's columns. The
/// `data_type`/`udt_name` strings are derived from each column's `pg_type` OID.
pub fn build_info_columns_rows(
    catalog: &str,
    schema: &str,
    relation: &str,
    columns: &[ColumnSpec],
) -> Vec<Row> {
    columns
        .iter()
        .enumerate()
        .map(|(idx, col)| {
            let (data_type, udt_name) = oid_to_type_names(col.type_oid);
            let ordinal = (idx + 1) as i32;
            let mut row = Row::new();
            row.insert("table_catalog".to_string(), json!(catalog));
            row.insert("table_schema".to_string(), json!(schema));
            row.insert("table_name".to_string(), json!(relation));
            row.insert("column_name".to_string(), json!(col.name));
            row.insert("ordinal_position".to_string(), json!(ordinal));
            row.insert(
                "is_nullable".to_string(),
                json!(if col.nullable { "YES" } else { "NO" }),
            );
            row.insert("data_type".to_string(), json!(data_type));
            row.insert("udt_catalog".to_string(), json!(catalog));
            row.insert("udt_schema".to_string(), json!("pg_catalog"));
            row.insert("udt_name".to_string(), json!(udt_name));
            row.insert("dtd_identifier".to_string(), json!(ordinal.to_string()));
            row.insert("is_self_referencing".to_string(), json!("NO"));
            row.insert("is_identity".to_string(), json!("NO"));
            row.insert("is_generated".to_string(), json!("NEVER"));
            row.insert("is_updatable".to_string(), json!("YES"));
            row
        })
        .collect()
}

/// Build one `information_schema.schemata` row for a schema.
pub fn build_info_schemata_row(catalog: &str, def: &SchemaDef) -> Row {
    let mut row = Row::new();
    row.insert("catalog_name".to_string(), json!(catalog));
    row.insert("schema_name".to_string(), json!(def.name));
    row.insert("schema_owner".to_string(), Value::Null);
    row
}

/// Walk the source hierarchy as far as `table` requires and build that table's
/// user rows. Any error from the source propagates unchanged.
pub fn build_rows_for(table: CatalogTable, source: &dyn LazyCatalogSource) -> DFResult<Vec<Row>> {
    let mut rows = Vec::new();
    match table {
        CatalogTable::PgDatabase => {
            for db in fetch_databases(source)? {
                rows.push(build_pg_database_row(&db));
            }
        }
        CatalogTable::PgNamespace => {
            for db in fetch_databases(source)? {
                for schema in fetch_schemas(source, &db.datname)? {
                    rows.push(build_pg_namespace_row(&schema));
                }
            }
        }
        CatalogTable::PgClass => {
            for db in fetch_databases(source)? {
                for schema in fetch_schemas(source, &db.datname)? {
                    for rel in fetch_relations(source, &db.datname, &schema.name)? {
                        rows.push(build_pg_class_row(&rel, schema.oid));
                    }
                }
            }
        }
        CatalogTable::PgType => {
            for db in fetch_databases(source)? {
                for schema in fetch_schemas(source, &db.datname)? {
                    for rel in fetch_relations(source, &db.datname, &schema.name)? {
                        rows.push(build_pg_type_rowtype_row(&rel, schema.oid));
                    }
                }
            }
        }
        CatalogTable::PgAttribute => {
            for db in fetch_databases(source)? {
                for schema in fetch_schemas(source, &db.datname)? {
                    for rel in fetch_relations(source, &db.datname, &schema.name)? {
                        let cols = fetch_columns(source, &db.datname, &schema.name, &rel.name)?;
                        rows.extend(build_pg_attribute_rows(rel.oid, &cols));
                    }
                }
            }
        }
        CatalogTable::InformationSchemaTables => {
            for db in fetch_databases(source)? {
                for schema in fetch_schemas(source, &db.datname)? {
                    for rel in fetch_relations(source, &db.datname, &schema.name)? {
                        rows.push(build_info_tables_row(&db.datname, &schema.name, &rel));
                    }
                }
            }
        }
        CatalogTable::InformationSchemaColumns => {
            for db in fetch_databases(source)? {
                for schema in fetch_schemas(source, &db.datname)? {
                    for rel in fetch_relations(source, &db.datname, &schema.name)? {
                        let cols = fetch_columns(source, &db.datname, &schema.name, &rel.name)?;
                        rows.extend(build_info_columns_rows(
                            &db.datname,
                            &schema.name,
                            &rel.name,
                            &cols,
                        ));
                    }
                }
            }
        }
        CatalogTable::InformationSchemaSchemata => {
            for db in fetch_databases(source)? {
                for schema in fetch_schemas(source, &db.datname)? {
                    rows.push(build_info_schemata_row(&db.datname, &schema));
                }
            }
        }
    }
    Ok(rows)
}

/// A [`TableProvider`] for one catalog table. On every scan it asks the source
/// for that table's user rows (here and now — nothing is cached), converts them
/// to a batch, and serves them *merged* with the built-in batches captured at
/// registration. DataFusion does all joins/filters/projection across providers.
pub struct LazyCatalogTableProvider {
    /// Which catalog table this provider serves.
    table: CatalogTable,
    /// The table's Arrow schema (taken from the YAML-loaded provider).
    schema: SchemaRef,
    /// The built-in system rows, captured once at registration (immutable).
    builtin: Vec<RecordBatch>,
    /// The user's callback object.
    source: Arc<dyn LazyCatalogSource>,
}

impl std::fmt::Debug for LazyCatalogTableProvider {
    /// Format without touching the opaque source object.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyCatalogTableProvider")
            .field("table", &self.table)
            .finish()
    }
}

#[async_trait]
impl TableProvider for LazyCatalogTableProvider {
    /// Return self as `Any` for downcasting by DataFusion.
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    /// The Arrow schema for this catalog table.
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    /// Catalog tables are plain base tables.
    fn table_type(&self) -> TableType {
        TableType::Base
    }

    /// Build the user rows from the source, merge them with the captured
    /// built-in rows, and serve the union through an in-memory plan honoring the
    /// requested projection/filters/limit. The (non-`Send`) callback used to
    /// pull rows lives and dies entirely before the first `.await`, so the
    /// returned future stays `Send` as DataFusion requires.
    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> DFResult<Arc<dyn ExecutionPlan>> {
        let user_rows = build_rows_for(self.table, &*self.source)?;
        let user_batch = rows_to_record_batch(&self.schema, &user_rows)?;

        let mut batches = self.builtin.clone();
        batches.push(user_batch);

        let mem = MemTable::try_new(self.schema.clone(), vec![batches])?;
        mem.scan(state, projection, filters, limit).await
    }
}

/// Which catalog tables [`register_lazy_catalog`] should install lazy providers
/// over.
#[derive(Clone, Debug)]
pub struct LazyCatalogOptions {
    /// The catalog tables to wrap with a [`LazyCatalogTableProvider`].
    pub tables: Vec<CatalogTable>,
}

impl LazyCatalogOptions {
    /// Every catalog table currently supported by the lazy mechanism
    /// (Tier 1 + Tier 2).
    pub fn all() -> Self {
        Self {
            tables: vec![
                CatalogTable::PgDatabase,
                CatalogTable::PgNamespace,
                CatalogTable::PgClass,
                CatalogTable::PgType,
                CatalogTable::PgAttribute,
                CatalogTable::InformationSchemaTables,
                CatalogTable::InformationSchemaColumns,
                CatalogTable::InformationSchemaSchemata,
            ],
        }
    }

    /// Install lazy providers over exactly the given tables.
    pub fn with_tables(tables: Vec<CatalogTable>) -> Self {
        Self { tables }
    }
}

impl Default for LazyCatalogOptions {
    /// Defaults to wrapping all supported tables.
    fn default() -> Self {
        Self::all()
    }
}

/// Install lazy providers over the catalog + information_schema tables, sourcing
/// user rows from `source`.
///
/// MUST be called right after [`crate::get_base_session_context`] and BEFORE any
/// static `register_user_*` call, so the captured built-in batches contain only
/// the YAML system rows. For each target table this captures the current
/// provider's rows (the built-ins), then swaps in a [`LazyCatalogTableProvider`]
/// that merges those built-ins with whatever the source returns per scan.
pub async fn register_lazy_catalog(
    ctx: &SessionContext,
    source: Arc<dyn LazyCatalogSource>,
    opts: LazyCatalogOptions,
) -> DFResult<()> {
    let default_catalog = {
        let state = ctx.state();
        state.config_options().catalog.default_catalog.clone()
    };

    let catalog = ctx.catalog(&default_catalog).ok_or_else(|| {
        DataFusionError::Execution(format!("default catalog '{default_catalog}' not found"))
    })?;

    for table in opts.tables {
        let (schema_name, table_name) = table.location();

        let schema_provider = catalog.schema(schema_name).ok_or_else(|| {
            DataFusionError::Execution(format!(
                "schema '{schema_name}' not found while registering lazy catalog"
            ))
        })?;

        let current = schema_provider.table(table_name).await?.ok_or_else(|| {
            DataFusionError::Execution(format!(
                "table '{schema_name}.{table_name}' not found while registering lazy catalog"
            ))
        })?;

        // Capture the built-in rows exactly once, before swapping providers.
        let table_schema = current.schema();
        let builtin = {
            let state = ctx.state();
            let plan = current.scan(&state, None, &[], None).await?;
            collect(plan, ctx.task_ctx()).await?
        };

        let provider: Arc<dyn TableProvider> = Arc::new(LazyCatalogTableProvider {
            table,
            schema: table_schema,
            builtin,
            source: source.clone(),
        });

        let _ = schema_provider.deregister_table(table_name);
        schema_provider.register_table(table_name.to_string(), provider)?;
    }

    Ok(())
}
