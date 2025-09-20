use arrow::array::{Int32Array, Int64Array, StringArray};
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

static SCHEMA_TABLES: Lazy<Mutex<HashMap<(String, String), HashSet<String>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn track_schema(database_name: &str, schema_name: &str) {
    let mut map = DATABASE_SCHEMAS.lock().unwrap();
    map.entry(database_name.to_string())
        .or_default()
        .insert(schema_name.to_string());
}

fn untrack_schema(database_name: &str, schema_name: &str) {
    let mut map = DATABASE_SCHEMAS.lock().unwrap();
    if let Some(schemas) = map.get_mut(database_name) {
        schemas.remove(schema_name);
        if schemas.is_empty() {
            map.remove(database_name);
        }
    }
}

fn tracked_schemas(database_name: &str) -> HashSet<String> {
    let map = DATABASE_SCHEMAS.lock().unwrap();
    map.get(database_name).cloned().unwrap_or_default()
}

fn track_table(database_name: &str, schema_name: &str, table_name: &str) {
    let mut map = SCHEMA_TABLES.lock().unwrap();
    map.entry((database_name.to_string(), schema_name.to_string()))
        .or_default()
        .insert(table_name.to_string());
}

fn untrack_table(database_name: &str, schema_name: &str, table_name: &str) {
    let key = (database_name.to_string(), schema_name.to_string());
    let mut map = SCHEMA_TABLES.lock().unwrap();
    if let Some(tables) = map.get_mut(&key) {
        tables.remove(table_name);
        if tables.is_empty() {
            map.remove(&key);
        }
    }
}

fn tracked_tables(database_name: &str, schema_name: &str) -> HashSet<String> {
    let key = (database_name.to_string(), schema_name.to_string());
    let map = SCHEMA_TABLES.lock().unwrap();
    map.get(&key).cloned().unwrap_or_default()
}

fn schema_names_from_table_map(database_name: &str) -> HashSet<String> {
    let map = SCHEMA_TABLES.lock().unwrap();
    map.keys()
        .filter(|(db, _)| db == database_name)
        .map(|(_, schema)| schema.clone())
        .collect()
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
    _database_name: &str,
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

    track_schema(_database_name, schema_name);

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
        track_table(_database_name, schema_name, table_name);
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

    track_table(_database_name, schema_name, table_name);

    Ok(())
}

pub async fn unregister_tables(
    ctx: &SessionContext,
    database_name: &str,
    schema_name: &str,
    table_name: &str,
) -> DFResult<()> {
    let df = ctx
        .sql(
            "SELECT c.oid, c.reltype \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON c.relnamespace = n.oid \
             WHERE c.relname=$table AND n.nspname=$schema",
        )
        .await?
        .with_param_values(vec![
            ("table", ScalarValue::from(table_name)),
            ("schema", ScalarValue::from(schema_name)),
        ])?;

    let batches = df.collect().await?;
    for batch in &batches {
        let oid_array = batch.column(0);
        let type_array = batch.column(1);
        for row in 0..batch.num_rows() {
            let table_oid = if let Some(arr) = oid_array.as_any().downcast_ref::<Int32Array>() {
                arr.value(row) as i64
            } else if let Some(arr) = oid_array.as_any().downcast_ref::<Int64Array>() {
                arr.value(row)
            } else {
                return Err(DataFusionError::Execution(
                    "unexpected data type for pg_class.oid".to_string(),
                ));
            };

            let type_oid = if let Some(arr) = type_array.as_any().downcast_ref::<Int32Array>() {
                arr.value(row) as i64
            } else if let Some(arr) = type_array.as_any().downcast_ref::<Int64Array>() {
                arr.value(row)
            } else {
                return Err(DataFusionError::Execution(
                    "unexpected data type for pg_class.reltype".to_string(),
                ));
            };

            ctx.sql(&format!(
                "DELETE FROM pg_catalog.pg_attribute WHERE attrelid = {table_oid}"
            ))
            .await?
            .collect()
            .await?;

            ctx.sql(&format!(
                "DELETE FROM pg_catalog.pg_type WHERE oid = {type_oid} OR typrelid = {table_oid}"
            ))
            .await?
            .collect()
            .await?;

            ctx.sql(&format!(
                "DELETE FROM pg_catalog.pg_class WHERE oid = {table_oid}"
            ))
            .await?
            .collect()
            .await?;
        }
    }

    untrack_table(database_name, schema_name, table_name);

    Ok(())
}

pub async fn unregister_schema(
    ctx: &SessionContext,
    database_name: &str,
    schema_name: &str,
) -> DFResult<()> {
    let mut table_names = tracked_tables(database_name, schema_name);

    let df = ctx
        .sql(
            "SELECT c.relname \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON c.relnamespace = n.oid \
             WHERE n.nspname=$schema",
        )
        .await?
        .with_param_values(vec![("schema", ScalarValue::from(schema_name))])?;
    let batches = df.collect().await?;
    for batch in &batches {
        let relname_array = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| {
                DataFusionError::Execution("unexpected data type for pg_class.relname".to_string())
            })?;
        for row in 0..batch.num_rows() {
            table_names.insert(relname_array.value(row).to_string());
        }
    }

    for table_name in table_names {
        unregister_tables(ctx, database_name, schema_name, &table_name).await?;
    }

    let escaped_schema = schema_name.replace('\'', "''");
    ctx.sql(&format!(
        "DELETE FROM pg_catalog.pg_namespace WHERE nspname = '{escaped_schema}'"
    ))
    .await?
    .collect()
    .await?;

    untrack_schema(database_name, schema_name);

    Ok(())
}

pub async fn unregister_database(ctx: &SessionContext, database_name: &str) -> DFResult<()> {
    let mut schemas = tracked_schemas(database_name);
    schemas.extend(schema_names_from_table_map(database_name));

    for schema_name in schemas {
        unregister_schema(ctx, database_name, &schema_name).await?;
    }

    let escaped_database = database_name.replace('\'', "''");
    ctx.sql(&format!(
        "DELETE FROM pg_catalog.pg_database WHERE datname = '{escaped_database}'"
    ))
    .await?
    .collect()
    .await?;

    {
        let mut map = DATABASE_SCHEMAS.lock().unwrap();
        map.remove(database_name);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::get_base_session_context;

    async fn fetch_table_identifiers(
        ctx: &SessionContext,
        schema: &str,
        table: &str,
    ) -> DFResult<Option<(i64, i64)>> {
        let df = ctx
            .sql(
                "SELECT c.oid, c.reltype \
                 FROM pg_catalog.pg_class c \
                 JOIN pg_catalog.pg_namespace n ON c.relnamespace = n.oid \
                 WHERE c.relname=$table AND n.nspname=$schema",
            )
            .await?
            .with_param_values(vec![
                ("table", ScalarValue::from(table)),
                ("schema", ScalarValue::from(schema)),
            ])?;
        let batches = df.collect().await?;
        if batches.is_empty() || batches[0].num_rows() == 0 {
            return Ok(None);
        }
        let batch = &batches[0];
        let table_oid = if let Some(arr) = batch.column(0).as_any().downcast_ref::<Int32Array>() {
            arr.value(0) as i64
        } else if let Some(arr) = batch.column(0).as_any().downcast_ref::<Int64Array>() {
            arr.value(0)
        } else {
            return Err(DataFusionError::Execution(
                "unexpected data type for pg_class.oid".to_string(),
            ));
        };
        let type_oid = if let Some(arr) = batch.column(1).as_any().downcast_ref::<Int32Array>() {
            arr.value(0) as i64
        } else if let Some(arr) = batch.column(1).as_any().downcast_ref::<Int64Array>() {
            arr.value(0)
        } else {
            return Err(DataFusionError::Execution(
                "unexpected data type for pg_class.reltype".to_string(),
            ));
        };
        Ok(Some((table_oid, type_oid)))
    }

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

        let schema_name = "unregister_tables_schema";
        let table_name = "unregister_tables";
        register_schema(&ctx, "pgtry", schema_name).await?;

        let mut c1 = BTreeMap::new();
        c1.insert(
            "id".to_string(),
            ColumnDef {
                col_type: "int".to_string(),
                nullable: false,
            },
        );

        register_user_tables(&ctx, "pgtry", schema_name, table_name, vec![c1]).await?;
        let (table_oid, type_oid) = fetch_table_identifiers(&ctx, schema_name, table_name)
            .await?
            .expect("table metadata");

        unregister_tables(&ctx, "pgtry", schema_name, table_name).await?;

        let df = ctx
            .sql(&format!(
                "SELECT 1 FROM pg_catalog.pg_class WHERE oid = {table_oid}"
            ))
            .await?;
        assert_eq!(df.count().await?, 0);

        let df = ctx
            .sql(&format!(
                "SELECT 1 FROM pg_catalog.pg_type WHERE oid = {type_oid} OR typrelid = {table_oid}"
            ))
            .await?;
        assert_eq!(df.count().await?, 0);

        let df = ctx
            .sql(&format!(
                "SELECT 1 FROM pg_catalog.pg_attribute WHERE attrelid = {table_oid}"
            ))
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

        let schema_name = "unregister_schema";
        register_schema(&ctx, "pgtry", schema_name).await?;

        let mut c1 = BTreeMap::new();
        c1.insert(
            "id".to_string(),
            ColumnDef {
                col_type: "int".to_string(),
                nullable: false,
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

        register_user_tables(&ctx, "pgtry", schema_name, "table_one", vec![c1.clone()]).await?;
        register_user_tables(&ctx, "pgtry", schema_name, "table_two", vec![c2.clone()]).await?;

        let (table_one_oid, table_one_type) =
            fetch_table_identifiers(&ctx, schema_name, "table_one")
                .await?
                .expect("table one metadata");
        let (table_two_oid, table_two_type) =
            fetch_table_identifiers(&ctx, schema_name, "table_two")
                .await?
                .expect("table two metadata");

        unregister_schema(&ctx, "pgtry", schema_name).await?;

        let df = ctx
            .sql(&format!(
                "SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = '{schema_name}'"
            ))
            .await?;
        assert_eq!(df.count().await?, 0);

        for (oid, type_oid) in [
            (table_one_oid, table_one_type),
            (table_two_oid, table_two_type),
        ] {
            let df = ctx
                .sql(&format!(
                    "SELECT 1 FROM pg_catalog.pg_class WHERE oid = {oid}"
                ))
                .await?;
            assert_eq!(df.count().await?, 0);

            let df = ctx
                .sql(&format!(
                    "SELECT 1 FROM pg_catalog.pg_type WHERE oid = {type_oid} OR typrelid = {oid}"
                ))
                .await?;
            assert_eq!(df.count().await?, 0);

            let df = ctx
                .sql(&format!(
                    "SELECT 1 FROM pg_catalog.pg_attribute WHERE attrelid = {oid}"
                ))
                .await?;
            assert_eq!(df.count().await?, 0);
        }

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

        let database_name = "crm_unregister";
        let schema_name = "crm_unregister_schema";
        let table_name = "crm_unregister_table";

        register_user_database(&ctx, database_name).await?;
        register_schema(&ctx, database_name, schema_name).await?;

        let mut c1 = BTreeMap::new();
        c1.insert(
            "id".to_string(),
            ColumnDef {
                col_type: "int".to_string(),
                nullable: false,
            },
        );

        register_user_tables(&ctx, database_name, schema_name, table_name, vec![c1]).await?;

        let (table_oid, type_oid) = fetch_table_identifiers(&ctx, schema_name, table_name)
            .await?
            .expect("table metadata");

        unregister_database(&ctx, database_name).await?;

        let df = ctx
            .sql(&format!(
                "SELECT 1 FROM pg_catalog.pg_database WHERE datname = '{database_name}'"
            ))
            .await?;
        assert_eq!(df.count().await?, 0);

        let df = ctx
            .sql(&format!(
                "SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = '{schema_name}'"
            ))
            .await?;
        assert_eq!(df.count().await?, 0);

        let df = ctx
            .sql(&format!(
                "SELECT 1 FROM pg_catalog.pg_class WHERE oid = {table_oid}"
            ))
            .await?;
        assert_eq!(df.count().await?, 0);

        let df = ctx
            .sql(&format!(
                "SELECT 1 FROM pg_catalog.pg_type WHERE oid = {type_oid} OR typrelid = {table_oid}"
            ))
            .await?;
        assert_eq!(df.count().await?, 0);

        let df = ctx
            .sql(&format!(
                "SELECT 1 FROM pg_catalog.pg_attribute WHERE attrelid = {table_oid}"
            ))
            .await?;
        assert_eq!(df.count().await?, 0);

        Ok(())
    }
}
