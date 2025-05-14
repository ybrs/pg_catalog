use arrow::array::{Int32Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};

use datafusion::catalog::{CatalogProvider, Session};
use datafusion::catalog::SchemaProvider;

use datafusion::catalog::memory::{MemoryCatalogProvider, MemorySchemaProvider};
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::datasource::provider::TableProviderFilterPushDown;
use datafusion::execution::context::SessionContext;
use datafusion::execution::TaskContext;
use datafusion::logical_expr::dml::InsertOp;
use datafusion::logical_expr::Expr;
use datafusion::physical_plan::empty::EmptyExec;
use datafusion::physical_plan::ExecutionPlan;
use datafusion::error::Result;
use async_trait::async_trait;
use arrow::record_batch::RecordBatch;

use serde::Deserialize;
use serde_json::json;
use serde_yaml;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::ops::Deref;
use std::path::Path;
use std::sync::{Arc, Mutex};
use pgwire::api::Type;
use crate::clean_duplicate_columns::alias_all_columns;
use crate::replace::{regclass_udfs, replace_regclass, replace_set_command_with_namespace};
use bytes::Bytes;

use datafusion::scalar::ScalarValue;
use crate::user_functions::{register_scalar_format_type, register_scalar_pg_tablespace_location, register_scalar_regclass_oid};
use datafusion::common::{config_err, config::ConfigEntry};

use datafusion::common::config::{ConfigExtension, ExtensionOptions};
use arrow::compute::concat_batches;
use datafusion::physical_plan::collect;



pub fn map_pg_type(pg_type: &str) -> DataType {
    match pg_type.to_lowercase().as_str() {
        "uuid" => DataType::Utf8,
        "int" | "integer" => DataType::Int32,
        "bigint" => DataType::Int64,
        "bool" | "boolean" => DataType::Boolean,
        _ if pg_type.to_lowercase().starts_with("varchar") => DataType::Utf8,
        _ => DataType::Utf8,
    }
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



#[derive(Debug, Clone)]
pub struct ScanTrace {
    table: String,
    projection: Option<Vec<usize>>,
    filters: Vec<Expr>,
    types: BTreeMap<String, String>,
}

#[derive(Debug)]
pub struct ObservableMemTable {
    schema: SchemaRef,
    mem: Arc<MemTable>,
    log: Arc<Mutex<Vec<ScanTrace>>>,
    table_name: String,
}

impl ObservableMemTable {
    pub fn new(table_name: String, schema: SchemaRef, log: Arc<Mutex<Vec<ScanTrace>>>, data: Vec<RecordBatch>) -> Self {
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

    async fn insert_into(
        &self,
        state: &dyn Session,
        input: Arc<dyn ExecutionPlan>,
        insert_op: InsertOp,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let task_ctx: Arc<TaskContext> = if let Some(ctx) = state.as_any().downcast_ref::<SessionContext>() {
            ctx.task_ctx()
        } else {
            Arc::new(TaskContext::from(state))
        };

        let mut new_batches = collect(input, task_ctx).await?;
        let merged = match insert_op {
            InsertOp::Overwrite => concat_batches(&self.schema, &new_batches)?,
            _ => {
                let mut guard = self.mem.batches[0].write().await;
                if !guard.is_empty() {
                    let mut all = vec![guard[0].clone()];
                    all.append(&mut new_batches);
                    concat_batches(&self.schema, &all)?
                } else {
                    concat_batches(&self.schema, &new_batches)?
                }
            }
        };

        {
            let mut guard = self.mem.batches[0].write().await;
            guard.clear();
            guard.push(merged);
        }

        Ok(Arc::new(EmptyExec::new(self.schema.clone())))
    }

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