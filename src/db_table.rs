// Wrapper around MemTable that records every scan (table, columns, filters) so
// tests can inspect which tables and columns were accessed. Also maps
// PostgreSQL type names to Arrow types.

use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use async_trait::async_trait;
use datafusion::catalog::Session;
use datafusion::datasource::provider::TableProviderFilterPushDown;
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::error::Result;
use datafusion::execution::TaskContext;
use datafusion::logical_expr::dml::InsertOp;
use datafusion::logical_expr::Expr;
use datafusion::physical_plan::empty::EmptyExec;
use datafusion::physical_plan::ExecutionPlan;

use serde_json::json;

use arrow::compute::concat_batches;
use datafusion::execution::context::SessionContext;
use datafusion::physical_plan::collect;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

pub fn map_pg_type(pg_type: &str) -> DataType {
    let lower = pg_type.to_lowercase();
    match lower.as_str() {
        "oidvector" => DataType::List(Arc::new(Field::new("item", DataType::Int64, true))),
        "int2vector" => DataType::List(Arc::new(Field::new("item", DataType::Int32, true))),
        _ if lower.ends_with("[]") || lower.starts_with('_') => {
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true)))
        }
        "uuid" => DataType::Utf8,
        "int" | "integer" | "int4" => DataType::Int32,
        "bigint" | "int8" => DataType::Int64,
        // Floating-point types. Map to the matching Arrow width so the wire type
        // is faithful: real/float4 -> Float32 (advertised as FLOAT4, OID 700);
        // double precision/float8 -> Float64 (FLOAT8, OID 701). Bare `float` is
        // double precision in PostgreSQL. (Before any float arm existed these
        // fell to the Utf8 default and their numeric values silently became NULL,
        // e.g. pg_class.reltuples, pg_stats columns.)
        "real" | "float4" => DataType::Float32,
        "float" | "double" | "double precision" | "float8" => DataType::Float64,
        "bool" | "boolean" => DataType::Boolean,
        "bytea" => DataType::Binary,
        _ if lower.starts_with("varchar") => DataType::Utf8,
        _ => DataType::Utf8,
    }
}

trait SchemaAccess {
    fn schema(&self) -> SchemaRef;
}

impl SchemaAccess for ScanTrace {
    fn schema(&self) -> SchemaRef {
        Arc::new(Schema::new(
            self.column_types
                .iter()
                .map(|(name, pg_type)| Field::new(name, map_pg_type(pg_type), true))
                .collect::<Vec<_>>(),
        ))
    }
}

#[derive(Debug, Clone)]
pub struct ScanTrace {
    table: String,
    projection: Option<Vec<usize>>,
    filters: Vec<Expr>,
    column_types: BTreeMap<String, String>,
}

#[derive(Debug)]
pub struct ScanRecordingMemTable {
    schema: SchemaRef,
    mem: Arc<MemTable>,
    scan_traces: Arc<Mutex<Vec<ScanTrace>>>,
    table_name: String,
}

impl ScanRecordingMemTable {
    pub fn new(
        table_name: String,
        schema: SchemaRef,
        scan_traces: Arc<Mutex<Vec<ScanTrace>>>,
        data: Vec<RecordBatch>,
    ) -> Self {
        let mem = MemTable::try_new(schema.clone(), vec![data]).unwrap();
        Self {
            table_name,
            schema,
            mem: Arc::new(mem),
            scan_traces,
        }
    }
}

#[async_trait]
impl TableProvider for ScanRecordingMemTable {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> Result<Vec<TableProviderFilterPushDown>> {
        Ok(vec![TableProviderFilterPushDown::Inexact; filters.len()])
    }

    async fn scan(
        &self,
        state: &dyn datafusion::catalog::Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        // No special-casing for pg_database; if a lazy source is registered,
        // the provider will be replaced by a LazyCatalogTableProvider.
        let mut column_types = BTreeMap::new();
        for field in self.schema.fields() {
            column_types.insert(field.name().clone(), field.data_type().to_string());
        }

        self.scan_traces.lock().unwrap().push(ScanTrace {
            table: self.table_name.clone(),
            projection: projection.cloned(),
            filters: filters.to_vec(),
            column_types,
        });

        self.mem.scan(state, projection, filters, limit).await
    }

    async fn insert_into(
        &self,
        state: &dyn Session,
        input: Arc<dyn ExecutionPlan>,
        insert_op: InsertOp,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let task_ctx: Arc<TaskContext> =
            if let Some(ctx) = state.as_any().downcast_ref::<SessionContext>() {
                ctx.task_ctx()
            } else {
                Arc::new(TaskContext::from(state))
            };

        let mut new_batches = collect(input, task_ctx).await?;
        let merged = match insert_op {
            InsertOp::Overwrite => concat_batches(&self.schema, &new_batches)?,
            _ => {
                let guard = self.mem.batches[0].write().await;
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

/// Serialize the recorded scan traces to JSON and emit them via `log::info!`,
/// so tests and debugging can see which tables/columns/filters were scanned.
pub fn log_scan_traces(scan_traces: Arc<Mutex<Vec<ScanTrace>>>) {
    let serialized: Vec<_> = scan_traces
        .lock()
        .unwrap()
        .iter()
        .map(|trace| {
            let columns: Vec<_> = match &trace.projection {
                Some(projected) => projected
                    .iter()
                    .map(|i| trace.schema().field(*i).name().clone())
                    .collect(),
                None => trace.column_types.keys().cloned().collect(),
            };
            json!({
                "table": trace.table,
                "columns": columns,
                "filters": trace.filters.iter().map(|f| f.to_string()).collect::<Vec<_>>(),
                "types": trace.column_types,
            })
        })
        .collect();

    log::info!("{}", serde_json::to_string_pretty(&serialized).unwrap());
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_pg_type() {
        assert_eq!(map_pg_type("int"), DataType::Int32);
        assert_eq!(map_pg_type("integer"), DataType::Int32);
        assert_eq!(map_pg_type("bigint"), DataType::Int64);
        assert_eq!(map_pg_type("bool"), DataType::Boolean);
        assert_eq!(map_pg_type("varchar(20)"), DataType::Utf8);
        assert_eq!(map_pg_type("unknown"), DataType::Utf8);
    }

    #[test]
    fn test_map_pg_float_types() {
        // Float types map to the matching Arrow width (faithful wire OID), not
        // the Utf8 default that silently dropped numeric values to NULL.
        // real/float4 -> Float32 (FLOAT4 / OID 700) ...
        for t in ["real", "float4", "REAL", "Float4"] {
            assert_eq!(map_pg_type(t), DataType::Float32, "mapping for {t:?}");
        }
        // ... double precision/float8/bare float -> Float64 (FLOAT8 / OID 701).
        for t in [
            "float",
            "double",
            "double precision",
            "float8",
            "FLOAT8",
            "Double Precision",
        ] {
            assert_eq!(map_pg_type(t), DataType::Float64, "mapping for {t:?}");
        }
    }

    #[test]
    fn test_map_pg_array_type() {
        match map_pg_type("int[]") {
            DataType::List(field) => assert_eq!(field.data_type(), &DataType::Utf8),
            other => panic!("unexpected datatype: {other:?}"),
        }

        match map_pg_type("_text") {
            DataType::List(field) => assert_eq!(field.data_type(), &DataType::Utf8),
            other => panic!("unexpected datatype: {other:?}"),
        }

        match map_pg_type("oidvector") {
            DataType::List(field) => assert_eq!(field.data_type(), &DataType::Int64),
            other => panic!("unexpected datatype: {other:?}"),
        }

        match map_pg_type("int2vector") {
            DataType::List(field) => assert_eq!(field.data_type(), &DataType::Int32),
            other => panic!("unexpected datatype: {other:?}"),
        }
    }
}
