// Wrapper around MemTable that records query scans.
// Also includes helpers for mapping PostgreSQL types and printing execution logs.
// Allows tests to inspect which tables and columns were accessed.

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
use datafusion::physical_plan::collect;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use crate::lazy_pg_catalog_helpers::current_database_rows;
use datafusion::execution::context::SessionContext;

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
    pub fn new(
        table_name: String,
        schema: SchemaRef,
        log: Arc<Mutex<Vec<ScanTrace>>>,
        data: Vec<RecordBatch>,
    ) -> Self {
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
        // If scanning pg_database and a lazy fetcher is registered, append
        // rows for fetched databases with default/null values where needed.
        if self.table_name == "pg_database" {
            if let Some(rows) = current_database_rows() {
                if !rows.is_empty() {
                    // Build a batch in-memory for immediate visibility, and also
                    // trigger SQL inserts for persistence.
                    use arrow::array::{new_null_array, ArrayRef, BooleanArray, BooleanBuilder, Int32Array, Int32Builder, StringArray, StringBuilder};

                    let mut oid_b = Int32Builder::new();
                    let mut datname_b = StringBuilder::new();
                    let mut datdba_b = Int32Builder::new();
                    let mut encoding_b = Int32Builder::new();
                    let mut datlocprovider_b = StringBuilder::new();
                    let mut datistemplate_b = BooleanBuilder::new();
                    let mut datallowconn_b = BooleanBuilder::new();
                    let mut dathasloginevt_b = BooleanBuilder::new();
                    let mut datconnlimit_b = Int32Builder::new();
                    let mut datfrozenxid_b = StringBuilder::new();
                    let mut datminmxid_b = StringBuilder::new();
                    let mut dattablespace_b = Int32Builder::new();
                    let mut datcollate_b = StringBuilder::new();
                    let mut datctype_b = StringBuilder::new();
                    let mut datlocale_b = StringBuilder::new();
                    let mut daticurules_b = StringBuilder::new();
                    let mut datcollversion_b = StringBuilder::new();

                    for r in &rows {
                        match r.oid { Some(v) => oid_b.append_value(v), None => oid_b.append_null() }
                        datname_b.append_value(&r.datname);
                        datdba_b.append_value(r.datdba);
                        encoding_b.append_value(r.encoding.unwrap_or(6));
                        if let Some(c) = r.datlocprovider { datlocprovider_b.append_value(&c.to_string()); } else { datlocprovider_b.append_null(); }
                        datistemplate_b.append_value(r.datistemplate.unwrap_or(false));
                        datallowconn_b.append_value(r.datallowconn.unwrap_or(true));
                        dathasloginevt_b.append_value(r.dathasloginevt.unwrap_or(false));
                        datconnlimit_b.append_value(r.datconnlimit.unwrap_or(-1));
                        datfrozenxid_b.append_value(&r.datfrozenxid.clone().unwrap_or_else(|| "726".to_string()));
                        datminmxid_b.append_value(&r.datminmxid.clone().unwrap_or_else(|| "1".to_string()));
                        dattablespace_b.append_value(r.dattablespace.unwrap_or(1663));
                        datcollate_b.append_value(&r.datcollate.clone().unwrap_or_else(|| "C".to_string()));
                        datctype_b.append_value(&r.datctype.clone().unwrap_or_else(|| "C".to_string()));
                        if let Some(v) = &r.datlocale { datlocale_b.append_value(v); } else { datlocale_b.append_null(); }
                        if let Some(v) = &r.daticurules { daticurules_b.append_value(v); } else { daticurules_b.append_null(); }
                        if let Some(v) = &r.datcollversion { datcollversion_b.append_value(v); } else { datcollversion_b.append_null(); }
                    }

                    // Build arrays in schema order; use NULL arrays for unknown/system columns.
                    let mut arrays: Vec<ArrayRef> = Vec::new();
                    for field in self.schema.fields() {
                        let name = field.name().as_str();
                        let arr: ArrayRef = match name {
                            "oid" => Arc::new(oid_b.finish()),
                            "datname" => Arc::new(datname_b.finish()),
                            "datdba" => Arc::new(datdba_b.finish()),
                            "encoding" => Arc::new(encoding_b.finish()),
                            "datlocprovider" => Arc::new(datlocprovider_b.finish()),
                            "datistemplate" => Arc::new(datistemplate_b.finish()),
                            "datallowconn" => Arc::new(datallowconn_b.finish()),
                            "dathasloginevt" => Arc::new(dathasloginevt_b.finish()),
                            "datconnlimit" => Arc::new(datconnlimit_b.finish()),
                            "datfrozenxid" => Arc::new(datfrozenxid_b.finish()),
                            "datminmxid" => Arc::new(datminmxid_b.finish()),
                            "dattablespace" => Arc::new(dattablespace_b.finish()),
                            "datcollate" => Arc::new(datcollate_b.finish()),
                            "datctype" => Arc::new(datctype_b.finish()),
                            "datlocale" => Arc::new(datlocale_b.finish()),
                            "daticurules" => Arc::new(daticurules_b.finish()),
                            "datcollversion" => Arc::new(datcollversion_b.finish()),
                            // datacl and any other fields: build NULL arrays sized to rows
                            _ => new_null_array(field.data_type(), rows.len()),
                        };
                        arrays.push(arr);
                    }

                    let new_batch = RecordBatch::try_new(self.schema.clone(), arrays).unwrap();
                    // Merge with existing batches
                    let mut guard = self.mem.batches[0].write().await;
                    if !guard.is_empty() {
                        let merged = concat_batches(&self.schema, &vec![guard[0].clone(), new_batch])?;
                        guard.clear();
                        guard.push(merged);
                    } else {
                        guard.push(new_batch);
                    }

                    if let Some(ctx) = state.as_any().downcast_ref::<SessionContext>() {
                        // Also persist via SQL so subsequent queries see them.
                        let _ = crate::lazy_pg_catalog_helpers::maybe_refresh_pg_database(ctx).await;
                    }
                }
            }
        }
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

pub fn print_execution_log(log: Arc<Mutex<Vec<ScanTrace>>>) {
    let out: Vec<_> = log
        .lock()
        .unwrap()
        .iter()
        .map(|entry| {
            let columns: Vec<_> = match &entry.projection {
                Some(p) => p
                    .iter()
                    .map(|i| entry.schema().field(*i).name().clone())
                    .collect(),
                None => entry.types.keys().cloned().collect(),
            };
            json!({
                "table": entry.table,
                "columns": columns,
                "filters": entry.filters.iter().map(|f| f.to_string()).collect::<Vec<_>>(),
                "types": entry.types,
            })
        })
        .collect();

    log::info!("{}", serde_json::to_string_pretty(&out).unwrap());
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
