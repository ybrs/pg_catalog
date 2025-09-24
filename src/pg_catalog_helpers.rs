use arrow::array::Int64Array;
use datafusion::common::ScalarValue;
use datafusion::error::{DataFusionError, Result as DFResult};
use datafusion::execution::context::SessionContext;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI32, Ordering};

#[derive(Debug, Clone, Deserialize)]
pub struct ColumnDef {
    #[serde(rename = "type")]
    pub col_type: String,
    pub nullable: bool,
}

static NEXT_OID: AtomicI32 = AtomicI32::new(50010);

fn map_type_to_oid(t: &str) -> i32 {
    match t.to_lowercase().as_str() {
        "int" | "integer" | "int4" => 23,
        "bigint" | "int8" => 20,
        "bool" | "boolean" => 16,
        _ => 25, // default to text
    }
}

fn normalize_data_type_name(t: &str) -> (String, String) {
    let lower = t.to_lowercase();
    match lower.as_str() {
        "int" | "integer" | "int4" => ("integer".to_string(), "int4".to_string()),
        "bigint" | "int8" => ("bigint".to_string(), "int8".to_string()),
        "bool" | "boolean" => ("boolean".to_string(), "bool".to_string()),
        s if s.starts_with("varchar") => ("character varying".to_string(), "varchar".to_string()),
        "text" => ("text".to_string(), "text".to_string()),
        _ => (lower.clone(), lower.clone()),
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

    // Also reflect the newly registered table in information_schema.tables.
    // Insert only if a matching row doesn't already exist (idempotent).
    let exists_df = ctx
        .sql(
            "SELECT 1 FROM information_schema.tables \
             WHERE table_catalog=$cat AND table_schema=$sch AND table_name=$tbl",
        )
        .await?
        .with_param_values(vec![
            ("cat", ScalarValue::from(_database_name)),
            ("sch", ScalarValue::from(schema_name)),
            ("tbl", ScalarValue::from(table_name)),
        ])?;
    if exists_df.count().await? == 0 {
        let insert_sql = format!(
            "INSERT INTO information_schema.tables \
             (table_catalog, table_schema, table_name, table_type, \
              self_referencing_column_name, reference_generation, \
              user_defined_type_catalog, user_defined_type_schema, user_defined_type_name, \
              is_insertable_into, is_typed, commit_action) \
             VALUES ('{}','{}','{}','BASE TABLE', \
                     NULL, NULL, \
                     NULL, NULL, NULL, \
                     'YES','NO', NULL)",
            _database_name.replace('\'', "''"),
            schema_name.replace('\'', "''"),
            table_name.replace('\'', "''"),
        );
        ctx.sql(&insert_sql).await?.collect().await?;
    }

    // Insert columns into information_schema.columns
    for (idx, col) in columns.iter().enumerate() {
        let (col_name, def) = col.iter().next().unwrap();
        let (data_type, udt_name) = normalize_data_type_name(&def.col_type);
        let is_nullable = if def.nullable { "YES" } else { "NO" };

        let exists_df = ctx
            .sql(
                "SELECT 1 FROM information_schema.columns \
                 WHERE table_catalog=$cat AND table_schema=$sch AND table_name=$tbl AND column_name=$col",
            )
            .await?
            .with_param_values(vec![
                ("cat", ScalarValue::from(_database_name)),
                ("sch", ScalarValue::from(schema_name)),
                ("tbl", ScalarValue::from(table_name)),
                ("col", ScalarValue::from(col_name.as_str())),
            ])?;
        if exists_df.count().await? == 0 {
            let insert_sql = format!(
                "INSERT INTO information_schema.columns \
                 (table_catalog, table_schema, table_name, column_name, ordinal_position, \
                  column_default, is_nullable, data_type, \
                  character_maximum_length, character_octet_length, numeric_precision, \
                  numeric_precision_radix, numeric_scale, datetime_precision, interval_type, \
                  interval_precision, character_set_catalog, character_set_schema, character_set_name, \
                  collation_catalog, collation_schema, collation_name, domain_catalog, domain_schema, domain_name, \
                  udt_catalog, udt_schema, udt_name, scope_catalog, scope_schema, scope_name, \
                  maximum_cardinality, dtd_identifier, is_self_referencing, is_identity, identity_generation, \
                  identity_start, identity_increment, identity_maximum, identity_minimum, identity_cycle, \
                  is_generated, generation_expression, is_updatable) \
                 VALUES ('{}','{}','{}','{}',{}, \
                         NULL,'{}','{}', \
                         NULL,NULL,NULL, \
                         NULL,NULL,NULL,NULL, \
                         NULL,NULL,NULL,NULL, \
                         NULL,NULL,NULL,NULL,NULL, \
                         NULL,'{}','pg_catalog','{}',NULL,NULL,NULL, \
                         NULL,'{}','NO','NO',NULL, \
                         NULL,NULL,NULL,NULL,'NO', \
                         'NEVER',NULL,'YES')",
                _database_name.replace('\'', "''"),
                schema_name.replace('\'', "''"),
                table_name.replace('\'', "''"),
                col_name.replace('\'', "''"),
                idx + 1,
                is_nullable,
                data_type,
                _database_name.replace('\'', "''"),
                udt_name,
                (idx + 1).to_string(),
            );
            ctx.sql(&insert_sql).await?.collect().await?;
        }
    }

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

    #[tokio::test]
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
}
