use arrow::array::{Array, Int32Array, Int64Array, LargeStringArray, RecordBatch, StringArray};
use datafusion::common::ScalarValue;
use datafusion::error::{DataFusionError, Result as DFResult};
use datafusion::execution::context::SessionContext;
use serde::Deserialize;

use crate::lazy_catalog::{
    build_index_pg_class_row, build_pg_attrdef_row, build_pg_attribute_rows, build_pg_class_row,
    build_pg_constraint_row, build_pg_index_row, build_pg_type_rowtype_row, ColumnSpec,
    ConstraintDef, ConstraintKind, IndexDef, RelationDef, RelationKind, DEFAULT_OWNER_ROLE_OID,
    FIRST_USER_OID,
};
use crate::session::rows_to_record_batch;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// One column of a user relation as the integration describes it on the wire.
///
/// The column NAME is not a field: callers pass each column as a single-entry
/// `name -> ColumnDef` map, so the name lives in the map key and this struct
/// carries only the column's properties.
#[derive(Debug, Clone, Deserialize)]
pub struct ColumnDef {
    /// Declared type text exactly as the integration writes it (`int`,
    /// `varchar(64)`, `text`, ...). Resolved to a `pg_type` OID by
    /// [`map_type_to_oid`]; unrecognized text falls back to `text`.
    #[serde(rename = "type")]
    pub col_type: String,
    /// Whether the column accepts NULL. Drives `pg_attribute.attnotnull` and the
    /// `information_schema.columns.is_nullable` flag derived from it.
    pub nullable: bool,
    /// Whether the column has a default expression. Drives `pg_attribute.atthasdef`
    /// and a `pg_attrdef` row; the default *text* is supplied at query time by the
    /// integration-supplied definition-text resolver (NULL until one is installed).
    /// Defaults to false, so existing callers and wire payloads need not set it.
    #[serde(default)]
    pub has_default: bool,
}

/// Per-call sequence making each `append_catalog_row` staging table name unique, so
/// concurrent appends can't collide on a shared register/deregister name.
static APPEND_STAGING_SEQ: AtomicU64 = AtomicU64::new(0);

/// Which schema names each database registered, keyed by database name.
///
/// `pg_namespace` is flattened across databases, so a namespace row alone does
/// not say which database asked for it. Dropping a database has to drop exactly
/// the schemas that database registered, and this map is what remembers them.
static DATABASE_SCHEMAS: std::sync::LazyLock<Mutex<HashMap<String, HashSet<String>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Give `database_name` an entry in the schema registry, empty if it had none.
///
/// Called on paths that may run before any schema is registered, so that a later
/// drop of the database sees a known database with no schemas rather than an
/// unknown one.
///
/// # Panics
///
/// Panics if the registry mutex was poisoned by a thread that panicked while
/// holding it.
fn ensure_database_registry(database_name: &str) {
    let mut registry = DATABASE_SCHEMAS.lock().unwrap();
    registry.entry(database_name.to_string()).or_default();
}

/// Record that `database_name` owns `schema_name`, creating the database's entry
/// if this is its first schema.
///
/// # Panics
///
/// Panics if the registry mutex was poisoned by a thread that panicked while
/// holding it.
fn add_schema_to_registry(database_name: &str, schema_name: &str) {
    let mut registry = DATABASE_SCHEMAS.lock().unwrap();
    registry
        .entry(database_name.to_string())
        .or_default()
        .insert(schema_name.to_string());
}

/// Forget that `database_name` owns `schema_name`, dropping the database's entry
/// once its last schema is gone. Unknown names are ignored.
///
/// # Panics
///
/// Panics if the registry mutex was poisoned by a thread that panicked while
/// holding it.
fn remove_schema_from_registry(database_name: &str, schema_name: &str) {
    let mut registry = DATABASE_SCHEMAS.lock().unwrap();
    if let Some(schemas) = registry.get_mut(database_name) {
        schemas.remove(schema_name);
        if schemas.is_empty() {
            registry.remove(database_name);
        }
    }
}

/// Remove `database_name` from the registry and return the schema names it owned
/// (empty when it owned none). Taking and returning in one locked step means the
/// caller can drop those schemas without holding the lock across `await` points.
///
/// # Panics
///
/// Panics if the registry mutex was poisoned by a thread that panicked while
/// holding it.
fn take_schemas_from_registry(database_name: &str) -> Vec<String> {
    let mut registry = DATABASE_SCHEMAS.lock().unwrap();
    registry
        .remove(database_name)
        .map(|set| set.into_iter().collect())
        .unwrap_or_default()
}

/// Resolve a column's declared type string (as the integration writes it, e.g.
/// `int`, `varchar(64)`, `text`) to its `PostgreSQL` `pg_type` OID. The OID is the
/// single source of truth for both `pg_attribute.atttypid` and the
/// `information_schema.columns` type names (via [`oid_to_type_names`]), so the
/// two never disagree. Unrecognized types fall back to `text` (OID 25), the
/// permissive default used across this module.
pub(crate) fn map_type_to_oid(t: &str) -> i32 {
    let lower = t.to_lowercase();
    match lower.as_str() {
        "int" | "integer" | "int4" => 23,
        "bigint" | "int8" => 20,
        "bool" | "boolean" => 16,
        other if other.starts_with("varchar") || other.starts_with("character varying") => 1043,
        _ => 25, // default to text
    }
}

/// Map a `PostgreSQL` type OID back to its `(data_type, udt_name)` pair as used in
/// `information_schema.columns`.
///
/// This is the inverse of [`map_type_to_oid`] and lets both the eager and lazy
/// registration paths render `information_schema.columns` rows from a column's
/// `pg_type` OID alone. Unknown OIDs fall back to `text`, mirroring the
/// permissive default used elsewhere in this module.
pub(crate) fn oid_to_type_names(oid: i32) -> (String, String) {
    match oid {
        23 => ("integer".to_string(), "int4".to_string()),
        20 => ("bigint".to_string(), "int8".to_string()),
        16 => ("boolean".to_string(), "bool".to_string()),
        1043 => ("character varying".to_string(), "varchar".to_string()),
        // OID 25 is text itself, and every unrecognized OID falls back to text,
        // so both land on the same names.
        _ => ("text".to_string(), "text".to_string()),
    }
}

/// The next unused OID in `ctx`'s catalog.
///
/// This is the auto-increment counter for hosts that do not supply their own
/// OIDs, and it lives IN the context rather than beside it: the next value is
/// read back out of the catalog `ctx` already holds. That is what makes it per
/// database without any shared state to scope, reset or clean up - each
/// database's context has its own catalog, so it has its own numbering.
///
/// Two databases giving their first table the same OID is correct, not a
/// collision: `PostgreSQL`'s OIDs are unique within a database, and two databases'
/// catalogs are never joined.
///
/// Numbering starts at [`FIRST_USER_OID`], clear of the built-in range, and the
/// value depends only on how many objects that database registered before this
/// one - fixed by the host's own registration order, so it is the same on every
/// run.
///
/// The maximum is taken across every catalog table drawing on the same OID
/// space, so a relation and a schema can never be handed the same number.
/// Callers needing several must insert each before asking for the next, or take
/// consecutive values from one call.
///
/// # Errors
///
/// Errors if the `max(oid)` query over the catalog tables cannot be planned or
/// executed, if it yields no batch at all, if its single column does not come
/// back as `Int64` (the type the explicit `::bigint` cast asks for), or if the
/// maximum it reports does not fit the `int4` oid this catalog hands out.
async fn next_catalog_oid(ctx: &SessionContext) -> DFResult<i32> {
    // Cast in SQL rather than guessing the Arrow type here: the oid columns are
    // int4, so an uncast max() comes back as Int32 and does not match the Int64
    // downcast below. Pinning the type in the query keeps the two in step, so the
    // maximum is really read instead of falling back to the floor - a floor that
    // would hand the first schema and the first relation the same OID.
    let batches = ctx
        .sql(
            "SELECT max(oid)::bigint FROM (
                 SELECT oid FROM pg_catalog.pg_class
                 UNION ALL SELECT oid FROM pg_catalog.pg_type
                 UNION ALL SELECT oid FROM pg_catalog.pg_namespace
                 UNION ALL SELECT oid FROM pg_catalog.pg_database
                 UNION ALL SELECT oid FROM pg_catalog.pg_attrdef
                 UNION ALL SELECT oid FROM pg_catalog.pg_constraint
             )",
        )
        .await?
        .collect()
        .await?;

    let column = batches
        .first()
        .map(|batch| batch.column(0))
        .ok_or_else(|| DataFusionError::Execution("max(oid) returned no rows".to_string()))?;
    let highest_oid_column = column
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| {
            DataFusionError::Execution(format!(
                "max(oid) came back as {} rather than Int64",
                column.data_type()
            ))
        })?;

    // NULL only if every source table is empty, which the floor covers.
    //
    // The maximum is i64 only because the query widens it with ::bigint, but the
    // oids behind it were written by whoever registered those objects - this
    // crate's counter for some, the embedder's own numbering for others - so the
    // narrowing back to i32 is range-checked rather than assumed. An oid that
    // does not fit int4 is not an oid this catalog could have handed out, and
    // truncating it here would restart numbering somewhere already in use.
    let highest_oid = if highest_oid_column.is_empty() || highest_oid_column.is_null(0) {
        0
    } else {
        let widened_oid = highest_oid_column.value(0);
        i32::try_from(widened_oid).map_err(|_| {
            DataFusionError::Execution(format!(
                "highest catalog oid {widened_oid} does not fit the int4 oid columns this catalog uses"
            ))
        })?
    };

    Ok(std::cmp::max(FIRST_USER_OID, highest_oid.saturating_add(1)))
}

/// Register a database in `pg_catalog.pg_database` and remember it in the
/// per-database schema registry.
///
/// Idempotent: a `pg_database` row of that name is left exactly as it is, so a
/// re-registration keeps the OID the database already has. A new row is given
/// the next free catalog OID and `PostgreSQL`'s conventional defaults (`SQL_ASCII`
/// era encoding 6, `C` collation, connectable, not a template).
///
/// # Errors
///
/// Errors if the `pg_database` probe or the `INSERT` cannot be planned or
/// executed, or if allocating the new OID fails (see [`next_catalog_oid`]).
pub async fn register_user_database(ctx: &SessionContext, database_name: &str) -> DFResult<()> {
    ensure_database_registry(database_name);

    let df: datafusion::prelude::DataFrame = ctx
        .sql("SELECT datname FROM pg_catalog.pg_database where datname=$database_name")
        .await?
        .with_param_values(vec![("database_name", ScalarValue::from(database_name))])?;
    if df.count().await? == 0 {
        let dbid = next_catalog_oid(ctx).await?;

        let df = ctx
            .sql(&format!(
                "INSERT INTO pg_catalog.pg_database (
            oid,
            datname,
            datdba,
            encoding,
            datcollate,
            datctype,
            datistemplate,
            datallowconn,
            datconnlimit,
            datfrozenxid,
            datminmxid,
            dattablespace,
            datacl
        ) VALUES (
            {},
            '{}',
            {DEFAULT_OWNER_ROLE_OID},
            6,
            'C',
            'C',
            false,
            true,
            -1,        
            726,
            1,
            1663,
            ARRAY['=Tc/dbuser', 'dbuser=CTc/dbuser']
        );",
                dbid,
                database_name.replace('\'', "''")
            ))
            .await?;
        df.collect().await?;
    }
    Ok(())
}

/// Register a schema and return its `pg_namespace.oid`. If a namespace of that name
/// already exists its OID is returned (registration is idempotent); otherwise one is
/// created. Callers pass this OID to `register_user_tables`/`register_user_index`/
/// `register_user_constraint`, which identify the namespace by OID rather than name -
/// the flattened catalog allows the same schema name under several databases, so a
/// name alone is ambiguous.
///
/// # Errors
///
/// Errors if the `pg_namespace` lookup or the `INSERT` cannot be planned or
/// executed, or if allocating the new OID fails (see [`next_catalog_oid`]).
pub async fn register_schema(
    ctx: &SessionContext,
    database_name: &str,
    schema_name: &str,
) -> DFResult<i32> {
    let oid = if let Some(oid) = get_schema_oid(ctx, schema_name).await? {
        oid
    } else {
        // Only ever a NEW schema: an existing one is found by the lookup
        // above and keeps the OID it already has, which is how a schema
        // named "public" holds on to the built-in 2200.
        let oid = next_catalog_oid(ctx).await?;
        let sql = format!(
            // The owner must be a role that exists: information_schema.schemata
            // inner-joins nspowner to pg_authid, so a made-up owner OID drops
            // the schema out of that view entirely rather than just blanking
            // its owner column.
            "INSERT INTO pg_catalog.pg_namespace (oid, nspname, nspowner, nspacl) VALUES ({oid}, '{}', {DEFAULT_OWNER_ROLE_OID}, NULL)",
            schema_name.replace('\'', "''")
        );
        ctx.sql(&sql).await?.collect().await?;
        oid
    };

    add_schema_to_registry(database_name, schema_name);

    Ok(oid)
}

/// Convert the wire-format column descriptions (each a single-entry
/// `name -> ColumnDef` map) into the `ColumnSpec` list the shared catalog
/// row-builders consume, resolving each declared type string to its `pg_type`
/// OID. A map with no entry is skipped.
fn column_specs_from_defs(columns: &[BTreeMap<String, ColumnDef>]) -> Vec<ColumnSpec> {
    columns
        .iter()
        .filter_map(|column| {
            column.iter().next().map(|(name, def)| {
                let spec =
                    ColumnSpec::new(name.clone(), map_type_to_oid(&def.col_type), def.nullable);
                if def.has_default {
                    spec.with_default()
                } else {
                    spec
                }
            })
        })
        .collect()
}

/// Register one user relation of the given [`RelationKind`] (table or view) in the
/// schema identified by `schema_oid`, writing every structural row both eager and
/// lazy registration share: a `pg_class` identity row carrying the relkind, its
/// composite rowtype in `pg_type`, a `pg_attribute` row per column, and a `pg_attrdef`
/// row per defaulted column. The `information_schema.tables` / `.columns` views
/// derive from those base-table rows, so they are not written here. Registration is
/// idempotent within the schema.
///
/// `register_user_tables` and `register_user_view` are thin wrappers that fix the
/// `kind`, so tables and views emit identical metadata except for the relkind.
///
/// `_database_name` is accepted to keep the public wrappers' signatures stable and
/// self-documenting. The relation's rows are keyed by schema OID, the OID comes
/// from the context's own counter, and the `information_schema` relations derive
/// from the base-table rows - so the database name is not needed here. Which
/// database this is is already settled by which context is being written to.
///
/// # Errors
///
/// Errors if `schema_oid` names no namespace in `pg_namespace`, if `columns`
/// holds more columns than the `int4` `pg_class.relnatts` column can carry, or if
/// any of the catalog queries, OID allocations or row appends fail.
async fn register_user_relation(
    ctx: &SessionContext,
    _database_name: &str,
    schema_oid: i32,
    relation_name: &str,
    kind: RelationKind,
    columns: Vec<BTreeMap<String, ColumnDef>>,
) -> DFResult<()> {
    // Idempotent: a relation of this name already registered IN THIS SCHEMA is left
    // as is. Scoped by relnamespace (pg_class identity is namespace + name), so the
    // same name under a different schema is not mistaken for a duplicate.
    let already_registered = ctx
        .sql(&format!(
            "SELECT 1 FROM pg_catalog.pg_class WHERE relname=$relname AND relnamespace={schema_oid}"
        ))
        .await?
        .with_param_values(vec![("relname", ScalarValue::from(relation_name))])?
        .count()
        .await?
        > 0;
    if already_registered {
        log::info!("relation already exists {relation_name}?");
        return Ok(());
    }

    // The relation is placed by OID (see `register_schema`); confirm the schema OID
    // resolves to a real schema before emitting rows that reference it.
    if get_schema_name(ctx, schema_oid).await?.is_none() {
        return Err(DataFusionError::Execution(format!(
            "schema oid {schema_oid} not found while registering relation '{relation_name}'"
        )));
    }

    // Two consecutive values: the relation and the composite rowtype it gets in
    // pg_type are separate objects sharing one OID space, and both rows are
    // written below before anything allocates again.
    let relation_oid = next_catalog_oid(ctx).await?;
    let type_oid = relation_oid + 1;
    let column_specs = column_specs_from_defs(&columns);
    let relation = RelationDef {
        oid: relation_oid,
        reltype_oid: type_oid,
        name: relation_name.to_string(),
        kind,
        owner_oid: None,
        has_index: false,
        has_rules: false,
        has_triggers: false,
        row_security: false,
    };

    // The column list arrives from the embedder (a host callback describing its
    // own relation); nothing in this crate bounds how many entries it contains,
    // so the count is range-checked instead of narrowed on trust. A truncated
    // count would be written to pg_class.relnatts and silently disagree with the
    // pg_attribute rows emitted below.
    let column_count = i32::try_from(column_specs.len()).map_err(|_| {
        DataFusionError::Execution(format!(
            "relation '{relation_name}' was registered with {} columns, more than the int4 pg_class.relnatts column can hold",
            column_specs.len()
        ))
    })?;

    // pg_class identity row, its composite rowtype in pg_type, and one
    // pg_attribute row per column - all from the same builders the lazy
    // registration path uses, so eager and lazy emit identical rows.
    append_catalog_row(
        ctx,
        "pg_catalog",
        "pg_class",
        build_pg_class_row(&relation, schema_oid, column_count),
    )
    .await?;
    append_catalog_row(
        ctx,
        "pg_catalog",
        "pg_type",
        build_pg_type_rowtype_row(&relation, schema_oid),
    )
    .await?;
    for attribute_row in build_pg_attribute_rows(relation_oid, &column_specs) {
        append_catalog_row(ctx, "pg_catalog", "pg_attribute", attribute_row).await?;
    }

    // One pg_attrdef row per column that has a default (its atthasdef flag is
    // already set on the pg_attribute row above). The default text is supplied at
    // call time by the integration-supplied definition-text resolver; this row is
    // the structural handle clients join on.
    for (idx, col) in column_specs.iter().enumerate() {
        if col.has_default {
            // 1-based attnum. `idx` indexes `column_specs`, whose length was
            // range-checked into `column_count` above, so `idx + 1` is at most
            // `column_count` and fits an i32 by that check.
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let adnum = (idx + 1) as i32;
            let attrdef_oid = next_catalog_oid(ctx).await?;
            append_catalog_row(
                ctx,
                "pg_catalog",
                "pg_attrdef",
                build_pg_attrdef_row(attrdef_oid, relation_oid, adnum),
            )
            .await?;
        }
    }

    // information_schema.tables / .columns are NOT written here: they are SQL views
    // (see VIEWS_TO_REGISTER in session.rs) that derive from the pg_class /
    // pg_attribute rows written above. Appending to them directly would target a
    // view and double-write what the view already derives.

    Ok(())
}

/// Register a user table in the schema identified by `schema_oid`. See
/// [`register_user_relation`] for the rows written; this fixes the relkind to a
/// table (`pg_class.relkind = 'r'`).
///
/// # Errors
///
/// Errors if `schema_oid` names no namespace in `pg_namespace`, or if any of the
/// catalog queries, OID allocations or row appends fail.
pub async fn register_user_tables(
    ctx: &SessionContext,
    database_name: &str,
    schema_oid: i32,
    table_name: &str,
    columns: Vec<BTreeMap<String, ColumnDef>>,
) -> DFResult<()> {
    register_user_relation(
        ctx,
        database_name,
        schema_oid,
        table_name,
        RelationKind::Table,
        columns,
    )
    .await
}

/// Register a user view in the schema identified by `schema_oid`. See
/// [`register_user_relation`] for the rows written; this fixes the relkind to a
/// view (`pg_class.relkind = 'v'`), so the view appears in `pg_class`, `pg_views`,
/// and `information_schema.views`. Its `definition` text comes from the
/// integration-supplied view-definition resolver at `pg_get_viewdef` call time
/// (NULL until one is installed); `columns` are the view's output columns.
///
/// # Errors
///
/// Errors if `schema_oid` names no namespace in `pg_namespace`, or if any of the
/// catalog queries, OID allocations or row appends fail.
pub async fn register_user_view(
    ctx: &SessionContext,
    database_name: &str,
    schema_oid: i32,
    view_name: &str,
    columns: Vec<BTreeMap<String, ColumnDef>>,
) -> DFResult<()> {
    register_user_relation(
        ctx,
        database_name,
        schema_oid,
        view_name,
        RelationKind::View,
        columns,
    )
    .await
}

/// Append one row, built as a `column -> JSON value` map, to a catalog table in
/// the given schema (`pg_catalog` or `information_schema`) by materializing it
/// against that table's Arrow schema and inserting it into the in-memory
/// provider. Columns absent from `row` take their schema default (NULL). Used by
/// the eager registration helpers to write rows whose columns include non-scalar
/// types (e.g. the `pg_index.indkey` list) that a literal `INSERT ... VALUES`
/// clause cannot express.
///
/// # Errors
///
/// Errors if the default catalog, the schema or the table cannot be resolved, if
/// `row` cannot be materialized against the table's Arrow schema, or if
/// registering the staging table or running the `INSERT` fails. The staging table
/// is deregistered before the insert's error is returned.
async fn append_catalog_row(
    ctx: &SessionContext,
    schema_name: &str,
    table_name: &str,
    row: BTreeMap<String, serde_json::Value>,
) -> DFResult<()> {
    let default_catalog = {
        let state = ctx.state();
        state.config_options().catalog.default_catalog.clone()
    };
    let catalog = ctx.catalog(&default_catalog).ok_or_else(|| {
        DataFusionError::Execution(format!("default catalog '{default_catalog}' not found"))
    })?;
    let schema_provider = catalog
        .schema(schema_name)
        .ok_or_else(|| DataFusionError::Execution(format!("schema '{schema_name}' not found")))?;
    let provider = schema_provider.table(table_name).await?.ok_or_else(|| {
        DataFusionError::Execution(format!("table '{schema_name}.{table_name}' not found"))
    })?;
    let schema = provider.schema();
    let batch = rows_to_record_batch(&schema, &[row])?;

    // Stage the row as a one-off source table, then INSERT ... SELECT it into the
    // catalog table so its columns are matched by the schema rather than by a
    // literal VALUES tuple. The staging table is dropped whether the insert
    // succeeds or fails. The name carries a per-call sequence so concurrent appends
    // to the same catalog table do not register/deregister a shared staging name.
    let seq = APPEND_STAGING_SEQ.fetch_add(1, Ordering::Relaxed);
    let staging_table = format!("__catalog_append_{schema_name}_{table_name}_{seq}");
    ctx.register_batch(&staging_table, batch)?;
    let inserted = ctx
        .sql(&format!(
            "INSERT INTO {schema_name}.{table_name} SELECT * FROM {staging_table}"
        ))
        .await;
    let inserted = match inserted {
        Ok(df) => df.collect().await,
        Err(e) => Err(e),
    };
    ctx.deregister_table(&staging_table)?;
    inserted?;
    Ok(())
}

/// Pre-register one index for an existing user table, the index analogue of
/// [`register_user_tables`]. It writes the two catalog rows that describe an
/// index: the index's own `pg_class` row (`relkind = 'i'`, which carries the
/// index name) and its `pg_catalog.pg_index` structure row, so `pg_indexes` and
/// `pg_get_indexdef` can describe the index.
///
/// `index_name` is the new index relation's name; `table_name` is the existing
/// table it indexes within the schema identified by `schema_oid` (passed in rather
/// than resolved by name, since the flattened catalog allows duplicate schema names
/// across databases); `key_attnums` lists the indexed columns by their 1-based
/// `pg_attribute.attnum`, in index order. The index's `pg_class.oid` is allocated
/// here. Re-registering an existing index name is a no-op.
///
/// # Errors
///
/// Errors if `table_name` is not registered in the namespace `schema_oid`, or if
/// any of the catalog queries, the OID allocation or the row appends fail.
pub async fn register_user_index(
    ctx: &SessionContext,
    schema_oid: i32,
    index_name: &str,
    table_name: &str,
    key_attnums: Vec<i32>,
    is_unique: bool,
    is_primary: bool,
) -> DFResult<()> {
    let Some(table_oid) = get_table_oid(ctx, schema_oid, table_name).await? else {
        return Err(DataFusionError::Execution(format!(
            "table '{table_name}' not found in schema oid {schema_oid} while registering index '{index_name}'"
        )));
    };

    if get_table_oid(ctx, schema_oid, index_name).await?.is_some() {
        log::info!("index already exists {index_name}?");
        return Ok(());
    }

    let index_oid = next_catalog_oid(ctx).await?;
    let index_def = IndexDef {
        index_oid,
        index_name: index_name.to_string(),
        table_oid,
        key_attnums,
        is_unique,
        is_primary,
    };

    append_catalog_row(
        ctx,
        "pg_catalog",
        "pg_class",
        build_index_pg_class_row(&index_def, schema_oid),
    )
    .await?;
    append_catalog_row(
        ctx,
        "pg_catalog",
        "pg_index",
        build_pg_index_row(&index_def),
    )
    .await?;

    Ok(())
}

/// Pre-register one constraint (primary key, unique, or foreign key) for an
/// existing user table, the constraint analogue of [`register_user_index`]. It
/// writes one `pg_catalog.pg_constraint` row, which the `information_schema`
/// constraint views (`table_constraints`, `key_column_usage`,
/// `constraint_column_usage`, `referential_constraints`) derive from.
///
/// `schema_oid` is the pre-resolved OID of the namespace the table lives in. It is
/// passed in rather than looked up from a schema name because the flattened catalog
/// allows the same schema name (e.g. `public`) under several databases, so a name
/// alone cannot identify the right namespace; the caller, which knows the database
/// context, resolves it. `key_attnums` are the constrained columns' 1-based attnums.
/// For a foreign key, `referenced_table_name` names the target table in the SAME
/// schema and `referenced_key_attnums` its referenced columns (positionally matched
/// to `key_attnums`); both are ignored for primary-key and unique constraints. The
/// call is idempotent: a constraint of the same name already on the table is left
/// untouched.
///
/// # Errors
///
/// Errors if `table_name` is not registered in the namespace `schema_oid`; if
/// `kind` is [`ConstraintKind::ForeignKey`] and `referenced_table_name` is `None`
/// or names a table not registered in that same namespace; if a foreign key's
/// `key_attnums` and `referenced_key_attnums` differ in length (they are matched
/// position by position); or if any of the catalog queries, the OID allocation or
/// the row append fail.
#[allow(clippy::too_many_arguments)]
pub async fn register_user_constraint(
    ctx: &SessionContext,
    schema_oid: i32,
    constraint_name: &str,
    table_name: &str,
    kind: ConstraintKind,
    key_attnums: Vec<i32>,
    referenced_table_name: Option<&str>,
    referenced_key_attnums: Vec<i32>,
) -> DFResult<()> {
    let Some(table_oid) = get_table_oid(ctx, schema_oid, table_name).await? else {
        return Err(DataFusionError::Execution(format!(
            "table '{table_name}' not found in schema oid {schema_oid} \
             while registering constraint '{constraint_name}'"
        )));
    };

    let already_registered = ctx
        .sql(&format!(
            "SELECT 1 FROM pg_catalog.pg_constraint \
             WHERE conname = $conname AND conrelid = {table_oid}"
        ))
        .await?
        .with_param_values(vec![("conname", ScalarValue::from(constraint_name))])?
        .count()
        .await?
        > 0;
    if already_registered {
        log::info!("constraint already exists {constraint_name}?");
        return Ok(());
    }

    let constraint_oid = next_catalog_oid(ctx).await?;
    let constraint = match kind {
        ConstraintKind::ForeignKey => {
            let Some(referenced_table) = referenced_table_name else {
                return Err(DataFusionError::Execution(format!(
                    "foreign key '{constraint_name}' requires a referenced table name"
                )));
            };
            let Some(referenced_oid) = get_table_oid(ctx, schema_oid, referenced_table).await?
            else {
                return Err(DataFusionError::Execution(format!(
                    "referenced table '{referenced_table}' not found in schema oid {schema_oid} \
                     while registering foreign key '{constraint_name}'"
                )));
            };
            ConstraintDef::foreign_key(
                constraint_oid,
                constraint_name,
                schema_oid,
                table_oid,
                key_attnums,
                referenced_oid,
                referenced_key_attnums,
                0,
            )?
        }
        ConstraintKind::PrimaryKey => ConstraintDef::primary_key(
            constraint_oid,
            constraint_name,
            schema_oid,
            table_oid,
            key_attnums,
            0,
        ),
        ConstraintKind::Unique => ConstraintDef::unique(
            constraint_oid,
            constraint_name,
            schema_oid,
            table_oid,
            key_attnums,
            0,
        ),
    };

    append_catalog_row(
        ctx,
        "pg_catalog",
        "pg_constraint",
        build_pg_constraint_row(&constraint),
    )
    .await?;

    Ok(())
}

/// Read a namespace's name from its OID. The inverse of [`get_schema_oid`], used by
/// `register_user_tables` to label `information_schema` rows once the namespace is
/// identified unambiguously by OID.
///
/// # Errors
///
/// Errors if the `pg_namespace` query cannot be planned or executed. An OID that
/// matches no namespace is `Ok(None)`, not an error.
async fn get_schema_name(ctx: &SessionContext, schema_oid: i32) -> DFResult<Option<String>> {
    let df = ctx
        .sql(&format!(
            "SELECT nspname FROM pg_catalog.pg_namespace WHERE oid = {schema_oid}"
        ))
        .await?;
    let batches = df.collect().await?;
    if batches.is_empty() || batches[0].num_rows() == 0 {
        return Ok(None);
    }
    Ok(collect_string_column(batches[0].column(0))
        .into_iter()
        .next())
}

/// Read the OID in the first row of a single-column `SELECT oid ...` result,
/// accepting either an `Int32` or `Int64` column (widened/narrowed to `i32`).
/// Returns `None` when the result has no rows. `kind` names the looked-up object
/// for the error text.
///
/// # Errors
///
/// Errors if the first column is neither an `Int32Array` nor an `Int64Array`,
/// which means the caller selected something other than an oid column, or if an
/// `Int64` cell holds a value outside the `int4` range oids live in.
fn first_oid_cell(batches: &[RecordBatch], kind: &str) -> DFResult<Option<i32>> {
    if batches.is_empty() || batches[0].num_rows() == 0 {
        return Ok(None);
    }

    let array = batches[0].column(0);
    if let Some(arr) = array.as_any().downcast_ref::<Int32Array>() {
        Ok(Some(arr.value(0)))
    } else if let Some(arr) = array.as_any().downcast_ref::<Int64Array>() {
        // An Int64 cell means the query or the engine widened an oid on the way
        // out. The row behind it was written by whoever registered the object, so
        // the narrowing is range-checked: a value that does not fit int4 names no
        // object this catalog holds, and truncating it would return the oid of a
        // different one.
        let widened_oid = arr.value(0);
        let oid = i32::try_from(widened_oid).map_err(|_| {
            DataFusionError::Execution(format!(
                "{kind} oid {widened_oid} does not fit the int4 oid columns this catalog uses"
            ))
        })?;
        Ok(Some(oid))
    } else {
        Err(DataFusionError::Execution(format!(
            "unexpected {kind} oid type"
        )))
    }
}

/// Look up a schema's OID by name from `pg_catalog.pg_namespace`, or `None` when
/// no schema of that name exists.
///
/// # Errors
///
/// Errors if the `pg_namespace` query cannot be planned or executed, or if its
/// oid column is not an integer column (see [`first_oid_cell`]).
async fn get_schema_oid(ctx: &SessionContext, schema_name: &str) -> DFResult<Option<i32>> {
    let df = ctx
        .sql("SELECT oid FROM pg_catalog.pg_namespace WHERE nspname=$schema")
        .await?
        .with_param_values(vec![("schema", ScalarValue::from(schema_name))])?;
    first_oid_cell(&df.collect().await?, "schema")
}

/// Look up a table's OID by name within a schema from `pg_catalog.pg_class`, or
/// `None` when no such table exists in that schema.
///
/// # Errors
///
/// Errors if the `pg_class` query cannot be planned or executed, or if its oid
/// column is not an integer column (see [`first_oid_cell`]).
async fn get_table_oid(
    ctx: &SessionContext,
    schema_oid: i32,
    table_name: &str,
) -> DFResult<Option<i32>> {
    let df = ctx
        .sql(&format!(
            "SELECT oid FROM pg_catalog.pg_class WHERE relname=$relname AND relnamespace={schema_oid}"
        ))
        .await?
        .with_param_values(vec![("relname", ScalarValue::from(table_name))])?;
    first_oid_cell(&df.collect().await?, "table")
}

/// Read a string column into owned `String`s, skipping NULL cells.
///
/// Accepts both `StringArray` and `LargeStringArray` because the catalog tables
/// are built from several sources that do not agree on the offset width. A
/// non-string column yields an empty vector rather than an error: callers use
/// this to enumerate names, where "no names" and "not a string column" lead to
/// the same no-op.
fn collect_string_column(column: &arrow::array::ArrayRef) -> Vec<String> {
    if let Some(arr) = column.as_any().downcast_ref::<StringArray>() {
        (0..arr.len())
            .filter(|&i| arr.is_valid(i))
            .map(|i| arr.value(i).to_string())
            .collect()
    } else if let Some(arr) = column.as_any().downcast_ref::<LargeStringArray>() {
        (0..arr.len())
            .filter(|&i| arr.is_valid(i))
            .map(|i| arr.value(i).to_string())
            .collect()
    } else {
        Vec::new()
    }
}

/// Remove every catalog row describing one user table: its `pg_class` identity
/// row, its composite rowtype in `pg_type`, its `pg_attribute` columns, and the
/// `pg_attrdef` / `pg_constraint` rows keyed by its OID.
///
/// Each table is rewritten with `INSERT OVERWRITE ... SELECT ... WHERE <> oid`,
/// which is how a row is deleted from an in-memory provider that has no DELETE.
/// The `information_schema` relations are SQL views over these base tables, so
/// they follow automatically and are not touched here.
///
/// An unknown schema or table is not an error - unregistering something that was
/// never registered leaves the catalog as it is.
///
/// # Errors
///
/// Errors if the schema/table lookups or any of the overwrite statements cannot
/// be planned or executed.
pub async fn unregister_tables(
    ctx: &SessionContext,
    database_name: &str,
    schema_name: &str,
    table_name: &str,
) -> DFResult<()> {
    ensure_database_registry(database_name);

    let Some(schema_oid) = get_schema_oid(ctx, schema_name).await? else {
        return Ok(());
    };

    let Some(table_oid) = get_table_oid(ctx, schema_oid, table_name).await? else {
        return Ok(());
    };

    ctx.sql(&format!(
        "INSERT OVERWRITE INTO pg_catalog.pg_attribute \
         SELECT * FROM pg_catalog.pg_attribute WHERE attrelid <> {table_oid}"
    ))
    .await?
    .collect()
    .await?;

    ctx.sql(&format!(
        "INSERT OVERWRITE INTO pg_catalog.pg_type \
         SELECT * FROM pg_catalog.pg_type WHERE typrelid <> {table_oid}"
    ))
    .await?
    .collect()
    .await?;

    ctx.sql(&format!(
        "INSERT OVERWRITE INTO pg_catalog.pg_class \
         SELECT * FROM pg_catalog.pg_class WHERE oid <> {table_oid}"
    ))
    .await?
    .collect()
    .await?;

    // The same registration path also writes pg_attrdef (defaulted columns) and
    // pg_constraint (PK/UNIQUE/FK) rows keyed by the table OID; drop those too so an
    // unregistered table leaves no orphaned default/constraint metadata behind.
    ctx.sql(&format!(
        "INSERT OVERWRITE INTO pg_catalog.pg_attrdef \
         SELECT * FROM pg_catalog.pg_attrdef WHERE adrelid <> {table_oid}"
    ))
    .await?
    .collect()
    .await?;

    ctx.sql(&format!(
        "INSERT OVERWRITE INTO pg_catalog.pg_constraint \
         SELECT * FROM pg_catalog.pg_constraint WHERE conrelid <> {table_oid}"
    ))
    .await?
    .collect()
    .await?;

    Ok(())
}

/// Unregister every relation in a schema and then the `pg_namespace` row itself,
/// and forget the schema in the per-database registry.
///
/// The relations are dropped first so no `pg_class` row is left pointing at a
/// namespace that no longer exists. A schema name that is not in `pg_namespace`
/// is still dropped from the registry, so the registry cannot outlive the
/// catalog.
///
/// # Errors
///
/// Errors if the namespace lookup, the relation enumeration, any per-table
/// unregistration, or the `pg_namespace` overwrite cannot be planned or executed.
pub async fn unregister_schema(
    ctx: &SessionContext,
    database_name: &str,
    schema_name: &str,
) -> DFResult<()> {
    ensure_database_registry(database_name);

    let Some(schema_oid) = get_schema_oid(ctx, schema_name).await? else {
        remove_schema_from_registry(database_name, schema_name);
        return Ok(());
    };

    let df = ctx
        .sql(&format!(
            "SELECT relname FROM pg_catalog.pg_class WHERE relnamespace = {schema_oid}"
        ))
        .await?;
    let batches = df.collect().await?;
    for batch in batches {
        if batch.num_rows() == 0 {
            continue;
        }
        let names = collect_string_column(batch.column(0));
        for table_name in names {
            unregister_tables(ctx, database_name, schema_name, &table_name).await?;
        }
    }

    ctx.sql(&format!(
        "INSERT OVERWRITE INTO pg_catalog.pg_namespace \
         SELECT * FROM pg_catalog.pg_namespace WHERE oid <> {schema_oid}"
    ))
    .await?
    .collect()
    .await?;

    remove_schema_from_registry(database_name, schema_name);

    Ok(())
}

/// Unregister a database: every schema it registered, then its `pg_database` row.
///
/// The schemas come from the per-database registry rather than from
/// `pg_namespace`, because namespaces are flattened across databases and a
/// namespace row does not say which database asked for it. Taking them from the
/// registry is what keeps this from dropping another database's schema of the
/// same name. A database with no `pg_database` row still has its registry entry
/// cleared, so a half-registered database leaves nothing behind.
///
/// # Errors
///
/// Errors if the `pg_database` lookup, any schema unregistration, or the
/// `pg_database` overwrite cannot be planned or executed.
pub async fn unregister_database(ctx: &SessionContext, database_name: &str) -> DFResult<()> {
    let df = ctx
        .sql("SELECT oid FROM pg_catalog.pg_database WHERE datname=$database")
        .await?
        .with_param_values(vec![("database", ScalarValue::from(database_name))])?;
    let batches = df.collect().await?;

    if batches.is_empty() || batches[0].num_rows() == 0 {
        let _ = take_schemas_from_registry(database_name);
        return Ok(());
    }

    let schema_names = take_schemas_from_registry(database_name);
    for schema in schema_names {
        unregister_schema(ctx, database_name, &schema).await?;
    }

    let escaped_name = database_name.replace('\'', "''");
    ctx.sql(&format!(
        "INSERT OVERWRITE INTO pg_catalog.pg_database \
         SELECT * FROM pg_catalog.pg_database WHERE datname <> '{escaped_name}'"
    ))
    .await?
    .collect()
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::get_base_session_context;

    /// Build one column in the wire format the registration helpers take: a
    /// single-entry map from the column name to its [`ColumnDef`]. The name is
    /// the map key rather than a field, which is why a column cannot be written
    /// as a plain struct literal.
    fn single_column_map(
        name: &str,
        col_type: &str,
        nullable: bool,
        has_default: bool,
    ) -> BTreeMap<String, ColumnDef> {
        let mut column = BTreeMap::new();
        column.insert(
            name.to_string(),
            ColumnDef {
                col_type: col_type.to_string(),
                nullable,
                has_default,
            },
        );
        column
    }

    /// The eager view path writes the same structural rows as a table, but with
    /// relkind 'v', so the view appears in `pg_class`, `information_schema.tables`
    /// (as a VIEW), and `pg_attribute` - the structural half of a view, ready for
    /// the definition resolver to supply its text at `pg_get_viewdef` call time.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_user_view_creates_view_relation() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
        )
        .await?;

        let schema_oid = register_schema(&ctx, "pgtry", "myschema").await?;

        let id = single_column_map("id", "int", false, false);
        register_user_view(&ctx, "pgtry", schema_oid, "active_users", vec![id]).await?;

        // A relkind 'v' pg_class row in the target schema.
        let df = ctx
            .sql(&format!(
                "SELECT 1 FROM pg_catalog.pg_class \
                 WHERE relname = 'active_users' AND relkind = 'v' AND relnamespace = {schema_oid}"
            ))
            .await?;
        assert_eq!(
            df.count().await?,
            1,
            "view must be a relkind 'v' pg_class row"
        );

        // information_schema.tables labels it a VIEW.
        let df = ctx
            .sql(
                "SELECT 1 FROM information_schema.tables \
                 WHERE table_name = 'active_users' AND table_type = 'VIEW'",
            )
            .await?;
        assert_eq!(
            df.count().await?,
            1,
            "information_schema.tables must report the view as a VIEW"
        );

        // Its output column is registered in pg_attribute.
        let df = ctx
            .sql(
                "SELECT 1 FROM pg_catalog.pg_attribute \
                 WHERE attrelid = (SELECT oid FROM pg_catalog.pg_class WHERE relname = 'active_users') \
                 AND attname = 'id'",
            )
            .await?;
        assert_eq!(df.count().await?, 1, "view column must be in pg_attribute");
        Ok(())
    }

    /// A registered table gets a `pg_class` row under the namespace it was
    /// registered in, and one `pg_attribute` row per column.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_user_tables_dynamic() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
        )
        .await?;

        let schema_oid = register_schema(&ctx, "pgtry", "myschema").await?;

        let id = single_column_map("id", "int", true, false);
        let name = single_column_map("name", "text", true, false);

        register_user_tables(&ctx, "pgtry", schema_oid, "contacts", vec![id, name]).await?;

        let df = ctx
            .sql("SELECT relname FROM pg_catalog.pg_class WHERE relname='contacts'")
            .await?;
        assert_eq!(df.count().await?, 1);

        let df = ctx
            .sql("SELECT nspname FROM pg_catalog.pg_namespace n JOIN pg_catalog.pg_class c ON n.oid=c.relnamespace WHERE c.relname='contacts'")
            .await?;
        let batches = df.collect().await?;
        let schema_name = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap()
            .value(0);
        assert_eq!(schema_name, "myschema");

        let df = ctx
            .sql(
                "SELECT attname FROM pg_catalog.pg_attribute \
                 WHERE attrelid = (SELECT oid FROM pg_catalog.pg_class WHERE relname='contacts') \
                 ORDER BY attnum",
            )
            .await?;
        let batches = df.collect().await?;
        assert_eq!(batches[0].num_rows(), 2);
        Ok(())
    }

    /// Regression: the eager registration API must identify the schema by OID,
    /// not by name. The flattened catalog allows the same schema name under
    /// several databases (a lazy source can produce two `public` namespaces); a
    /// name-only lookup would pick an arbitrary one and bind the table to the
    /// wrong namespace.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_user_tables_resolves_schema_by_oid_not_name() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
        )
        .await?;

        // Two namespaces with the SAME name but different OIDs, inserted directly
        // (register_schema dedupes by name, so it can't create the collision).
        for oid in [71001, 71002] {
            ctx.sql(&format!(
                "INSERT INTO pg_catalog.pg_namespace (oid, nspname, nspowner, nspacl) \
                 VALUES ({oid}, 'dupschema', 27735, NULL)"
            ))
            .await?
            .collect()
            .await?;
        }

        // Register a table against the SECOND namespace, by OID.
        let id = single_column_map("id", "int", false, false);
        register_user_tables(&ctx, "pgtry", 71002, "widget", vec![id]).await?;

        // The table lands in the OID we passed (71002), not the arbitrary first
        // same-named namespace (71001) a name lookup would have resolved to.
        let rows_in_requested_namespace = ctx
            .sql("SELECT 1 FROM pg_catalog.pg_class WHERE relname='widget' AND relnamespace=71002")
            .await?
            .count()
            .await?;
        assert_eq!(
            rows_in_requested_namespace, 1,
            "table must be registered under the OID passed (71002)"
        );
        let rows_in_samename_namespace = ctx
            .sql("SELECT 1 FROM pg_catalog.pg_class WHERE relname='widget' AND relnamespace=71001")
            .await?
            .count()
            .await?;
        assert_eq!(
            rows_in_samename_namespace, 0,
            "table must NOT land in the same-named namespace 71001 that was not requested"
        );
        Ok(())
    }

    /// A registered index gets its own relkind 'i' `pg_class` row plus a
    /// `pg_index` structure row that names the indexed table, its uniqueness and
    /// primary-key flags, and the indexed column attnums in `indkey`.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_user_index_dynamic() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
        )
        .await?;

        let schema_oid = register_schema(&ctx, "pgtry", "myschema").await?;

        let id = single_column_map("id", "int", false, false);
        register_user_tables(&ctx, "pgtry", schema_oid, "contacts", vec![id]).await?;

        register_user_index(
            &ctx,
            schema_oid,
            "contacts_pkey",
            "contacts",
            vec![1],
            true,
            true,
        )
        .await?;

        // The index gets its own pg_class row, relkind 'i'.
        let df = ctx
            .sql("SELECT relkind FROM pg_catalog.pg_class WHERE relname='contacts_pkey'")
            .await?;
        let batches = df.collect().await?;
        assert_eq!(batches[0].num_rows(), 1, "index needs a pg_class row");
        let relkind = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .value(0);
        assert_eq!(relkind, "i");

        // The pg_index structure row points at the table and is unique + primary,
        // joining through pg_class on the index name.
        let df = ctx
            .sql(
                "SELECT t.relname FROM pg_catalog.pg_index x \
                 JOIN pg_catalog.pg_class i ON i.oid = x.indexrelid \
                 JOIN pg_catalog.pg_class t ON t.oid = x.indrelid \
                 WHERE i.relname = 'contacts_pkey' AND x.indisunique AND x.indisprimary",
            )
            .await?;
        let batches = df.collect().await?;
        assert_eq!(batches[0].num_rows(), 1);
        let table_name = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .value(0);
        assert_eq!(table_name, "contacts");

        // indkey carries the indexed column's attnum.
        let df = ctx
            .sql(
                "SELECT unnest(indkey) AS k FROM pg_catalog.pg_index \
                 WHERE indexrelid = (SELECT oid FROM pg_catalog.pg_class WHERE relname='contacts_pkey')",
            )
            .await?;
        let batches = df.collect().await?;
        let attnum = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap()
            .value(0);
        assert_eq!(attnum, 1);
        Ok(())
    }

    /// conkey and confkey are matched position-by-position, so the two attnum
    /// lists must be the same length; an unequal pairing is rejected at
    /// construction, before it can be serialized into `pg_constraint`.
    #[test]
    fn test_foreign_key_rejects_mismatched_attnum_counts() {
        assert!(
            ConstraintDef::foreign_key(1, "fk", 10, 20, vec![1, 2], 30, vec![1], 0).is_err(),
            "2 key columns vs 1 referenced column must be rejected"
        );
        assert!(
            ConstraintDef::foreign_key(1, "fk", 10, 20, vec![1], 30, vec![1, 2], 0).is_err(),
            "1 key column vs 2 referenced columns must be rejected"
        );
        assert!(
            ConstraintDef::foreign_key(1, "fk", 10, 20, vec![1, 2], 30, vec![3, 4], 0).is_ok(),
            "equal-length pairings are accepted"
        );
    }

    /// Register the parent table 'users' (id, email) and the child table 'orders'
    /// (id, `user_id`) that the constraint test hangs its constraints on.
    async fn register_users_and_orders_tables(
        ctx: &SessionContext,
        schema_oid: i32,
    ) -> DFResult<()> {
        let users_id = single_column_map("id", "int", false, false);
        let users_email = single_column_map("email", "text", false, false);
        register_user_tables(
            ctx,
            "pgtry",
            schema_oid,
            "users",
            vec![users_id, users_email],
        )
        .await?;

        let orders_id = single_column_map("id", "int", false, false);
        let orders_user_id = single_column_map("user_id", "int", false, false);
        register_user_tables(
            ctx,
            "pgtry",
            schema_oid,
            "orders",
            vec![orders_id, orders_user_id],
        )
        .await
    }

    /// Register the three constraints the constraint test asserts on: a primary
    /// key and a unique constraint on 'users', and a foreign key from
    /// `orders.user_id` to users.id.
    async fn register_users_and_orders_constraints(
        ctx: &SessionContext,
        schema_oid: i32,
    ) -> DFResult<()> {
        register_user_constraint(
            ctx,
            schema_oid,
            "users_pkey",
            "users",
            ConstraintKind::PrimaryKey,
            vec![1],
            None,
            vec![],
        )
        .await?;
        register_user_constraint(
            ctx,
            schema_oid,
            "users_email_key",
            "users",
            ConstraintKind::Unique,
            vec![2],
            None,
            vec![],
        )
        .await?;
        register_user_constraint(
            ctx,
            schema_oid,
            "orders_user_id_fkey",
            "orders",
            ConstraintKind::ForeignKey,
            vec![2],
            Some("users"),
            vec![1],
        )
        .await
    }

    /// A primary key, a unique constraint and a foreign key each land in
    /// `pg_constraint` with the right contype, table, and column lists, the
    /// foreign key defaulting to NO ACTION update/delete rules; registering the
    /// same constraint twice leaves a single row.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_user_constraint_dynamic() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
        )
        .await?;

        let schema_oid = register_schema(&ctx, "pgtry", "myschema").await?;
        register_users_and_orders_tables(&ctx, schema_oid).await?;
        register_users_and_orders_constraints(&ctx, schema_oid).await?;

        // The primary key carries contype 'p' over the right table and column.
        let df = ctx
            .sql(
                "SELECT c.contype FROM pg_catalog.pg_constraint c \
                 JOIN pg_catalog.pg_class t ON t.oid = c.conrelid \
                 WHERE c.conname = 'users_pkey' AND t.relname = 'users' \
                 AND c.contype = 'p' AND c.conkey = [1]",
            )
            .await?;
        assert_eq!(df.count().await?, 1, "primary key row must be present");

        // The unique constraint targets the second column.
        let df = ctx
            .sql(
                "SELECT 1 FROM pg_catalog.pg_constraint \
                 WHERE conname = 'users_email_key' AND contype = 'u' AND conkey = [2]",
            )
            .await?;
        assert_eq!(
            df.count().await?,
            1,
            "unique constraint row must be present"
        );

        // The foreign key points at the parent table and column, with NO ACTION
        // ('a') update/delete rules.
        let df = ctx
            .sql(
                "SELECT 1 FROM pg_catalog.pg_constraint fk \
                 JOIN pg_catalog.pg_class parent ON parent.oid = fk.confrelid \
                 WHERE fk.conname = 'orders_user_id_fkey' AND fk.contype = 'f' \
                 AND parent.relname = 'users' AND fk.confkey = [1] \
                 AND fk.confupdtype = 'a' AND fk.confdeltype = 'a'",
            )
            .await?;
        assert_eq!(df.count().await?, 1, "foreign key row must reference users");

        // Re-registering the same constraint is a no-op (still one row).
        register_user_constraint(
            &ctx,
            schema_oid,
            "users_pkey",
            "users",
            ConstraintKind::PrimaryKey,
            vec![1],
            None,
            vec![],
        )
        .await?;
        let df = ctx
            .sql("SELECT 1 FROM pg_catalog.pg_constraint WHERE conname = 'users_pkey'")
            .await?;
        assert_eq!(
            df.count().await?,
            1,
            "constraint registration is idempotent"
        );
        Ok(())
    }

    /// Column registration must derive each column's `PostgreSQL` storage
    /// attributes (length, by-value, alignment, storage, collation) from its type
    /// and must back a defaulted column with both `atthasdef` and a `pg_attrdef`
    /// row, while columns without a default get neither.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_user_columns_fidelity_and_attrdef() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
        )
        .await?;

        let schema_oid = register_schema(&ctx, "pgtry", "myschema").await?;

        // 'id' int (no default), 'label' text (no default), 'created' bigint with
        // a default expression.
        let id = single_column_map("id", "int", false, false);
        let label = single_column_map("label", "text", true, false);
        let created = single_column_map("created", "bigint", false, true);
        register_user_tables(
            &ctx,
            "pgtry",
            schema_oid,
            "widgets",
            vec![id, label, created],
        )
        .await?;

        // pg_class records the column count and the heap access method.
        let df = ctx
            .sql(
                "SELECT 1 FROM pg_catalog.pg_class \
                 WHERE relname = 'widgets' AND relnatts = 3 AND relam = 2",
            )
            .await?;
        assert_eq!(
            df.count().await?,
            1,
            "pg_class relnatts/relam must be filled"
        );

        // The int column has fixed 4-byte, by-value, int-aligned storage.
        let df = ctx
            .sql(
                "SELECT 1 FROM pg_catalog.pg_attribute \
                 WHERE attrelid = (SELECT oid FROM pg_catalog.pg_class WHERE relname = 'widgets') \
                 AND attname = 'id' AND attlen = 4 AND attbyval = true AND attalign = 'i'",
            )
            .await?;
        assert_eq!(
            df.count().await?,
            1,
            "int4 storage attributes must be derived"
        );

        // The text column is variable-length, extended storage, default collation.
        let df = ctx
            .sql(
                "SELECT 1 FROM pg_catalog.pg_attribute \
                 WHERE attrelid = (SELECT oid FROM pg_catalog.pg_class WHERE relname = 'widgets') \
                 AND attname = 'label' AND attlen = -1 AND attbyval = false \
                 AND attstorage = 'x' AND attcollation = 100",
            )
            .await?;
        assert_eq!(
            df.count().await?,
            1,
            "text storage attributes must be derived"
        );

        // The defaulted column carries atthasdef and a backing pg_attrdef row.
        let df = ctx
            .sql(
                "SELECT 1 FROM pg_catalog.pg_attribute \
                 WHERE attrelid = (SELECT oid FROM pg_catalog.pg_class WHERE relname = 'widgets') \
                 AND attname = 'created' AND atthasdef = true",
            )
            .await?;
        assert_eq!(df.count().await?, 1, "defaulted column must set atthasdef");
        let df = ctx
            .sql(
                "SELECT 1 FROM pg_catalog.pg_attrdef \
                 WHERE adrelid = (SELECT oid FROM pg_catalog.pg_class WHERE relname = 'widgets') \
                 AND adnum = 3",
            )
            .await?;
        assert_eq!(
            df.count().await?,
            1,
            "defaulted column must get a pg_attrdef row"
        );

        // A column without a default has neither atthasdef nor a pg_attrdef row.
        let df = ctx
            .sql(
                "SELECT 1 FROM pg_catalog.pg_attrdef \
                 WHERE adrelid = (SELECT oid FROM pg_catalog.pg_class WHERE relname = 'widgets') \
                 AND adnum = 1",
            )
            .await?;
        assert_eq!(
            df.count().await?,
            0,
            "a column with no default has no pg_attrdef row"
        );
        Ok(())
    }

    /// `information_schema.tables` derives a full row for a registered table:
    /// catalog, schema, name, BASE TABLE type, and the insertable/typed flags.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_user_tables_information_schema() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
        )
        .await?;

        let schema_oid = register_schema(&ctx, "pgtry", "myschema").await?;

        let id_column = single_column_map("id", "int", true, false);
        let name_column = single_column_map("name", "text", true, false);

        register_user_tables(
            &ctx,
            "pgtry",
            schema_oid,
            "contacts",
            vec![id_column, name_column],
        )
        .await?;

        let df = ctx
            .sql("SELECT table_catalog, table_schema, table_name, table_type, is_insertable_into, is_typed \
                  FROM information_schema.tables \
                  WHERE table_schema='myschema' AND table_name='contacts'")
            .await?;
        let batches = df.collect().await?;
        assert_eq!(batches[0].num_rows(), 1);

        let cat = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::StringViewArray>()
            .unwrap()
            .value(0);
        let sch = batches[0]
            .column(1)
            .as_any()
            .downcast_ref::<arrow::array::StringViewArray>()
            .unwrap()
            .value(0);
        let name = batches[0]
            .column(2)
            .as_any()
            .downcast_ref::<arrow::array::StringViewArray>()
            .unwrap()
            .value(0);
        let typ = batches[0]
            .column(3)
            .as_any()
            .downcast_ref::<arrow::array::StringViewArray>()
            .unwrap()
            .value(0);
        let insertable = batches[0]
            .column(4)
            .as_any()
            .downcast_ref::<arrow::array::StringViewArray>()
            .unwrap()
            .value(0);
        let is_typed = batches[0]
            .column(5)
            .as_any()
            .downcast_ref::<arrow::array::StringViewArray>()
            .unwrap()
            .value(0);

        assert_eq!(cat, "pgtry");
        assert_eq!(sch, "myschema");
        assert_eq!(name, "contacts");
        assert_eq!(typ, "BASE TABLE");
        assert_eq!(insertable, "YES");
        assert_eq!(is_typed, "NO");
        Ok(())
    }

    /// `information_schema.columns` derives one row per registered column, in
    /// registration order, with the type names that match the column's `pg_type`
    /// OID and the nullability it was registered with.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_user_columns_information_schema() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
        )
        .await?;

        let schema_oid = register_schema(&ctx, "pgtry", "myschema").await?;

        let id_column = single_column_map("id", "int", true, false);
        let name_column = single_column_map("name", "text", true, false);

        register_user_tables(
            &ctx,
            "pgtry",
            schema_oid,
            "contacts",
            vec![id_column, name_column],
        )
        .await?;

        let df = ctx
            .sql(
                "SELECT column_name, ordinal_position, data_type, is_nullable \
                 FROM information_schema.columns \
                 WHERE table_schema='myschema' AND table_name='contacts' \
                 ORDER BY ordinal_position",
            )
            .await?;
        let batches = df.collect().await?;
        assert_eq!(batches[0].num_rows(), 2);

        let col0 = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::StringViewArray>()
            .unwrap()
            .value(0);
        let pos0 = batches[0]
            .column(1)
            .as_any()
            .downcast_ref::<arrow::array::Int32Array>()
            .unwrap()
            .value(0);
        let dt0 = batches[0]
            .column(2)
            .as_any()
            .downcast_ref::<arrow::array::StringViewArray>()
            .unwrap()
            .value(0);
        let nul0 = batches[0]
            .column(3)
            .as_any()
            .downcast_ref::<arrow::array::StringViewArray>()
            .unwrap()
            .value(0);

        let col1 = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::StringViewArray>()
            .unwrap()
            .value(1);
        let pos1 = batches[0]
            .column(1)
            .as_any()
            .downcast_ref::<arrow::array::Int32Array>()
            .unwrap()
            .value(1);
        let dt1 = batches[0]
            .column(2)
            .as_any()
            .downcast_ref::<arrow::array::StringViewArray>()
            .unwrap()
            .value(1);
        let nul1 = batches[0]
            .column(3)
            .as_any()
            .downcast_ref::<arrow::array::StringViewArray>()
            .unwrap()
            .value(1);

        assert_eq!(col0, "id");
        assert_eq!(pos0, 1);
        assert_eq!(dt0, "integer");
        assert_eq!(nul0, "YES");

        assert_eq!(col1, "name");
        assert_eq!(pos1, 2);
        assert_eq!(dt1, "text");
        assert_eq!(nul1, "YES");

        Ok(())
    }

    /// Registering the same table twice leaves one `pg_class` row and one
    /// `pg_attribute` row per column, so a host replaying its registrations does
    /// not duplicate the catalog.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_user_tables_idempotent() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
        )
        .await?;

        let schema_oid = register_schema(&ctx, "pgtry", "myschema").await?;

        let id_column = single_column_map("id", "int", true, false);
        let name_column = single_column_map("name", "text", true, false);

        register_user_tables(
            &ctx,
            "pgtry",
            schema_oid,
            "contacts",
            vec![id_column.clone(), name_column.clone()],
        )
        .await?;
        // call again to ensure idempotency
        register_user_tables(
            &ctx,
            "pgtry",
            schema_oid,
            "contacts",
            vec![id_column, name_column],
        )
        .await?;

        let df = ctx
            .sql("SELECT relname FROM pg_catalog.pg_class WHERE relname='contacts'")
            .await?;
        assert_eq!(df.count().await?, 1);

        let df = ctx
            .sql(
                "SELECT attname FROM pg_catalog.pg_attribute \
                 WHERE attrelid = (SELECT oid FROM pg_catalog.pg_class WHERE relname='contacts') \
                 ORDER BY attnum",
            )
            .await?;
        let batches = df.collect().await?;
        assert_eq!(batches[0].num_rows(), 2);
        Ok(())
    }

    /// Registering a schema writes its `pg_namespace` row.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_schema() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
        )
        .await?;

        register_schema(&ctx, "pgtry", "custom").await?;

        let df = ctx
            .sql("SELECT nspname FROM pg_catalog.pg_namespace WHERE nspname='custom'")
            .await?;
        assert_eq!(df.count().await?, 1);
        Ok(())
    }

    /// Registering a database writes its `pg_database` row.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_user_database() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
        )
        .await?;

        register_user_database(&ctx, "crm").await?;

        let df = ctx
            .sql("SELECT datname FROM pg_catalog.pg_database WHERE datname='crm'")
            .await?;
        assert_eq!(df.count().await?, 1);
        Ok(())
    }

    /// Unregistering a table removes its `pg_class`, `pg_attribute` and `pg_type`
    /// rowtype rows together, leaving no half-described relation behind.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_unregister_tables_removes_metadata() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
        )
        .await?;

        let schema_oid = register_schema(&ctx, "pgtry", "myschema").await?;

        let id = single_column_map("id", "int", false, false);
        register_user_tables(&ctx, "pgtry", schema_oid, "contacts", vec![id]).await?;

        unregister_tables(&ctx, "pgtry", "myschema", "contacts").await?;

        let df = ctx
            .sql("SELECT 1 FROM pg_catalog.pg_class WHERE relname='contacts'")
            .await?;
        assert_eq!(df.count().await?, 0);

        let df = ctx
            .sql(
                "SELECT 1 FROM pg_catalog.pg_attribute \
                 WHERE attrelid = (SELECT oid FROM pg_catalog.pg_class WHERE relname='contacts')",
            )
            .await?;
        assert_eq!(df.count().await?, 0);

        let df = ctx
            .sql("SELECT 1 FROM pg_catalog.pg_type WHERE typrelid = (SELECT oid FROM pg_catalog.pg_class WHERE relname='contacts')")
            .await?;
        assert_eq!(df.count().await?, 0);

        Ok(())
    }

    /// A table registered with a defaulted column and a primary key writes
    /// `pg_attrdef` and `pg_constraint` rows keyed by its OID; unregistering must
    /// drop both so no orphaned default/constraint metadata survives.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_unregister_tables_removes_attrdef_and_constraint_rows() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
        )
        .await?;

        let schema_oid = register_schema(&ctx, "pgtry", "myschema").await?;

        // Each column is its own single-entry map (the wire format). 'id' has no
        // default; 'note' carries one, so it seeds a pg_attrdef row.
        let id = single_column_map("id", "int", false, false);
        let note = single_column_map("note", "text", true, true);
        register_user_tables(&ctx, "pgtry", schema_oid, "contacts", vec![id, note]).await?;
        register_user_constraint(
            &ctx,
            schema_oid,
            "contacts_pkey",
            "contacts",
            ConstraintKind::PrimaryKey,
            vec![1],
            None,
            vec![],
        )
        .await?;

        // Both rows exist before unregistering.
        let attrdef_before = ctx
            .sql(
                "SELECT 1 FROM pg_catalog.pg_attrdef \
                 WHERE adrelid = (SELECT oid FROM pg_catalog.pg_class WHERE relname='contacts')",
            )
            .await?
            .count()
            .await?;
        assert_eq!(
            attrdef_before, 1,
            "defaulted column must seed a pg_attrdef row"
        );
        let constraint_before = ctx
            .sql("SELECT 1 FROM pg_catalog.pg_constraint WHERE conname='contacts_pkey'")
            .await?
            .count()
            .await?;
        assert_eq!(
            constraint_before, 1,
            "primary key must seed a pg_constraint row"
        );

        unregister_tables(&ctx, "pgtry", "myschema", "contacts").await?;

        // Both rows are gone afterwards. The table OID no longer resolves, so these
        // query the metadata tables directly by the now-absent name/conname.
        let attrdef_after = ctx
            .sql("SELECT 1 FROM pg_catalog.pg_attrdef WHERE adrelid IN (SELECT oid FROM pg_catalog.pg_class WHERE relname='contacts')")
            .await?
            .count()
            .await?;
        assert_eq!(
            attrdef_after, 0,
            "pg_attrdef row must be removed on unregister"
        );
        let constraint_after = ctx
            .sql("SELECT 1 FROM pg_catalog.pg_constraint WHERE conname='contacts_pkey'")
            .await?
            .count()
            .await?;
        assert_eq!(
            constraint_after, 0,
            "pg_constraint row must be removed on unregister"
        );
        Ok(())
    }

    /// Unregistering a schema drops the relations it held before dropping the
    /// `pg_namespace` row, so no `pg_class` row is left pointing at a namespace
    /// that no longer exists.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_unregister_schema_removes_tables() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
        )
        .await?;

        let schema_oid = register_schema(&ctx, "pgtry", "myschema").await?;

        let id = single_column_map("id", "int", false, false);
        register_user_tables(&ctx, "pgtry", schema_oid, "contacts", vec![id]).await?;

        unregister_schema(&ctx, "pgtry", "myschema").await?;

        let df = ctx
            .sql("SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname='myschema'")
            .await?;
        assert_eq!(df.count().await?, 0);

        let df = ctx
            .sql("SELECT 1 FROM pg_catalog.pg_class WHERE relname='contacts'")
            .await?;
        assert_eq!(df.count().await?, 0);

        Ok(())
    }

    /// Unregistering a database drops the schemas it registered and the relations
    /// in them along with its `pg_database` row, using the per-database registry
    /// to know which flattened namespaces belonged to it.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_unregister_database_removes_children() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
        )
        .await?;

        register_user_database(&ctx, "crm").await?;
        let schema_oid = register_schema(&ctx, "crm", "crm_schema").await?;

        let id = single_column_map("id", "int", false, false);
        register_user_tables(&ctx, "crm", schema_oid, "contacts", vec![id]).await?;

        unregister_database(&ctx, "crm").await?;

        let df = ctx
            .sql("SELECT 1 FROM pg_catalog.pg_database WHERE datname='crm'")
            .await?;
        assert_eq!(df.count().await?, 0);

        let df = ctx
            .sql("SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname='crm_schema'")
            .await?;
        assert_eq!(df.count().await?, 0);

        let df = ctx
            .sql("SELECT 1 FROM pg_catalog.pg_class WHERE relname='contacts'")
            .await?;
        assert_eq!(df.count().await?, 0);

        Ok(())
    }
}
