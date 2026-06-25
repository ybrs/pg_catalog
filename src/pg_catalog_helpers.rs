use arrow::array::{Array, Int32Array, Int64Array, LargeStringArray, StringArray};
use datafusion::common::ScalarValue;
use datafusion::error::{DataFusionError, Result as DFResult};
use datafusion::execution::context::SessionContext;
use serde::Deserialize;

use crate::lazy_catalog::{
    build_index_pg_class_row, build_info_columns_rows, build_info_tables_row, build_pg_attrdef_row,
    build_pg_attribute_rows, build_pg_class_row, build_pg_constraint_row, build_pg_index_row,
    build_pg_type_rowtype_row, ColumnSpec, ConstraintDef, ConstraintKind, IndexDef, RelationDef,
};
use crate::session::rows_to_record_batch;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

use once_cell::sync::Lazy;

#[derive(Debug, Clone, Deserialize)]
pub struct ColumnDef {
    #[serde(rename = "type")]
    pub col_type: String,
    pub nullable: bool,
    /// Whether the column has a default expression. Drives `pg_attribute.atthasdef`
    /// and a `pg_attrdef` row; the default *text* is supplied later (Phase 2).
    /// Defaults to false, so existing callers and wire payloads need not set it.
    #[serde(default)]
    pub has_default: bool,
}

static NEXT_OID: AtomicI32 = AtomicI32::new(50010);

static DATABASE_SCHEMAS: Lazy<Mutex<HashMap<String, HashSet<String>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn ensure_database_registry(database_name: &str) {
    let mut registry = DATABASE_SCHEMAS.lock().unwrap();
    registry
        .entry(database_name.to_string())
        .or_insert_with(HashSet::new);
}

fn add_schema_to_registry(database_name: &str, schema_name: &str) {
    let mut registry = DATABASE_SCHEMAS.lock().unwrap();
    registry
        .entry(database_name.to_string())
        .or_insert_with(HashSet::new)
        .insert(schema_name.to_string());
}

fn remove_schema_from_registry(database_name: &str, schema_name: &str) {
    let mut registry = DATABASE_SCHEMAS.lock().unwrap();
    if let Some(schemas) = registry.get_mut(database_name) {
        schemas.remove(schema_name);
        if schemas.is_empty() {
            registry.remove(database_name);
        }
    }
}

fn take_schemas_from_registry(database_name: &str) -> Vec<String> {
    let mut registry = DATABASE_SCHEMAS.lock().unwrap();
    registry
        .remove(database_name)
        .map(|set| set.into_iter().collect())
        .unwrap_or_default()
}

/// Resolve a column's declared type string (as the integration writes it, e.g.
/// `int`, `varchar(64)`, `text`) to its PostgreSQL `pg_type` OID. The OID is the
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

/// Map a PostgreSQL type OID back to its `(data_type, udt_name)` pair as used in
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
        25 => ("text".to_string(), "text".to_string()),
        _ => ("text".to_string(), "text".to_string()),
    }
}

pub async fn register_user_database(ctx: &SessionContext, database_name: &str) -> DFResult<()> {
    // let oid = NEXT_OID.fetch_add(1, Ordering::SeqCst);

    ensure_database_registry(database_name);

    let df: datafusion::prelude::DataFrame = ctx
        .sql("SELECT datname FROM pg_catalog.pg_database where datname=$database_name")
        .await?
        .with_param_values(vec![("database_name", ScalarValue::from(database_name))])?;
    if df.count().await? == 0 {
        let next_oid_df = ctx
            .sql("select max(oid)+1 from pg_catalog.pg_database")
            .await?;
        let batches = next_oid_df.collect().await?;
        let array = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let dbid = array.value(0);

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
            27735,
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
    let df = ctx
        .sql("select datname from pg_catalog.pg_database")
        .await?;
    df.show().await?;
    Ok(())
}

pub async fn register_schema(
    ctx: &SessionContext,
    database_name: &str,
    schema_name: &str,
) -> DFResult<()> {
    let df = ctx
        .sql("SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname=$schema")
        .await?
        .with_param_values(vec![("schema", ScalarValue::from(schema_name))])?;

    if df.count().await? == 0 {
        let oid = NEXT_OID.fetch_add(1, Ordering::SeqCst);
        let sql = format!(
            "INSERT INTO pg_catalog.pg_namespace (oid, nspname, nspowner, nspacl) VALUES ({oid}, '{}', 27735, NULL)",
            schema_name.replace('\'', "''")
        );
        ctx.sql(&sql).await?.collect().await?;
    }

    add_schema_to_registry(database_name, schema_name);

    Ok(())
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

pub async fn register_user_tables(
    ctx: &SessionContext,
    database_name: &str,
    schema_name: &str,
    table_name: &str,
    columns: Vec<BTreeMap<String, ColumnDef>>,
) -> DFResult<()> {
    // Idempotent: a relation already registered under this name is left as is.
    let already_registered = ctx
        .sql("SELECT 1 FROM pg_catalog.pg_class WHERE relname=$relname")
        .await?
        .with_param_values(vec![("relname", ScalarValue::from(table_name))])?
        .count()
        .await?
        > 0;
    if already_registered {
        log::info!("table already exists {table_name}?");
        return Ok(());
    }

    let Some(schema_oid) = get_schema_oid(ctx, schema_name).await? else {
        return Err(DataFusionError::Execution(format!(
            "schema '{schema_name}' not found while registering table '{table_name}'"
        )));
    };

    let table_oid = NEXT_OID.fetch_add(1, Ordering::SeqCst);
    let type_oid = NEXT_OID.fetch_add(1, Ordering::SeqCst);
    let column_specs = column_specs_from_defs(&columns);
    let relation = RelationDef::table(table_oid, type_oid, table_name);

    // pg_class identity row, its composite rowtype in pg_type, and one
    // pg_attribute row per column - all from the same builders the lazy
    // registration path uses, so eager and lazy emit identical rows.
    append_catalog_row(
        ctx,
        "pg_catalog",
        "pg_class",
        build_pg_class_row(&relation, schema_oid, column_specs.len() as i32),
    )
    .await?;
    append_catalog_row(
        ctx,
        "pg_catalog",
        "pg_type",
        build_pg_type_rowtype_row(&relation, schema_oid),
    )
    .await?;
    for attribute_row in build_pg_attribute_rows(table_oid, &column_specs) {
        append_catalog_row(ctx, "pg_catalog", "pg_attribute", attribute_row).await?;
    }

    // One pg_attrdef row per column that has a default (its atthasdef flag is
    // already set on the pg_attribute row above). The default text is supplied
    // later (Phase 2); this is the structural handle clients join on.
    for (idx, col) in column_specs.iter().enumerate() {
        if col.has_default {
            let adnum = (idx + 1) as i32;
            let attrdef_oid = NEXT_OID.fetch_add(1, Ordering::SeqCst);
            append_catalog_row(
                ctx,
                "pg_catalog",
                "pg_attrdef",
                build_pg_attrdef_row(attrdef_oid, table_oid, adnum),
            )
            .await?;
        }
    }

    // Reflect the same relation in information_schema, where ORMs and BI tools
    // read it - again via the shared builders rather than hand-written INSERTs.
    append_catalog_row(
        ctx,
        "information_schema",
        "tables",
        build_info_tables_row(database_name, schema_name, &relation),
    )
    .await?;
    for column_row in build_info_columns_rows(database_name, schema_name, table_name, &column_specs)
    {
        append_catalog_row(ctx, "information_schema", "columns", column_row).await?;
    }

    Ok(())
}

/// Append one row, built as a `column -> JSON value` map, to a catalog table in
/// the given schema (`pg_catalog` or `information_schema`) by materializing it
/// against that table's Arrow schema and inserting it into the in-memory
/// provider. Columns absent from `row` take their schema default (NULL). Used by
/// the eager registration helpers to write rows whose columns include non-scalar
/// types (e.g. the `pg_index.indkey` list) that a literal `INSERT ... VALUES`
/// clause cannot express.
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
    // succeeds or fails.
    let staging_table = format!("__catalog_append_{schema_name}_{table_name}");
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
/// table it indexes within `schema_name`; `key_attnums` lists the indexed columns
/// by their 1-based `pg_attribute.attnum`, in index order. The index's
/// `pg_class.oid` is allocated here. Errors if the schema or table is not
/// registered; re-registering an existing index name is a no-op.
pub async fn register_user_index(
    ctx: &SessionContext,
    schema_name: &str,
    index_name: &str,
    table_name: &str,
    key_attnums: Vec<i32>,
    is_unique: bool,
    is_primary: bool,
) -> DFResult<()> {
    let Some(schema_oid) = get_schema_oid(ctx, schema_name).await? else {
        return Err(DataFusionError::Execution(format!(
            "schema '{schema_name}' not found while registering index '{index_name}'"
        )));
    };
    let Some(table_oid) = get_table_oid(ctx, schema_oid, table_name).await? else {
        return Err(DataFusionError::Execution(format!(
            "table '{schema_name}.{table_name}' not found while registering index '{index_name}'"
        )));
    };

    if get_table_oid(ctx, schema_oid, index_name).await?.is_some() {
        log::info!("index already exists {index_name}?");
        return Ok(());
    }

    let index_oid = NEXT_OID.fetch_add(1, Ordering::SeqCst);
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
/// `key_attnums` are the constrained columns' 1-based attnums. For a foreign key,
/// `referenced_table_name` names the target table within `schema_name` and
/// `referenced_key_attnums` its referenced columns (positionally matched to
/// `key_attnums`); both are ignored for primary-key and unique constraints. The
/// call is idempotent: a constraint of the same name already on the table is left
/// untouched.
#[allow(clippy::too_many_arguments)]
pub async fn register_user_constraint(
    ctx: &SessionContext,
    schema_name: &str,
    constraint_name: &str,
    table_name: &str,
    kind: ConstraintKind,
    key_attnums: Vec<i32>,
    referenced_table_name: Option<&str>,
    referenced_key_attnums: Vec<i32>,
) -> DFResult<()> {
    let Some(schema_oid) = get_schema_oid(ctx, schema_name).await? else {
        return Err(DataFusionError::Execution(format!(
            "schema '{schema_name}' not found while registering constraint '{constraint_name}'"
        )));
    };
    let Some(table_oid) = get_table_oid(ctx, schema_oid, table_name).await? else {
        return Err(DataFusionError::Execution(format!(
            "table '{schema_name}.{table_name}' not found while registering constraint '{constraint_name}'"
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

    let constraint_oid = NEXT_OID.fetch_add(1, Ordering::SeqCst);
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
                    "referenced table '{schema_name}.{referenced_table}' not found \
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
            )
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

async fn get_schema_oid(ctx: &SessionContext, schema_name: &str) -> DFResult<Option<i32>> {
    let df = ctx
        .sql("SELECT oid FROM pg_catalog.pg_namespace WHERE nspname=$schema")
        .await?
        .with_param_values(vec![("schema", ScalarValue::from(schema_name))])?;
    let batches = df.collect().await?;

    if batches.is_empty() || batches[0].num_rows() == 0 {
        return Ok(None);
    }

    let array = batches[0].column(0);
    if let Some(arr) = array.as_any().downcast_ref::<Int32Array>() {
        Ok(Some(arr.value(0)))
    } else if let Some(arr) = array.as_any().downcast_ref::<Int64Array>() {
        Ok(Some(arr.value(0) as i32))
    } else {
        Err(DataFusionError::Execution(
            "unexpected schema oid type".to_string(),
        ))
    }
}

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
    let batches = df.collect().await?;

    if batches.is_empty() || batches[0].num_rows() == 0 {
        return Ok(None);
    }

    let array = batches[0].column(0);
    if let Some(arr) = array.as_any().downcast_ref::<Int32Array>() {
        Ok(Some(arr.value(0)))
    } else if let Some(arr) = array.as_any().downcast_ref::<Int64Array>() {
        Ok(Some(arr.value(0) as i32))
    } else {
        Err(DataFusionError::Execution(
            "unexpected table oid type".to_string(),
        ))
    }
}

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

    Ok(())
}

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

    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_user_tables_dynamic() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
            None,
        )
        .await?;

        register_schema(&ctx, "pgtry", "myschema").await?;

        let mut c1 = BTreeMap::new();
        c1.insert(
            "id".to_string(),
            ColumnDef {
                col_type: "int".to_string(),
                nullable: true,
                has_default: false,
            },
        );
        let mut c2 = BTreeMap::new();
        c2.insert(
            "name".to_string(),
            ColumnDef {
                col_type: "text".to_string(),
                nullable: true,
                has_default: false,
            },
        );

        register_user_tables(&ctx, "pgtry", "myschema", "contacts", vec![c1, c2]).await?;

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

    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_user_index_dynamic() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
            None,
        )
        .await?;

        register_schema(&ctx, "pgtry", "myschema").await?;

        let mut c1 = BTreeMap::new();
        c1.insert(
            "id".to_string(),
            ColumnDef {
                col_type: "int".to_string(),
                nullable: false,
                has_default: false,
            },
        );
        register_user_tables(&ctx, "pgtry", "myschema", "contacts", vec![c1]).await?;

        register_user_index(
            &ctx,
            "myschema",
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

    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_user_constraint_dynamic() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
            None,
        )
        .await?;

        register_schema(&ctx, "pgtry", "myschema").await?;

        // Parent table 'users' (id, email) and child 'orders' (id, user_id).
        let mut users_id = BTreeMap::new();
        users_id.insert(
            "id".to_string(),
            ColumnDef {
                col_type: "int".to_string(),
                nullable: false,
                has_default: false,
            },
        );
        let mut users_email = BTreeMap::new();
        users_email.insert(
            "email".to_string(),
            ColumnDef {
                col_type: "text".to_string(),
                nullable: false,
                has_default: false,
            },
        );
        register_user_tables(
            &ctx,
            "pgtry",
            "myschema",
            "users",
            vec![users_id, users_email],
        )
        .await?;

        let mut orders_id = BTreeMap::new();
        orders_id.insert(
            "id".to_string(),
            ColumnDef {
                col_type: "int".to_string(),
                nullable: false,
                has_default: false,
            },
        );
        let mut orders_user_id = BTreeMap::new();
        orders_user_id.insert(
            "user_id".to_string(),
            ColumnDef {
                col_type: "int".to_string(),
                nullable: false,
                has_default: false,
            },
        );
        register_user_tables(
            &ctx,
            "pgtry",
            "myschema",
            "orders",
            vec![orders_id, orders_user_id],
        )
        .await?;

        // A primary key and a unique constraint on 'users', and a foreign key
        // from orders.user_id -> users.id.
        register_user_constraint(
            &ctx,
            "myschema",
            "users_pkey",
            "users",
            ConstraintKind::PrimaryKey,
            vec![1],
            None,
            vec![],
        )
        .await?;
        register_user_constraint(
            &ctx,
            "myschema",
            "users_email_key",
            "users",
            ConstraintKind::Unique,
            vec![2],
            None,
            vec![],
        )
        .await?;
        register_user_constraint(
            &ctx,
            "myschema",
            "orders_user_id_fkey",
            "orders",
            ConstraintKind::ForeignKey,
            vec![2],
            Some("users"),
            vec![1],
        )
        .await?;

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
            "myschema",
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

    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_user_columns_fidelity_and_attrdef() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
            None,
        )
        .await?;

        register_schema(&ctx, "pgtry", "myschema").await?;

        // 'id' int (no default), 'label' text (no default), 'created' bigint with
        // a default expression.
        let mut id = BTreeMap::new();
        id.insert(
            "id".to_string(),
            ColumnDef {
                col_type: "int".to_string(),
                nullable: false,
                has_default: false,
            },
        );
        let mut label = BTreeMap::new();
        label.insert(
            "label".to_string(),
            ColumnDef {
                col_type: "text".to_string(),
                nullable: true,
                has_default: false,
            },
        );
        let mut created = BTreeMap::new();
        created.insert(
            "created".to_string(),
            ColumnDef {
                col_type: "bigint".to_string(),
                nullable: false,
                has_default: true,
            },
        );
        register_user_tables(
            &ctx,
            "pgtry",
            "myschema",
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

    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_user_tables_information_schema() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
            None,
        )
        .await?;

        register_schema(&ctx, "pgtry", "myschema").await?;

        let mut c1 = BTreeMap::new();
        c1.insert(
            "id".to_string(),
            ColumnDef {
                col_type: "int".to_string(),
                nullable: true,
                has_default: false,
            },
        );
        let mut c2 = BTreeMap::new();
        c2.insert(
            "name".to_string(),
            ColumnDef {
                col_type: "text".to_string(),
                nullable: true,
                has_default: false,
            },
        );

        register_user_tables(&ctx, "pgtry", "myschema", "contacts", vec![c1, c2]).await?;

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
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap()
            .value(0);
        let sch = batches[0]
            .column(1)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap()
            .value(0);
        let name = batches[0]
            .column(2)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap()
            .value(0);
        let typ = batches[0]
            .column(3)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap()
            .value(0);
        let insertable = batches[0]
            .column(4)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap()
            .value(0);
        let is_typed = batches[0]
            .column(5)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
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

    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_user_columns_information_schema() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
            None,
        )
        .await?;

        register_schema(&ctx, "pgtry", "myschema").await?;

        let mut c1 = BTreeMap::new();
        c1.insert(
            "id".to_string(),
            ColumnDef {
                col_type: "int".to_string(),
                nullable: true,
                has_default: false,
            },
        );
        let mut c2 = BTreeMap::new();
        c2.insert(
            "name".to_string(),
            ColumnDef {
                col_type: "text".to_string(),
                nullable: true,
                has_default: false,
            },
        );

        register_user_tables(&ctx, "pgtry", "myschema", "contacts", vec![c1, c2]).await?;

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
            .downcast_ref::<arrow::array::StringArray>()
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
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap()
            .value(0);
        let nul0 = batches[0]
            .column(3)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap()
            .value(0);

        let col1 = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
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
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap()
            .value(1);
        let nul1 = batches[0]
            .column(3)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
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

    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_user_tables_idempotent() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
            None,
        )
        .await?;

        register_schema(&ctx, "pgtry", "myschema").await?;

        let mut c1 = BTreeMap::new();
        c1.insert(
            "id".to_string(),
            ColumnDef {
                col_type: "int".to_string(),
                nullable: true,
                has_default: false,
            },
        );
        let mut c2 = BTreeMap::new();
        c2.insert(
            "name".to_string(),
            ColumnDef {
                col_type: "text".to_string(),
                nullable: true,
                has_default: false,
            },
        );

        register_user_tables(
            &ctx,
            "pgtry",
            "myschema",
            "contacts",
            vec![c1.clone(), c2.clone()],
        )
        .await?;
        // call again to ensure idempotency
        register_user_tables(&ctx, "pgtry", "myschema", "contacts", vec![c1, c2]).await?;

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

    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_schema() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
            None,
        )
        .await?;

        register_schema(&ctx, "pgtry", "custom").await?;

        let df = ctx
            .sql("SELECT nspname FROM pg_catalog.pg_namespace WHERE nspname='custom'")
            .await?;
        assert_eq!(df.count().await?, 1);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_user_database() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
            None,
        )
        .await?;

        register_user_database(&ctx, "crm").await?;

        let df = ctx
            .sql("SELECT datname FROM pg_catalog.pg_database WHERE datname='crm'")
            .await?;
        assert_eq!(df.count().await?, 1);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_unregister_tables_removes_metadata() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
            None,
        )
        .await?;

        register_schema(&ctx, "pgtry", "myschema").await?;

        let mut c1 = BTreeMap::new();
        c1.insert(
            "id".to_string(),
            ColumnDef {
                col_type: "int".to_string(),
                nullable: false,
                has_default: false,
            },
        );
        register_user_tables(&ctx, "pgtry", "myschema", "contacts", vec![c1]).await?;

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

    #[tokio::test(flavor = "multi_thread")]
    async fn test_unregister_schema_removes_tables() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
            None,
        )
        .await?;

        register_schema(&ctx, "pgtry", "myschema").await?;

        let mut c1 = BTreeMap::new();
        c1.insert(
            "id".to_string(),
            ColumnDef {
                col_type: "int".to_string(),
                nullable: false,
                has_default: false,
            },
        );
        register_user_tables(&ctx, "pgtry", "myschema", "contacts", vec![c1]).await?;

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

    #[tokio::test(flavor = "multi_thread")]
    async fn test_unregister_database_removes_children() -> DFResult<()> {
        let (ctx, _) = get_base_session_context(
            Some("pg_catalog_data/pg_schema"),
            "pgtry".to_string(),
            "public".to_string(),
            None,
        )
        .await?;

        register_user_database(&ctx, "crm").await?;
        register_schema(&ctx, "crm", "crm_schema").await?;

        let mut c1 = BTreeMap::new();
        c1.insert(
            "id".to_string(),
            ColumnDef {
                col_type: "int".to_string(),
                nullable: false,
                has_default: false,
            },
        );
        register_user_tables(&ctx, "crm", "crm_schema", "contacts", vec![c1]).await?;

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
