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
//! OIDs). What backs a source - an embedded SQL engine, a network service, a
//! file, an in-memory `Vec`, or nothing - is entirely opaque to `pg_catalog`.
//!
//! OIDs are supplied by the source and written through verbatim; `pg_catalog`
//! never invents, derives, caches, or remembers them. Keeping OIDs stable and
//! consistent across calls (so cross-table joins resolve) is the source's job.

use std::collections::HashSet;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BooleanArray, Int32Array, Int64Array, LargeStringArray, StringArray,
    StringViewArray,
};
use arrow::compute::filter_record_batch;
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

/// One `pg_config` build/install setting fed into `pg_catalog.pg_config`
/// (the static `name`/`setting` pairs the `pg_config` CLI reports). An embedder
/// supplies these to override or extend the built-in defaults; a setting whose
/// `name` matches a built-in one replaces it.
#[derive(Clone, Debug)]
pub struct ConfigSettingDef {
    /// The setting name (`pg_config.name`), e.g. `VERSION` or `BINDIR`.
    pub name: String,
    /// The setting value (`pg_config.setting`).
    pub setting: String,
}

/// One `pg_settings` runtime configuration parameter (GUC) fed into
/// `pg_catalog.pg_settings`. An embedder supplies these to override the built-in
/// snapshot - typically the session-mutable settings whose live value it knows
/// (e.g. `search_path`, `TimeZone`). A setting whose `name` matches a built-in
/// one replaces that whole row, so the metadata columns of an overridden row are
/// left NULL unless re-supplied.
#[derive(Clone, Debug)]
pub struct SettingDef {
    /// The parameter name (`pg_settings.name`), e.g. `search_path`.
    pub name: String,
    /// The parameter's current value (`pg_settings.setting`).
    pub setting: String,
}

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
    /// Owning role OID (`pg_class.relowner`), surfaced as `pg_tables.tableowner`
    /// via `pg_get_userbyid`. `None` leaves it NULL - appropriate for backends
    /// with no ownership concept (e.g. DuckDB); a backend that has owners (e.g.
    /// PostgreSQL) supplies the role OID here.
    pub owner_oid: Option<i32>,
    /// Whether the relation has any index (`pg_class.relhasindex`). Drives
    /// `pg_tables.hasindexes`; defaults to `false` via [`RelationDef::table`].
    pub has_index: bool,
    /// Whether the relation has any rule (`pg_class.relhasrules`).
    pub has_rules: bool,
    /// Whether the relation has any trigger (`pg_class.relhastriggers`). Drives
    /// `pg_tables.hastriggers`.
    pub has_triggers: bool,
    /// Whether row-level security is enabled (`pg_class.relrowsecurity`).
    pub row_security: bool,
}

impl RelationDef {
    /// Construct a `Table` relation definition from its OID, rowtype OID, and
    /// name, with all metadata flags off. Use the struct literal (or set the
    /// flags afterward) for views, indexed tables, etc.
    pub fn table(oid: i32, reltype_oid: i32, name: impl Into<String>) -> Self {
        Self {
            oid,
            reltype_oid,
            name: name.into(),
            kind: RelationKind::Table,
            owner_oid: None,
            has_index: false,
            has_rules: false,
            has_triggers: false,
            row_security: false,
        }
    }
}

/// One index fed into `pg_catalog.pg_index` (plus its own `pg_class` row).
///
/// In PostgreSQL an index is itself a relation: it has a `pg_class` row of
/// `relkind = 'i'` that carries its *name*, and a `pg_index` row that carries its
/// *structure* (which table and columns it covers, whether it is unique/primary).
/// `pg_indexes` and `pg_get_indexdef` reconstruct the `CREATE INDEX` text by
/// joining the two, so a single [`IndexDef`] produces BOTH rows: the catalog
/// derives the index's `pg_class` identity row and its `pg_index` structure row
/// from this one description.
///
/// All OIDs are user-supplied and written through verbatim. Functional/partial
/// index expressions (`pg_index.indexprs`/`indpred`, stored by PostgreSQL as
/// node trees) are out of scope and left NULL; a plain column index needs none of
/// them.
#[derive(Clone, Debug)]
pub struct IndexDef {
    /// The index relation's own `pg_class.oid`, written to `pg_index.indexrelid`.
    pub index_oid: i32,
    /// The index relation's name (`pg_class.relname`), e.g. `users_pkey`.
    pub index_name: String,
    /// The indexed table's `pg_class.oid`, written to `pg_index.indrelid`.
    pub table_oid: i32,
    /// The indexed table columns, as 1-based `pg_attribute.attnum` values in index
    /// order. Written to `pg_index.indkey`; its length is `indnatts`.
    pub key_attnums: Vec<i32>,
    /// Whether the index enforces uniqueness (`pg_index.indisunique`).
    pub is_unique: bool,
    /// Whether the index implements the table's primary key
    /// (`pg_index.indisprimary`).
    pub is_primary: bool,
}

impl IndexDef {
    /// Construct an index definition from its OID, name, the OID of the table it
    /// indexes, and the indexed column attnums, with the unique/primary flags off.
    /// Set those flags afterward for a unique index or a primary key.
    pub fn new(
        index_oid: i32,
        index_name: impl Into<String>,
        table_oid: i32,
        key_attnums: Vec<i32>,
    ) -> Self {
        Self {
            index_oid,
            index_name: index_name.into(),
            table_oid,
            key_attnums,
            is_unique: false,
            is_primary: false,
        }
    }
}

/// The kind of table constraint a [`ConstraintDef`] describes. Check constraints
/// are excluded: their defining SQL is a node tree we do not deparse, so their text
/// is supplied by the integration-provided definition-text resolver rather than
/// derived structurally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstraintKind {
    /// A primary key (`pg_constraint.contype = 'p'`).
    PrimaryKey,
    /// A unique constraint (`pg_constraint.contype = 'u'`).
    Unique,
    /// A foreign key (`pg_constraint.contype = 'f'`).
    ForeignKey,
}

impl ConstraintKind {
    /// The single-character `pg_constraint.contype` code for this kind.
    pub fn contype(&self) -> &'static str {
        match self {
            ConstraintKind::PrimaryKey => "p",
            ConstraintKind::Unique => "u",
            ConstraintKind::ForeignKey => "f",
        }
    }
}

/// The action a foreign key takes on UPDATE or DELETE of a referenced row
/// (`pg_constraint.confupdtype` / `confdeltype`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForeignKeyAction {
    /// `NO ACTION` (`a`) - the default; reject the change if references remain.
    NoAction,
    /// `RESTRICT` (`r`).
    Restrict,
    /// `CASCADE` (`c`).
    Cascade,
    /// `SET NULL` (`n`).
    SetNull,
    /// `SET DEFAULT` (`d`).
    SetDefault,
}

impl ForeignKeyAction {
    /// The single-character action code stored in `pg_constraint`.
    pub fn code(&self) -> &'static str {
        match self {
            ForeignKeyAction::NoAction => "a",
            ForeignKeyAction::Restrict => "r",
            ForeignKeyAction::Cascade => "c",
            ForeignKeyAction::SetNull => "n",
            ForeignKeyAction::SetDefault => "d",
        }
    }
}

/// How a foreign key matches a multi-column reference
/// (`pg_constraint.confmatchtype`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForeignKeyMatch {
    /// `MATCH FULL` (`f`).
    Full,
    /// `MATCH PARTIAL` (`p`).
    Partial,
    /// `MATCH SIMPLE` (`s`) - the default.
    Simple,
}

impl ForeignKeyMatch {
    /// The single-character match-type code stored in `pg_constraint`.
    pub fn code(&self) -> &'static str {
        match self {
            ForeignKeyMatch::Full => "f",
            ForeignKeyMatch::Partial => "p",
            ForeignKeyMatch::Simple => "s",
        }
    }
}

/// One constraint fed into `pg_catalog.pg_constraint`.
///
/// Describes a primary-key, unique, or foreign-key constraint structurally - by
/// its key columns, and (for a foreign key) the referenced relation and columns.
/// A single [`ConstraintDef`] becomes one `pg_constraint` row, which the
/// `information_schema` constraint views (`table_constraints`,
/// `key_column_usage`, `constraint_column_usage`, `referential_constraints`)
/// derive from. FK targets are given as OIDs the source already knows.
#[derive(Clone, Debug)]
pub struct ConstraintDef {
    /// The constraint's `pg_constraint.oid`.
    pub oid: i32,
    /// The constraint name (`conname`).
    pub name: String,
    /// Primary key, unique, or foreign key (`contype`).
    pub kind: ConstraintKind,
    /// The schema OID the constraint belongs to (`connamespace`), normally the
    /// constrained table's schema.
    pub namespace_oid: i32,
    /// The constrained table's `pg_class.oid` (`conrelid`).
    pub table_oid: i32,
    /// The constrained columns as 1-based attnums in key order (`conkey`).
    pub key_attnums: Vec<i32>,
    /// The OID of the index backing this constraint (`conindid`), or 0 if none.
    /// PK/UNIQUE constraints are normally backed by a unique index.
    pub index_oid: i32,
    /// For a foreign key, the referenced table's `pg_class.oid` (`confrelid`); 0
    /// for primary-key and unique constraints.
    pub referenced_table_oid: i32,
    /// For a foreign key, the referenced columns as 1-based attnums (`confkey`),
    /// positionally matched to `key_attnums`; empty for non-foreign-key kinds.
    pub referenced_key_attnums: Vec<i32>,
    /// A foreign key's `ON UPDATE` action (`confupdtype`).
    pub on_update: ForeignKeyAction,
    /// A foreign key's `ON DELETE` action (`confdeltype`).
    pub on_delete: ForeignKeyAction,
    /// A foreign key's match type (`confmatchtype`).
    pub match_type: ForeignKeyMatch,
}

impl ConstraintDef {
    /// Construct a primary-key constraint over `key_attnums` of `table_oid`,
    /// backed by `index_oid` (the unique index implementing it; pass 0 if none).
    pub fn primary_key(
        oid: i32,
        name: impl Into<String>,
        namespace_oid: i32,
        table_oid: i32,
        key_attnums: Vec<i32>,
        index_oid: i32,
    ) -> Self {
        Self::key_constraint(
            ConstraintKind::PrimaryKey,
            oid,
            name,
            namespace_oid,
            table_oid,
            key_attnums,
            index_oid,
        )
    }

    /// Construct a unique constraint over `key_attnums` of `table_oid`, backed by
    /// `index_oid` (the unique index implementing it; pass 0 if none).
    pub fn unique(
        oid: i32,
        name: impl Into<String>,
        namespace_oid: i32,
        table_oid: i32,
        key_attnums: Vec<i32>,
        index_oid: i32,
    ) -> Self {
        Self::key_constraint(
            ConstraintKind::Unique,
            oid,
            name,
            namespace_oid,
            table_oid,
            key_attnums,
            index_oid,
        )
    }

    /// Shared constructor for the key-only constraint kinds (primary key and
    /// unique), which reference no other relation.
    fn key_constraint(
        kind: ConstraintKind,
        oid: i32,
        name: impl Into<String>,
        namespace_oid: i32,
        table_oid: i32,
        key_attnums: Vec<i32>,
        index_oid: i32,
    ) -> Self {
        Self {
            oid,
            name: name.into(),
            kind,
            namespace_oid,
            table_oid,
            key_attnums,
            index_oid,
            referenced_table_oid: 0,
            referenced_key_attnums: Vec::new(),
            on_update: ForeignKeyAction::NoAction,
            on_delete: ForeignKeyAction::NoAction,
            match_type: ForeignKeyMatch::Simple,
        }
    }

    /// Construct a foreign-key constraint: `key_attnums` of `table_oid` reference
    /// `referenced_key_attnums` of `referenced_table_oid`, positionally matched.
    /// The ON UPDATE / ON DELETE actions default to NO ACTION and the match type
    /// to SIMPLE; set those fields afterward to change them.
    ///
    /// Because the two attnum lists are matched position-by-position (and become
    /// `pg_constraint.conkey` / `confkey`), they must have the same length; an
    /// unequal pairing is rejected here so an invalid foreign key can never be
    /// constructed and later serialized.
    #[allow(clippy::too_many_arguments)]
    pub fn foreign_key(
        oid: i32,
        name: impl Into<String>,
        namespace_oid: i32,
        table_oid: i32,
        key_attnums: Vec<i32>,
        referenced_table_oid: i32,
        referenced_key_attnums: Vec<i32>,
        index_oid: i32,
    ) -> DFResult<Self> {
        let name = name.into();
        if key_attnums.len() != referenced_key_attnums.len() {
            return Err(DataFusionError::Execution(format!(
                "foreign key '{name}' has {} key column(s) but {} referenced column(s); \
                 they are positionally matched and must be equal in number",
                key_attnums.len(),
                referenced_key_attnums.len(),
            )));
        }
        Ok(Self {
            oid,
            name,
            kind: ConstraintKind::ForeignKey,
            namespace_oid,
            table_oid,
            key_attnums,
            index_oid,
            referenced_table_oid,
            referenced_key_attnums,
            on_update: ForeignKeyAction::NoAction,
            on_delete: ForeignKeyAction::NoAction,
            match_type: ForeignKeyMatch::Simple,
        })
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
    /// Whether the column has a default expression (`pg_attribute.atthasdef`, and a
    /// backing `pg_attrdef` row). The default *text* is supplied by the
    /// integration-provided definition-text resolver; this flag and the `pg_attrdef`
    /// handle are the structural part.
    pub has_default: bool,
}

impl ColumnSpec {
    /// Construct a column specification from a name, `pg_type` OID, and
    /// nullability, with no column default.
    pub fn new(name: impl Into<String>, type_oid: i32, nullable: bool) -> Self {
        Self {
            name: name.into(),
            type_oid,
            nullable,
            has_default: false,
        }
    }

    /// Mark this column as having a default expression, so it gets a `pg_attrdef`
    /// row and `pg_attribute.atthasdef = true`. The default text itself is supplied
    /// by the integration-provided definition-text resolver.
    pub fn with_default(mut self) -> Self {
        self.has_default = true;
        self
    }
}

/// Abstract source of *user* catalog metadata, backend-agnostic and
/// connection-free. Each method takes a `callback` and calls it with the objects
/// it found. How the implementor produces them (SQL engine, service, file,
/// in-memory, or empty) is opaque to `pg_catalog`. Built-in system rows are added
/// by the layer, so implementors return ONLY their own objects.
///
/// Errors are returned as `DataFusionError` and propagate to the client - a
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

    /// Indexes in `database`.`schema` -> `pg_catalog.pg_index` (plus each index's
    /// own `pg_class` row).
    ///
    /// Has a default that contributes nothing, so existing implementors expose no
    /// indexes and keep compiling. Override it to report a relation's indexes so
    /// `pg_indexes` / `pg_get_indexdef` can describe them. The `index_oid` of each
    /// returned [`IndexDef`] must be distinct from every relation OID, since an
    /// index occupies its own `pg_class` row.
    fn indexes(
        &self,
        _database: &str,
        _schema: &str,
        _callback: &mut dyn FnMut(Vec<IndexDef>),
    ) -> DFResult<()> {
        Ok(())
    }

    /// Constraints in `database`.`schema` -> `pg_catalog.pg_constraint`.
    ///
    /// Has a default that contributes nothing, so existing implementors expose no
    /// constraints and keep compiling. Override it to report a relation's
    /// primary-key/unique/foreign-key constraints so the `information_schema`
    /// constraint views describe them. Each returned [`ConstraintDef`]'s `oid`
    /// must be distinct from every relation and index OID.
    fn constraints(
        &self,
        _database: &str,
        _schema: &str,
        _callback: &mut dyn FnMut(Vec<ConstraintDef>),
    ) -> DFResult<()> {
        Ok(())
    }

    /// `pg_config` build/install settings -> `pg_catalog.pg_config`.
    ///
    /// Has a default that contributes nothing, so existing implementors keep the
    /// built-in `pg_config` defaults. Override it to report the embedding
    /// application's own build settings (e.g. its real `VERSION`); a setting
    /// whose `name` matches a built-in one replaces it.
    fn config(&self, _callback: &mut dyn FnMut(Vec<ConfigSettingDef>)) -> DFResult<()> {
        Ok(())
    }

    /// `pg_settings` runtime parameters -> `pg_catalog.pg_settings`.
    ///
    /// Has a default that contributes nothing, so existing implementors keep the
    /// built-in settings snapshot. Override it to report the embedding
    /// application's live values for session-mutable parameters (e.g.
    /// `search_path`, `TimeZone`); a setting whose `name` matches a built-in one
    /// replaces it.
    fn settings(&self, _callback: &mut dyn FnMut(Vec<SettingDef>)) -> DFResult<()> {
        Ok(())
    }
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
    /// `pg_catalog.pg_index`.
    PgIndex,
    /// `pg_catalog.pg_constraint`.
    PgConstraint,
    /// `pg_catalog.pg_attrdef`.
    PgAttrdef,
    /// `pg_catalog.pg_config`.
    PgConfig,
    /// `pg_catalog.pg_settings`.
    PgSettings,
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
            CatalogTable::PgIndex => ("pg_catalog", "pg_index"),
            CatalogTable::PgConstraint => ("pg_catalog", "pg_constraint"),
            CatalogTable::PgAttrdef => ("pg_catalog", "pg_attrdef"),
            CatalogTable::PgConfig => ("pg_catalog", "pg_config"),
            CatalogTable::PgSettings => ("pg_catalog", "pg_settings"),
            CatalogTable::InformationSchemaTables => ("information_schema", "tables"),
            CatalogTable::InformationSchemaColumns => ("information_schema", "columns"),
            CatalogTable::InformationSchemaSchemata => ("information_schema", "schemata"),
        }
    }

    /// The columns that identify one row of this catalog table, used to merge
    /// user rows with built-in rows: a user row replaces any built-in row sharing
    /// the same key, and two user rows sharing a key are a source error.
    ///
    /// Keys are scoped so that legitimately distinct objects never collide:
    /// relations/types/attributes are keyed by their *parent OID* plus name
    /// (e.g. `(relnamespace, relname)`), so the same name under two different
    /// schemas is not a duplicate. `pg_namespace` is keyed by `oid` (its true
    /// identity) because the flattened catalog intentionally allows the same
    /// schema name - e.g. `public` - under several databases.
    pub fn key_columns(&self) -> &'static [&'static str] {
        match self {
            CatalogTable::PgDatabase => &["datname"],
            CatalogTable::PgNamespace => &["oid"],
            CatalogTable::PgClass => &["relnamespace", "relname"],
            CatalogTable::PgType => &["typnamespace", "typname"],
            CatalogTable::PgAttribute => &["attrelid", "attname"],
            CatalogTable::PgIndex => &["indexrelid"],
            CatalogTable::PgConstraint => &["oid"],
            CatalogTable::PgAttrdef => &["adrelid", "adnum"],
            CatalogTable::PgConfig => &["name"],
            CatalogTable::PgSettings => &["name"],
            CatalogTable::InformationSchemaTables => {
                &["table_catalog", "table_schema", "table_name"]
            }
            CatalogTable::InformationSchemaColumns => {
                &["table_catalog", "table_schema", "table_name", "column_name"]
            }
            CatalogTable::InformationSchemaSchemata => &["catalog_name", "schema_name"],
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

/// Pull the indexes of `database`.`schema` from `source`.
fn fetch_indexes(
    source: &dyn LazyCatalogSource,
    database: &str,
    schema: &str,
) -> DFResult<Vec<IndexDef>> {
    let mut out = Vec::new();
    source.indexes(database, schema, &mut |rows| out.extend(rows))?;
    Ok(out)
}

/// Pull the constraints of `database`.`schema` from `source`.
fn fetch_constraints(
    source: &dyn LazyCatalogSource,
    database: &str,
    schema: &str,
) -> DFResult<Vec<ConstraintDef>> {
    let mut out = Vec::new();
    source.constraints(database, schema, &mut |rows| out.extend(rows))?;
    Ok(out)
}

/// Pull the `pg_config` settings from `source`.
fn fetch_config(source: &dyn LazyCatalogSource) -> DFResult<Vec<ConfigSettingDef>> {
    let mut out = Vec::new();
    source.config(&mut |rows| out.extend(rows))?;
    Ok(out)
}

/// Pull the `pg_settings` parameters from `source`.
fn fetch_settings(source: &dyn LazyCatalogSource) -> DFResult<Vec<SettingDef>> {
    let mut out = Vec::new();
    source.settings(&mut |rows| out.extend(rows))?;
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

/// Build a JSON array from a list of strings, or NULL when the list is absent.
fn string_list_to_json(items: &Option<Vec<String>>) -> Value {
    match items {
        Some(items) => Value::Array(items.iter().map(|s| json!(s)).collect()),
        None => Value::Null,
    }
}

/// Build one `pg_catalog.pg_config` row (a `name`/`setting` pair) from a
/// [`ConfigSettingDef`].
pub fn build_pg_config_row(def: &ConfigSettingDef) -> Row {
    let mut row = Row::new();
    row.insert("name".to_string(), json!(def.name));
    row.insert("setting".to_string(), json!(def.setting));
    row
}

/// Build one `pg_catalog.pg_settings` row from a [`SettingDef`]. Only `name` and
/// `setting` are populated; the remaining metadata columns are left to default to
/// NULL (they describe the parameter, not its value).
pub fn build_pg_settings_row(def: &SettingDef) -> Row {
    let mut row = Row::new();
    row.insert("name".to_string(), json!(def.name));
    row.insert("setting".to_string(), json!(def.setting));
    row
}

/// Build one `pg_catalog.pg_database` row from a [`DatabaseDef`], filling unset
/// optional fields with PostgreSQL-compatible defaults.
pub fn build_pg_database_row(def: &DatabaseDef) -> Row {
    let mut row = Row::new();
    row.insert("oid".to_string(), json!(def.oid));
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
    row.insert("datacl".to_string(), string_list_to_json(&def.datacl));
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

/// The `pg_am.oid` of the heap table access method (a fixed PostgreSQL system
/// OID). An ordinary registered table is heap-stored, so its `pg_class.relam`
/// points here. (A view/materialized view has no access method; PostgreSQL still
/// records heap for a materialized view and 0 for a plain view, but heap is a
/// safe non-NULL default for client introspection either way.)
pub const HEAP_ACCESS_METHOD_OID: i32 = 2;

/// Build one `pg_catalog.pg_class` row from a [`RelationDef`], the OID of its
/// owning schema, and its column count (`natts`, written to `relnatts`).
pub fn build_pg_class_row(def: &RelationDef, namespace_oid: i32, natts: i32) -> Row {
    let mut row = Row::new();
    row.insert("oid".to_string(), json!(def.oid));
    row.insert("relname".to_string(), json!(def.name));
    row.insert("relnamespace".to_string(), json!(namespace_oid));
    row.insert("reltype".to_string(), json!(def.reltype_oid));
    row.insert("relkind".to_string(), json!(def.kind.relkind()));
    row.insert("relam".to_string(), json!(HEAP_ACCESS_METHOD_OID));
    row.insert("reltuples".to_string(), json!(0.0));
    row.insert("relispartition".to_string(), json!(false));
    // Flags read by pg_tables / pg_views and common client introspection. The
    // index/trigger/rule/RLS flags come from the source; the rest are sensible
    // non-NULL defaults so the view columns aren't blank for user relations.
    // Owner is optional: written through only when the source supplies it, so a
    // backend without ownership leaves pg_tables.tableowner blank.
    if let Some(owner) = def.owner_oid {
        row.insert("relowner".to_string(), json!(owner));
    }
    row.insert("relhasindex".to_string(), json!(def.has_index));
    row.insert("relhasrules".to_string(), json!(def.has_rules));
    row.insert("relhastriggers".to_string(), json!(def.has_triggers));
    row.insert("relrowsecurity".to_string(), json!(def.row_security));
    row.insert("relispopulated".to_string(), json!(true));
    row.insert("relpersistence".to_string(), json!("p"));
    row.insert("relreplident".to_string(), json!("d"));
    // Structural columns clients read off pg_class for a real relation. The
    // relation has no separate on-disk file in this layer, so relfilenode mirrors
    // the OID (PostgreSQL's own default at creation) and the size/freeze counters
    // are zero.
    row.insert("relnatts".to_string(), json!(natts));
    row.insert("relchecks".to_string(), json!(0));
    row.insert("relhassubclass".to_string(), json!(false));
    row.insert("relfilenode".to_string(), json!(def.oid));
    row.insert("reltablespace".to_string(), json!(0));
    row.insert("relpages".to_string(), json!(0));
    row.insert("relallvisible".to_string(), json!(0));
    row.insert("reltoastrelid".to_string(), json!(0));
    row.insert("relfrozenxid".to_string(), json!(0));
    row.insert("relminmxid".to_string(), json!(0));
    row
}

/// The `pg_am.oid` of the B-tree access method (a fixed PostgreSQL system OID).
/// A registered [`IndexDef`] describes a plain column index, which is always
/// B-tree, so its `pg_class.relam` points here - letting `pg_get_indexdef`
/// render `USING btree`.
pub const BTREE_ACCESS_METHOD_OID: i32 = 403;

/// Build the `pg_catalog.pg_class` row for an index relation (`relkind = 'i'`).
///
/// An index has no composite rowtype, so `reltype` is 0 and no `pg_type` row is
/// emitted. `relhasindex` is false (an index does not itself have indexes). The
/// remaining defaults match [`build_pg_class_row`] so client introspection sees
/// the same non-NULL columns for an index as for a table.
pub fn build_index_pg_class_row(def: &IndexDef, namespace_oid: i32) -> Row {
    let mut row = Row::new();
    row.insert("oid".to_string(), json!(def.index_oid));
    row.insert("relname".to_string(), json!(def.index_name));
    row.insert("relnamespace".to_string(), json!(namespace_oid));
    row.insert("reltype".to_string(), json!(0));
    row.insert("relkind".to_string(), json!("i"));
    row.insert("relam".to_string(), json!(BTREE_ACCESS_METHOD_OID));
    row.insert("reltuples".to_string(), json!(0.0));
    row.insert("relispartition".to_string(), json!(false));
    row.insert("relhasindex".to_string(), json!(false));
    row.insert("relhasrules".to_string(), json!(false));
    row.insert("relhastriggers".to_string(), json!(false));
    row.insert("relrowsecurity".to_string(), json!(false));
    row.insert("relispopulated".to_string(), json!(true));
    row.insert("relpersistence".to_string(), json!("p"));
    row.insert("relreplident".to_string(), json!("n"));
    row
}

/// Build one `pg_catalog.pg_index` row from an [`IndexDef`].
///
/// `indkey` is the list of indexed-column attnums; `indnatts`/`indnkeyatts` are
/// its length. The boolean flags describe a plain, valid, ready index. The
/// node-tree columns (`indexprs`/`indpred`) and the per-column option vectors
/// (`indcollation`/`indclass`/`indoption`) are left NULL - a plain column index
/// needs none of them, and the node trees are out of scope.
pub fn build_pg_index_row(def: &IndexDef) -> Row {
    let natts = def.key_attnums.len() as i32;
    let mut row = Row::new();
    row.insert("indexrelid".to_string(), json!(def.index_oid));
    row.insert("indrelid".to_string(), json!(def.table_oid));
    row.insert("indnatts".to_string(), json!(natts));
    row.insert("indnkeyatts".to_string(), json!(natts));
    row.insert("indisunique".to_string(), json!(def.is_unique));
    row.insert("indnullsnotdistinct".to_string(), json!(false));
    row.insert("indisprimary".to_string(), json!(def.is_primary));
    row.insert("indisexclusion".to_string(), json!(false));
    row.insert("indimmediate".to_string(), json!(true));
    row.insert("indisclustered".to_string(), json!(false));
    row.insert("indisvalid".to_string(), json!(true));
    row.insert("indcheckxmin".to_string(), json!(false));
    row.insert("indisready".to_string(), json!(true));
    row.insert("indislive".to_string(), json!(true));
    row.insert("indisreplident".to_string(), json!(false));
    row.insert("indkey".to_string(), json!(def.key_attnums));
    row
}

/// Build one `pg_catalog.pg_constraint` row from a [`ConstraintDef`].
///
/// The foreign-key fields (`confrelid`/`confkey`/`confupdtype`/`confdeltype`/
/// `confmatchtype`) carry real values only for a foreign key; for primary-key and
/// unique constraints they take PostgreSQL's non-FK sentinels (`confrelid` 0,
/// `confkey` NULL, and the three type codes a single space). The check-expression
/// column (`conbin`) is left NULL - these kinds have no expression, and check text
/// comes from the integration-provided definition-text resolver anyway.
pub fn build_pg_constraint_row(def: &ConstraintDef) -> Row {
    let is_foreign_key = def.kind == ConstraintKind::ForeignKey;
    let mut row = Row::new();
    row.insert("oid".to_string(), json!(def.oid));
    row.insert("conname".to_string(), json!(def.name));
    row.insert("connamespace".to_string(), json!(def.namespace_oid));
    row.insert("contype".to_string(), json!(def.kind.contype()));
    row.insert("condeferrable".to_string(), json!(false));
    row.insert("condeferred".to_string(), json!(false));
    row.insert("convalidated".to_string(), json!(true));
    row.insert("conrelid".to_string(), json!(def.table_oid));
    row.insert("contypid".to_string(), json!(0));
    row.insert("conindid".to_string(), json!(def.index_oid));
    row.insert("conparentid".to_string(), json!(0));
    row.insert("conkey".to_string(), json!(def.key_attnums));
    row.insert("conislocal".to_string(), json!(true));
    row.insert("coninhcount".to_string(), json!(0));
    row.insert("connoinherit".to_string(), json!(false));
    if is_foreign_key {
        row.insert("confrelid".to_string(), json!(def.referenced_table_oid));
        row.insert("confkey".to_string(), json!(def.referenced_key_attnums));
        row.insert("confupdtype".to_string(), json!(def.on_update.code()));
        row.insert("confdeltype".to_string(), json!(def.on_delete.code()));
        row.insert("confmatchtype".to_string(), json!(def.match_type.code()));
    } else {
        row.insert("confrelid".to_string(), json!(0));
        row.insert("confkey".to_string(), Value::Null);
        row.insert("confupdtype".to_string(), json!(" "));
        row.insert("confdeltype".to_string(), json!(" "));
        row.insert("confmatchtype".to_string(), json!(" "));
    }
    row
}

/// Build one `pg_catalog.pg_type` row describing a relation's composite rowtype.
///
/// A composite type is variable-length and passed by reference (`typlen` -1,
/// `typbyval` false), with the `record` I/O routines and extended storage that
/// PostgreSQL uses for every relation rowtype. `typelem`/`typarray` are 0 (the
/// rowtype is not an array element type and we do not register its array type).
pub fn build_pg_type_rowtype_row(def: &RelationDef, namespace_oid: i32) -> Row {
    let mut row = Row::new();
    row.insert("oid".to_string(), json!(def.reltype_oid));
    row.insert("typname".to_string(), json!(def.name));
    row.insert("typnamespace".to_string(), json!(namespace_oid));
    row.insert("typrelid".to_string(), json!(def.oid));
    row.insert("typlen".to_string(), json!(-1));
    row.insert("typtype".to_string(), json!("c"));
    row.insert("typcategory".to_string(), json!("C"));
    // Physical/behavioral columns clients read for a composite type.
    row.insert("typbyval".to_string(), json!(false));
    row.insert("typalign".to_string(), json!("d"));
    row.insert("typstorage".to_string(), json!("x"));
    row.insert("typisdefined".to_string(), json!(true));
    row.insert("typispreferred".to_string(), json!(false));
    row.insert("typnotnull".to_string(), json!(false));
    row.insert("typelem".to_string(), json!(0));
    row.insert("typarray".to_string(), json!(0));
    row.insert("typbasetype".to_string(), json!(0));
    row.insert("typtypmod".to_string(), json!(-1));
    row.insert("typndims".to_string(), json!(0));
    row.insert("typinput".to_string(), json!("record_in"));
    row.insert("typoutput".to_string(), json!("record_out"));
    row.insert("typreceive".to_string(), json!("record_recv"));
    row.insert("typsend".to_string(), json!("record_send"));
    row.insert("typdelim".to_string(), json!(","));
    row
}

/// The physical storage attributes a `pg_type` OID implies for `pg_attribute`:
/// `(attlen, attbyval, attalign, attstorage)`. Covers the common scalar types a
/// user table exposes; unknown OIDs fall back to a variable-length,
/// not-by-value, extended-storage column - the safe default for any text-like or
/// composite type.
fn column_type_storage(type_oid: i32) -> (i32, bool, &'static str, &'static str) {
    match type_oid {
        16 => (1, true, "c", "p"),                 // bool
        18 => (1, true, "c", "p"),                 // "char"
        21 => (2, true, "s", "p"),                 // int2
        23 => (4, true, "i", "p"),                 // int4
        26 => (4, true, "i", "p"),                 // oid
        20 => (8, true, "d", "p"),                 // int8
        700 => (4, true, "i", "p"),                // float4
        701 => (8, true, "d", "p"),                // float8
        1082 => (4, true, "i", "p"),               // date
        1114 | 1184 => (8, true, "d", "p"),        // timestamp / timestamptz
        1700 => (-1, false, "i", "m"),             // numeric (main-storage)
        25 | 1042 | 1043 => (-1, false, "i", "x"), // text / bpchar / varchar
        _ => (-1, false, "i", "x"),
    }
}

/// The default collation OID a `pg_type` OID implies for `pg_attribute`: the
/// collatable string types use the database default collation (100); every other
/// type is non-collatable (0).
fn column_collation(type_oid: i32) -> i32 {
    match type_oid {
        25 | 1042 | 1043 => 100, // text / bpchar / varchar -> default collation
        _ => 0,
    }
}

/// Build the `pg_catalog.pg_attribute` rows for a relation's columns. `attrelid`
/// is the owning relation's OID; `attnum` is the 1-based ordinal position. The
/// physical columns clients read for the binary protocol and for column
/// introspection (`attlen`/`attbyval`/`attalign`/`attstorage`/`attcollation`) are
/// derived from each column's type OID; `atthasdef` reflects whether the column
/// has a default (its `pg_attrdef` handle).
pub fn build_pg_attribute_rows(attrelid: i32, columns: &[ColumnSpec]) -> Vec<Row> {
    columns
        .iter()
        .enumerate()
        .map(|(idx, col)| {
            let (attlen, attbyval, attalign, attstorage) = column_type_storage(col.type_oid);
            let mut row = Row::new();
            row.insert("attrelid".to_string(), json!(attrelid));
            row.insert("attname".to_string(), json!(col.name));
            row.insert("atttypid".to_string(), json!(col.type_oid));
            row.insert("attnum".to_string(), json!((idx + 1) as i32));
            row.insert("atttypmod".to_string(), json!(-1));
            row.insert("attnotnull".to_string(), json!(!col.nullable));
            row.insert("atthasdef".to_string(), json!(col.has_default));
            row.insert("attisdropped".to_string(), json!(false));
            // Physical layout, derived from the column's type.
            row.insert("attlen".to_string(), json!(attlen));
            row.insert("attbyval".to_string(), json!(attbyval));
            row.insert("attalign".to_string(), json!(attalign));
            row.insert("attstorage".to_string(), json!(attstorage));
            row.insert(
                "attcollation".to_string(),
                json!(column_collation(col.type_oid)),
            );
            // Fixed structural defaults for a plain, locally-defined column.
            row.insert("attndims".to_string(), json!(0));
            row.insert("attcacheoff".to_string(), json!(-1));
            row.insert("attislocal".to_string(), json!(true));
            row.insert("attinhcount".to_string(), json!(0));
            row.insert("attstattarget".to_string(), json!(-1));
            row.insert("attidentity".to_string(), json!(""));
            row.insert("attgenerated".to_string(), json!(""));
            row.insert("attcompression".to_string(), json!(""));
            row.insert("atthasmissing".to_string(), json!(false));
            row
        })
        .collect()
}

/// Base OID for synthesized `pg_attrdef.oid` values on the lazy path. The column
/// is NOT NULL in PostgreSQL but no consumer reads it (the constraint/column
/// views join `pg_attrdef` on `adrelid`+`adnum`), so a high, unread range avoids
/// colliding with real allocated OIDs.
const SYNTHETIC_ATTRDEF_OID_BASE: i32 = 900_000;

/// Build one `pg_catalog.pg_attrdef` row marking that column `adnum` of relation
/// `adrelid` has a default.
///
/// The compiled default expression (`adbin`, a node tree) is left NULL: we do not
/// store node trees, and the human-facing default text comes from the
/// integration-provided definition-text resolver. This row, joined with
/// `pg_attribute.atthasdef`, is the structural handle that
/// `information_schema.columns` and clients read.
pub fn build_pg_attrdef_row(oid: i32, adrelid: i32, adnum: i32) -> Row {
    let mut row = Row::new();
    row.insert("oid".to_string(), json!(oid));
    row.insert("adrelid".to_string(), json!(adrelid));
    row.insert("adnum".to_string(), json!(adnum));
    row.insert("adbin".to_string(), Value::Null);
    row
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
        CatalogTable::PgConfig => {
            for setting in fetch_config(source)? {
                rows.push(build_pg_config_row(&setting));
            }
        }
        CatalogTable::PgSettings => {
            for setting in fetch_settings(source)? {
                rows.push(build_pg_settings_row(&setting));
            }
        }
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
                        let natts = fetch_columns(source, &db.datname, &schema.name, &rel.name)?
                            .len() as i32;
                        rows.push(build_pg_class_row(&rel, schema.oid, natts));
                    }
                    // An index is itself a relation, so it gets its own pg_class
                    // row (relkind 'i') alongside the tables in this schema.
                    for index in fetch_indexes(source, &db.datname, &schema.name)? {
                        rows.push(build_index_pg_class_row(&index, schema.oid));
                    }
                }
            }
        }
        CatalogTable::PgIndex => {
            for db in fetch_databases(source)? {
                for schema in fetch_schemas(source, &db.datname)? {
                    for index in fetch_indexes(source, &db.datname, &schema.name)? {
                        rows.push(build_pg_index_row(&index));
                    }
                }
            }
        }
        CatalogTable::PgConstraint => {
            for db in fetch_databases(source)? {
                for schema in fetch_schemas(source, &db.datname)? {
                    for constraint in fetch_constraints(source, &db.datname, &schema.name)? {
                        rows.push(build_pg_constraint_row(&constraint));
                    }
                }
            }
        }
        CatalogTable::PgAttrdef => {
            // One pg_attrdef row per column flagged as having a default. The OID
            // is synthesized from a per-scan counter: nothing reads pg_attrdef.oid
            // (consumers join on adrelid+adnum), so only stability within a scan
            // matters, and the build order is deterministic.
            let mut synthetic_oid = SYNTHETIC_ATTRDEF_OID_BASE;
            for db in fetch_databases(source)? {
                for schema in fetch_schemas(source, &db.datname)? {
                    for rel in fetch_relations(source, &db.datname, &schema.name)? {
                        let columns = fetch_columns(source, &db.datname, &schema.name, &rel.name)?;
                        for (idx, col) in columns.iter().enumerate() {
                            if col.has_default {
                                rows.push(build_pg_attrdef_row(
                                    synthetic_oid,
                                    rel.oid,
                                    (idx + 1) as i32,
                                ));
                                synthetic_oid += 1;
                            }
                        }
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

/// A NULL placeholder used when building merge keys, distinct from any real
/// value's text so a present empty string never collides with absence.
const NULL_KEY: &str = "\u{0}NULL\u{0}";

/// Stringify a JSON value for use as one component of a merge key.
fn json_key_component(value: Option<&Value>) -> String {
    match value {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Null) | None => NULL_KEY.to_string(),
        Some(other) => other.to_string(),
    }
}

/// Stringify the value at row `i` of an Arrow array for use as a merge-key
/// component. Key columns are always OIDs (int) or names (text); anything else
/// yields a sentinel that cannot match a user key, so such a built-in row is
/// simply kept.
fn array_key_component(array: &ArrayRef, i: usize) -> String {
    if array.is_null(i) {
        return NULL_KEY.to_string();
    }
    if let Some(a) = array.as_any().downcast_ref::<Int32Array>() {
        return a.value(i).to_string();
    }
    if let Some(a) = array.as_any().downcast_ref::<Int64Array>() {
        return a.value(i).to_string();
    }
    if let Some(a) = array.as_any().downcast_ref::<StringArray>() {
        return a.value(i).to_string();
    }
    if let Some(a) = array.as_any().downcast_ref::<LargeStringArray>() {
        return a.value(i).to_string();
    }
    if let Some(a) = array.as_any().downcast_ref::<StringViewArray>() {
        return a.value(i).to_string();
    }
    "\u{0}UNMATCHABLE\u{0}".to_string()
}

/// Build the merge key of a user `Row` from `key_cols`.
fn user_row_key(row: &Row, key_cols: &[&str]) -> Vec<String> {
    key_cols
        .iter()
        .map(|c| json_key_component(row.get(*c)))
        .collect()
}

/// Return a copy of `batch` with every row whose merge key is present in
/// `user_keys` removed, so user rows win over the built-in rows they shadow.
/// When `user_keys` is empty the batch is returned unchanged.
fn drop_builtin_rows_shadowed_by_users(
    batch: &RecordBatch,
    schema: &SchemaRef,
    key_cols: &[&str],
    user_keys: &HashSet<Vec<String>>,
) -> DFResult<RecordBatch> {
    if user_keys.is_empty() {
        return Ok(batch.clone());
    }
    let key_arrays = key_cols
        .iter()
        .map(|c| Ok(batch.column(schema.index_of(c)?).clone()))
        .collect::<DFResult<Vec<ArrayRef>>>()?;
    let keep: Vec<bool> = (0..batch.num_rows())
        .map(|i| {
            let key: Vec<String> = key_arrays
                .iter()
                .map(|a| array_key_component(a, i))
                .collect();
            !user_keys.contains(&key)
        })
        .collect();
    Ok(filter_record_batch(batch, &BooleanArray::from(keep))?)
}

/// A [`TableProvider`] for one catalog table. On every scan it asks the source
/// for that table's user rows (here and now - nothing is cached), converts them
/// to a batch, and serves them *merged* with the built-in batches captured at
/// registration. DataFusion does all joins/filters/projection across providers.
pub struct LazyCatalogTableProvider {
    /// Which catalog table this provider serves.
    table: CatalogTable,
    /// The table's Arrow schema (taken from the YAML-loaded provider).
    schema: SchemaRef,
    /// The built-in system rows, captured once at registration (immutable).
    builtin_batches: Vec<RecordBatch>,
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
        let key_cols = self.table.key_columns();

        // Collect the user rows' identities. Two user rows sharing a key mean the
        // source defined the same object twice (e.g. two `mytable`s in one
        // schema) - that is a source error, surfaced rather than silently merged.
        let mut user_keys: HashSet<Vec<String>> = HashSet::with_capacity(user_rows.len());
        for row in &user_rows {
            let key = user_row_key(row, key_cols);
            if !user_keys.insert(key.clone()) {
                let (_schema_name, table_name) = self.table.location();
                return Err(DataFusionError::Execution(format!(
                    "lazy catalog source returned a duplicate {table_name} entry for ({}) = {key:?}",
                    key_cols.join(", "),
                )));
            }
        }

        let user_batch = rows_to_record_batch(&self.schema, &user_rows)?;

        // Merge: a user row replaces any built-in row with the same identity, so
        // a user-supplied object always wins over the one it shadows.
        let mut batches = Vec::with_capacity(self.builtin_batches.len() + 1);
        for builtin in &self.builtin_batches {
            batches.push(drop_builtin_rows_shadowed_by_users(
                builtin,
                &self.schema,
                key_cols,
                &user_keys,
            )?);
        }
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
                CatalogTable::PgIndex,
                CatalogTable::PgConstraint,
                CatalogTable::PgAttrdef,
                CatalogTable::PgConfig,
                CatalogTable::PgSettings,
                // information_schema.tables / .columns / .schemata are intentionally
                // omitted: they are SQL views (VIEWS_TO_REGISTER in session.rs) that
                // derive from the lazily-wrapped pg_class / pg_attribute /
                // pg_namespace above, so wrapping them as tables here would shadow the
                // view with a frozen MemTable. The InformationSchema* variants remain
                // available via `with_tables` for an embedder that wants the
                // materialized tables instead of the views.
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
            builtin_batches: builtin,
            source: source.clone(),
        });

        let _ = schema_provider.deregister_table(table_name);
        schema_provider.register_table(table_name.to_string(), provider)?;
    }

    // The catalog views (information_schema.columns and the rest) were planned
    // against the providers that existed before the swap above; a planned view
    // keeps reading the provider it was planned from, so without this it would
    // never see the lazy rows. Re-plan those views so they bind to the lazy
    // providers just installed.
    crate::session::replan_registered_views_against_current_providers(ctx).await?;

    Ok(())
}
