use arrow::datatypes::{DataType, Field, Schema, SchemaRef};

use datafusion::catalog::CatalogProvider;
use datafusion::catalog::SchemaProvider;

use datafusion::catalog::memory::{MemoryCatalogProvider, MemorySchemaProvider};
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::datasource::provider::TableProviderFilterPushDown;
use datafusion::execution::context::SessionContext;
use datafusion::logical_expr::Expr;
use datafusion::physical_plan::ExecutionPlan;
use datafusion::error::Result;
use async_trait::async_trait;
use arrow::record_batch::RecordBatch;

use serde::Deserialize;
use serde_json::json;
use serde_yaml;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use pgwire::api::Type;
use crate::clean_duplicate_columns::alias_all_columns;
use crate::replace::{regclass_udfs, replace_regclass};
use bytes::Bytes;

use datafusion::scalar::ScalarValue;
use crate::user_functions::{register_scalar_pg_tablespace_location, register_scalar_regclass_oid};
use datafusion::common::{config_err, config::ConfigEntry};

use datafusion::common::config::{ConfigExtension, ExtensionOptions};

#[derive(Default, Clone, Debug)]
pub struct ClientOpts {
    pub application_name: String,
}


impl ConfigExtension for ClientOpts {
    const PREFIX: &'static str = "client";
}

impl ExtensionOptions for ClientOpts {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }
    fn cloned(&self) -> Box<dyn ExtensionOptions> { Box::new(self.clone()) }

    fn set(&mut self, key: &str, value: &str) -> datafusion::error::Result<()> {
        println!("set key {:?}", key);
        match key {
            "application_name" => {
                self.application_name = value.to_string();
                Ok(())
            }
            _ => config_err!("unknown key {key}"),
        }
    }

    fn entries(&self) -> Vec<ConfigEntry> {
        vec![ConfigEntry {
            // key: format!("{}.application_name", Self::PREFIX),
            key: "application_name".to_string(),
            value: Some(self.application_name.clone()),
            description: "",
        }]
    }

}


#[derive(Debug, Deserialize)]
struct TableDef {
    schema: BTreeMap<String, String>,
    rows: Option<Vec<BTreeMap<String, serde_json::Value>>>,
}

#[derive(Debug, Deserialize)]
struct YamlSchema(HashMap<String, HashMap<String, HashMap<String, TableDef>>>);


fn map_pg_type(pg_type: &str) -> DataType {
    match pg_type.to_lowercase().as_str() {
        "uuid" => DataType::Utf8,
        "int" | "integer" => DataType::Int32,
        "bigint" => DataType::Int64,
        "bool" | "boolean" => DataType::Boolean,
        _ if pg_type.to_lowercase().starts_with("varchar") => DataType::Utf8,
        _ => DataType::Utf8,
    }
}

#[derive(Debug, Clone)]
pub struct ScanTrace {
    table: String,
    projection: Option<Vec<usize>>,
    filters: Vec<Expr>,
    types: BTreeMap<String, String>,
}

#[derive(Debug)]
struct ObservableMemTable {
    schema: SchemaRef,
    mem: Arc<MemTable>,
    log: Arc<Mutex<Vec<ScanTrace>>>,
    table_name: String,
}

impl ObservableMemTable {
    fn new(table_name: String, schema: SchemaRef, log: Arc<Mutex<Vec<ScanTrace>>>, data: Vec<RecordBatch>) -> Self {
        let mem = MemTable::try_new(schema.clone(), vec![data]).unwrap();
        Self {
            table_name,
            schema,
            mem: Arc::new(mem),
            log,
        }
    }
}

#[async_trait]
impl TableProvider for ObservableMemTable {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    fn supports_filters_pushdown(&self, filters: &[&Expr]) -> Result<Vec<TableProviderFilterPushDown>> {
        Ok(vec![TableProviderFilterPushDown::Inexact; filters.len()])
    }

    async fn scan(
        &self,
        state: &dyn datafusion::catalog::Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let mut types = BTreeMap::new();
        for f in self.schema.fields() {
            types.insert(f.name().clone(), f.data_type().to_string());
        }

        self.log.lock().unwrap().push(ScanTrace {
            table: self.table_name.clone(),
            projection: projection.cloned(),
            filters: filters.to_vec(),
            types,
        });

        self.mem.scan(state, projection, filters, limit).await
    }
}





fn rename_columns(batch: &RecordBatch, name_map: &HashMap<String, String>) -> RecordBatch {
    let new_fields = batch
        .schema()
        .fields()
        .iter()
        .map(|old_field| {
            let new_name = name_map
                .get(old_field.name())
                .map(|s| s.as_str())
                .unwrap_or_else(|| old_field.name().as_str());
            Field::new(new_name, old_field.data_type().clone(), old_field.is_nullable())
        })
        .collect::<Vec<_>>();

    let new_schema = std::sync::Arc::new(Schema::new(new_fields));
    RecordBatch::try_new(new_schema, batch.columns().to_vec()).unwrap()
}

trait SchemaAccess {
    fn schema(&self) -> SchemaRef;
}

impl SchemaAccess for ScanTrace {
    fn schema(&self) -> SchemaRef {
        Arc::new(Schema::new(
            self.types
                .iter()
                .map(|(k, v)| Field::new(k, map_pg_type(v), true))
                .collect::<Vec<_>>(),
        ))
    }
}



pub async fn execute_sql(
    ctx: &SessionContext,
    sql: &str,
    vec: Option<Vec<Option<Bytes>>>,
    vec0: Option<Vec<Type>>,
) -> datafusion::error::Result<(Vec<RecordBatch>, Arc<Schema>)> {
    let sql = replace_regclass(sql);
    let (sql, aliases) = alias_all_columns(&sql);

    let df = ctx.sql(&sql).await?;

    if let (Some(params), Some(types)) = (vec, vec0) {

        let mut scalars = Vec::new();

        for (param, typ) in params.into_iter().zip(types.into_iter()) {
            let value = match (param, typ) {
                (Some(bytes), Type::INT2) => {
                    let v = i16::from_be_bytes(bytes[..].try_into().unwrap());
                    ScalarValue::Int16(Some(v))
                }
                (Some(bytes), Type::INT8) => {
                    let v = i64::from_be_bytes(bytes[..].try_into().unwrap());
                    ScalarValue::Int64(Some(v))
                }
                (Some(bytes), Type::INT4) => {
                    let v = i32::from_be_bytes(bytes[..].try_into().unwrap());
                    ScalarValue::Int32(Some(v))
                }
                (Some(bytes), Type::VARCHAR) => {
                    let s = String::from_utf8(bytes.to_vec()).unwrap();
                    ScalarValue::Utf8(Some(s))
                }
                (None, Type::INT2) => ScalarValue::Int16(None),
                (None, Type::INT8) => ScalarValue::Int64(None),
                (None, Type::INT4) => ScalarValue::Int32(None),
                (None, Type::VARCHAR) => ScalarValue::Utf8(None),
                (some, other_type) => {
                    panic!("unsupported param {:?} type {:?}", some, other_type);
                }
            };
            scalars.push(value);
        }

    }


    let original_schema = df.schema();
    let renamed_fields = original_schema
        .fields()
        .iter()
        .map(|f| {
            let new_name = aliases
                .get(f.name())
                .map(|s| s.as_str())
                .unwrap_or_else(|| f.name().as_str());
            Field::new(new_name, f.data_type().clone(), f.is_nullable())
        })
        .collect::<Vec<_>>();
    let schema = Arc::new(Schema::new(renamed_fields));

    let results = df.collect().await?;
    let results = results
        .iter()
        .map(|batch| rename_columns(batch, &aliases))
        .collect();

    Ok((results, schema))


}


fn parse_schema(schema_path: &str) -> HashMap<String, HashMap<String, HashMap<String, (SchemaRef, Vec<RecordBatch>)>>> {
    if Path::new(schema_path).is_file() {
        parse_schema_file(schema_path)
    } else if Path::new(schema_path).is_dir() {
        parse_schema_dir(schema_path)
    } else {
        panic!("schema_path {} is neither a file nor a directory", schema_path);
    }
}

fn parse_schema_file(path: &str) -> HashMap<String, HashMap<String, HashMap<String, (SchemaRef, Vec<RecordBatch>)>>> {
    let contents = fs::read_to_string(path).expect("Failed to read schema file");
    let parsed: YamlSchema = serde_yaml::from_str(&contents).expect("Invalid YAML");
    parsed.0.into_iter()
        .map(|(catalog, schemas)| {
            let schemas = schemas.into_iter().map(|(schema, tables)| {
                let tables = tables.into_iter().map(|(table, def)| {
                    let (schema_ref, batches) = build_table(def);
                    (table, (schema_ref, batches))
                }).collect();
                (schema, tables)
            }).collect();
            (catalog, schemas)
        }).collect()
}

fn parse_schema_dir(
    dir_path: &str,
) -> HashMap<String, HashMap<String, HashMap<String, (SchemaRef, Vec<RecordBatch>)>>> {
    let mut all = HashMap::new();

    for entry in fs::read_dir(dir_path).expect("Failed to read directory") {
        let path = entry.expect("Invalid dir entry").path();
        if path.extension().and_then(|s| s.to_str()) == Some("yaml") {
            let mut partial = parse_schema_file(path.to_str().unwrap());

            for schemas in partial.values_mut() {
                schemas.retain(|name, _| name != "information_schema");
            }

            merge_schema_maps(&mut all, partial);
        }
    }

    all
}

fn merge_schema_maps(
    target: &mut HashMap<String, HashMap<String, HashMap<String, (SchemaRef, Vec<RecordBatch>)>>>,
    other: HashMap<String, HashMap<String, HashMap<String, (SchemaRef, Vec<RecordBatch>)>>>,
) {
    for (catalog, schemas) in other {
        let catalog_entry = target.entry(catalog).or_insert_with(HashMap::new);
        for (schema, tables) in schemas {
            let schema_entry = catalog_entry.entry(schema).or_insert_with(HashMap::new);
            schema_entry.extend(tables);
        }
    }
}

fn build_table(def: TableDef) -> (SchemaRef, Vec<RecordBatch>) {
    let fields: Vec<Field> = def.schema.iter()
        .map(|(col, typ)| Field::new(col, map_pg_type(typ), true))
        .collect();

    let schema_ref = Arc::new(Schema::new(fields.clone()));

    let record_batches = if let Some(rows) = def.rows {
        let mut cols: Vec<Vec<serde_json::Value>> = vec![vec![]; fields.len()];
        for row in rows {
            for (i, field) in fields.iter().enumerate() {
                cols[i].push(row.get(field.name()).cloned().unwrap_or(serde_json::Value::Null));
            }
        }

        let arrays = fields.iter().zip(cols.into_iter())
            .map(|(field, col_data)| {
                use arrow::array::*;
                use arrow::datatypes::DataType;
                let array: ArrayRef = match field.data_type() {
                    DataType::Utf8 => Arc::new(StringArray::from(
                        col_data.into_iter()
                            .map(|v| v.as_str().map(|s| s.to_string()))
                            .collect::<Vec<_>>()
                    )),
                    DataType::Int32 => Arc::new(Int32Array::from(
                        col_data.into_iter()
                            .map(|v| v.as_i64().map(|i| i as i32))
                            .collect::<Vec<_>>()
                    )),
                    DataType::Int64 => Arc::new(Int64Array::from(
                        col_data.into_iter()
                            .map(|v| v.as_i64())
                            .collect::<Vec<_>>()
                    )),
                    DataType::Boolean => Arc::new(BooleanArray::from(
                        col_data.into_iter()
                            .map(|v| v.as_bool())
                            .collect::<Vec<_>>()
                    )),
                    _ => Arc::new(StringArray::from(
                        col_data.into_iter()
                            .map(|v| Some(v.to_string()))
                            .collect::<Vec<_>>()
                    )),
                };
                array
            })
            .collect::<Vec<_>>();

        vec![RecordBatch::try_new(schema_ref.clone(), arrays).unwrap()]
    } else {
        vec![RecordBatch::new_empty(schema_ref.clone())]
    };

    (schema_ref, record_batches)
}

pub fn get_session_context(schema_path: &String, default_catalog:String, default_schema:String) -> datafusion::error::Result<(SessionContext, Arc<Mutex<Vec<ScanTrace>>>)> {
    let log: Arc<Mutex<Vec<ScanTrace>>> = Arc::new(Mutex::new(Vec::new()));

    let schemas = parse_schema(schema_path.as_str());

    let mut config = datafusion::execution::context::SessionConfig::new()
        .with_default_catalog_and_schema(&default_catalog, &default_schema)
        .with_option_extension(ClientOpts::default());

    config.options_mut().catalog.information_schema = true;

    let ctx = SessionContext::new_with_config(config);

    for (catalog_name, schemas) in schemas {
        let catalog_provider = Arc::new(MemoryCatalogProvider::new());
        ctx.register_catalog(&catalog_name, catalog_provider.clone());

        for (schema_name, tables) in schemas {
            let schema_provider = Arc::new(MemorySchemaProvider::new());
            let _ = catalog_provider.register_schema(&schema_name, schema_provider.clone());

            for (table, (schema_ref, batches)) in tables {
                let wrapped = ObservableMemTable::new(table.clone(), schema_ref, log.clone(), batches);
                schema_provider.register_table(table, Arc::new(wrapped))?;
            }
        }
    }


    for f in regclass_udfs(&ctx) {
        ctx.register_udf(f);
    }

    ctx.register_udtf("regclass_oid", Arc::new(crate::user_functions::RegClassOidFunc));

    register_scalar_regclass_oid(&ctx)?;
    register_scalar_pg_tablespace_location(&ctx)?;

    Ok((ctx, log))
}

pub fn print_execution_log(log:Arc<Mutex<Vec<ScanTrace>>>){
    let out: Vec<_> = log.lock().unwrap().iter().map(|entry| {
        let columns: Vec<_> = match &entry.projection {
            Some(p) => p.iter().map(|i| entry.schema().field(*i).name().clone()).collect(),
            None => entry.types.keys().cloned().collect(),
        };
        json!({
            "table": entry.table,
            "columns": columns,
            "filters": entry.filters.iter().map(|f| f.to_string()).collect::<Vec<_>>(),
            "types": entry.types,
        })
    }).collect();

    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}