use arrow::array::{Array, Int32Array, Int64Array, LargeStringArray, StringArray};
use datafusion::common::ScalarValue;
use datafusion::error::{DataFusionError, Result as DFResult};
use datafusion::execution::context::SessionContext;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

use once_cell::sync::Lazy;

#[derive(Debug, Clone, Deserialize)]
pub struct ColumnDef {
    #[serde(rename = "type")]
    pub col_type: String,
    pub nullable: bool,
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

fn map_type_to_oid(t: &str) -> i32 {
    match t.to_lowercase().as_str() {
        "int" | "integer" | "int4" => 23,
        "bigint" | "int8" => 20,
        "bool" | "boolean" => 16,
        _ => 25, // default to text
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
        let getiddf = ctx
            .sql("select max(oid)+1 from pg_catalog.pg_database")
            .await?;
        let batches = getiddf.collect().await?;
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

pub async fn register_user_tables(
    ctx: &SessionContext,
    _database_name: &str,
    schema_name: &str,
    table_name: &str,
    columns: Vec<BTreeMap<String, ColumnDef>>,
) -> DFResult<()> {
    let df = ctx
        .sql("SELECT 1 FROM pg_catalog.pg_class WHERE relname=$relname")
        .await?
        .with_param_values(vec![("relname", ScalarValue::from(table_name))])?;

    if df.count().await? > 0 {
        log::info!("table already exists {:}?", table_name);
        return Ok(());
    }

    let table_oid = NEXT_OID.fetch_add(1, Ordering::SeqCst);
    let type_oid = NEXT_OID.fetch_add(1, Ordering::SeqCst);

    let ns_df = ctx
        .sql("SELECT oid FROM pg_catalog.pg_namespace WHERE nspname=$schema")
        .await?
        .with_param_values(vec![("schema", ScalarValue::from(schema_name))])?;
    let ns_batches = ns_df.collect().await?;
    let schema_oid = if ns_batches.is_empty() || ns_batches[0].num_rows() == 0 {
        return Err(DataFusionError::Execution("schema not found".to_string()));
    } else {
        let arr = ns_batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int32Array>()
            .unwrap();
        arr.value(0)
    };

    if ctx
        .sql(&format!(
            "SELECT 1 FROM pg_catalog.pg_class WHERE oid = {table_oid}"
        ))
        .await?
        .count()
        .await?
        == 0
    {
        let sql = format!(
            "INSERT INTO pg_catalog.pg_class \
                 (oid, relname, relnamespace, relkind, reltuples, reltype, relispartition) \
                 VALUES ({table_oid},'{}',{schema_oid},'r',0,{type_oid}, false)",
            table_name.replace('\'', "''")
        );
        ctx.sql(&sql).await?.collect().await?;
    }

    if ctx
        .sql(&format!(
            "SELECT 1 FROM pg_catalog.pg_type WHERE oid = {type_oid}"
        ))
        .await?
        .count()
        .await?
        == 0
    {
        let sql = format!(
            "INSERT INTO pg_catalog.pg_type \
                 (oid, typname, typrelid, typlen, typcategory) \
                 VALUES ({type_oid},'_{table_name}',{table_oid},-1,'C')"
        );
        ctx.sql(&sql).await?.collect().await?;
    }

    for (idx, col) in columns.iter().enumerate() {
        let (name, def) = col.iter().next().unwrap();
        let atttypid = map_type_to_oid(&def.col_type);
        let notnull = if def.nullable { "false" } else { "true" };
        let sql = format!(
            "INSERT INTO pg_catalog.pg_attribute \
                 (attrelid,attnum,attname,atttypid,atttypmod,attnotnull,attisdropped) \
                 VALUES ({table_oid},{},'{}',{atttypid},-1,{notnull},false)",
            idx + 1,
            name.replace('\'', "''")
        );
        ctx.sql(&sql).await?.collect().await?;
    }

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

    #[tokio::test]
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
            },
        );
        let mut c2 = BTreeMap::new();
        c2.insert(
            "name".to_string(),
            ColumnDef {
                col_type: "text".to_string(),
                nullable: true,
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

    #[tokio::test]
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
            },
        );
        let mut c2 = BTreeMap::new();
        c2.insert(
            "name".to_string(),
            ColumnDef {
                col_type: "text".to_string(),
                nullable: true,
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

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
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
