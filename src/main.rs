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
// use datafusion::common::DataFusionError;
use arrow::record_batch::RecordBatch;
use datafusion::arrow::util::pretty;

use serde::Deserialize;
use serde_json::json;
use serde_yaml;

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::sync::{Arc, Mutex};

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

// fn parse_schema(yaml_path: &str) -> HashMap<String, HashMap<String, HashMap<String, SchemaRef>>> {
//     let contents = fs::read_to_string(yaml_path).expect("Failed to read schema.yaml");
//     let parsed: YamlSchema = serde_yaml::from_str(&contents).expect("Invalid YAML schema");
//
//     parsed.0
//         .into_iter()
//         .map(|(catalog_name, schemas)| {
//             let schemas = schemas
//                 .into_iter()
//                 .map(|(schema_name, tables)| {
//                     let tables = tables
//                         .into_iter()
//                         .map(|(table_name, columns)| {
//                             let fields: Vec<Field> = columns
//                                 .into_iter()
//                                 .map(|(col, typ)| Field::new(&col, map_pg_type(&typ), true))
//                                 .collect();
//                             (table_name, Arc::new(Schema::new(fields)))
//                         })
//                         .collect();
//                     (schema_name, tables)
//                 })
//                 .collect();
//             (catalog_name, schemas)
//         })
//         .collect()
// }

fn parse_schema(yaml_path: &str) -> HashMap<String, HashMap<String, HashMap<String, (SchemaRef, Vec<RecordBatch>)>>> {
    let contents = fs::read_to_string(yaml_path).expect("Failed to read schema.yaml");
    let parsed: YamlSchema = serde_yaml::from_str(&contents).expect("Invalid YAML schema");

    parsed.0.into_iter().map(|(catalog_name, schemas)| {
        let schemas = schemas.into_iter().map(|(schema_name, tables)| {
            let tables = tables.into_iter().map(|(table_name, def)| {
                let fields: Vec<Field> = def.schema.iter()
                    .map(|(col, typ)| Field::new(col, map_pg_type(typ), true))
                    .collect();

                let schema_ref = Arc::new(Schema::new(fields.clone()));

                let record_batch = if let Some(rows) = def.rows {
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

                (table_name, (schema_ref, record_batch))
            }).collect();
            (schema_name, tables)
        }).collect();
        (catalog_name, schemas)
    }).collect()
}

#[derive(Debug, Clone)]
struct ScanTrace {
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

#[tokio::main]
async fn main() -> datafusion::error::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        println!("Usage: {} schema.yaml \"SQL_QUERY\"", args[0]);
        std::process::exit(1);
    }

    let schema_path = &args[1];
    let sql = &args[2];
    let log = Arc::new(Mutex::new(Vec::new()));


    let default_catalog = args.iter()
        .position(|x| x == "--default-catalog")
        .and_then(|i| args.get(i + 1))
        .unwrap_or(&"datafusion".to_string())
        .clone();

    let default_schema = args.iter()
        .position(|x| x == "--default-schema")
        .and_then(|i| args.get(i + 1))
        .unwrap_or(&"public".to_string())
        .clone();

    let config = datafusion::execution::context::SessionConfig::new()
        .with_default_catalog_and_schema(&default_catalog, &default_schema);

    let ctx = SessionContext::new_with_config(config);

    let schemas = parse_schema(schema_path);

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

    let df = ctx.sql(sql).await?;
    let results = df.collect().await?;
    pretty::print_batches(&results)?;

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

    Ok(())
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