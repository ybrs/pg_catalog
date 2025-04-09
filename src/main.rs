use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::datasource::provider::TableProviderFilterPushDown;
use datafusion::execution::context::{SessionContext};
use datafusion::logical_expr::Expr;
use datafusion::physical_plan::ExecutionPlan;
use datafusion::error::Result;
use async_trait::async_trait;

use serde::Deserialize;
use serde_json::json;
use serde_yaml;

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::sync::{Arc, Mutex};

#[derive(Debug, Deserialize)]
struct YamlSchema(HashMap<String, BTreeMap<String, String>>);

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

fn parse_schema(yaml_path: &str) -> HashMap<String, SchemaRef> {
    let contents = fs::read_to_string(yaml_path).expect("Failed to read schema.yaml");
    let parsed: YamlSchema = serde_yaml::from_str(&contents).expect("Invalid YAML schema");

    parsed
        .0
        .into_iter()
        .map(|(table, columns)| {
            let fields: Vec<Field> = columns
                .into_iter()
                .map(|(col, typ)| Field::new(&col, map_pg_type(&typ), true))
                .collect();
            (table, Arc::new(Schema::new(fields)))
        })
        .collect()
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
    name: String,
    schema: SchemaRef,
    mem: Arc<MemTable>,
    log: Arc<Mutex<Vec<ScanTrace>>>,
}

impl ObservableMemTable {
    fn new(name: String, schema: SchemaRef, log: Arc<Mutex<Vec<ScanTrace>>>) -> Self {
        let mem = MemTable::try_new(schema.clone(), vec![]).unwrap();
        Self {
            name,
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
            table: self.name.clone(),
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
        eprintln!("Usage: {} schema.yaml \"SQL_QUERY\"", args[0]);
        std::process::exit(1);
    }

    let schema_path = &args[1];
    let sql = &args[2];

    let schemas = parse_schema(schema_path);
    let log = Arc::new(Mutex::new(Vec::new()));
    let ctx = SessionContext::new();

    for (table, schema) in &schemas {
        let wrapped = ObservableMemTable::new(table.clone(), schema.clone(), log.clone());
        ctx.register_table(table, Arc::new(wrapped))?;
    }

    let df = ctx.sql(sql).await?;
    let _ = df.collect().await?;

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
