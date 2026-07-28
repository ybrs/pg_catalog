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

/// Translate a `PostgreSQL` type name (as written in a catalog table definition)
/// into the Arrow type used to store that column.
///
/// Matching is case-insensitive. Unknown type names fall back to `Utf8` so a
/// catalog column with an exotic type still round-trips as text rather than
/// failing to build.
#[must_use]
pub fn map_pg_type(pg_type: &str) -> DataType {
    let lower = pg_type.to_lowercase();
    match lower.as_str() {
        // Integer vectors and arrays. These arms keep the element width equal to
        // the width of the matching scalar type (int2/int4 scalars are Int32,
        // int8 is Int64) so `intcol = ANY(array)` compares like-with-like - e.g.
        // `pg_attribute.attnum = ANY(pg_constraint.conkey)`. `_oid` follows
        // oidvector at Int64 since oid values can exceed Int32. Without these
        // arms the names match the `_`-prefix text-array rule below and the
        // integer values become unmatchable text.
        "int2vector" | "_int2" | "_int4" => {
            DataType::List(Arc::new(Field::new("item", DataType::Int32, true)))
        }
        "oidvector" | "_int8" | "_oid" => {
            DataType::List(Arc::new(Field::new("item", DataType::Int64, true)))
        }
        _ if lower.ends_with("[]") || lower.starts_with('_') => {
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true)))
        }
        "uuid" => DataType::Utf8,
        "int" | "integer" | "int4" => DataType::Int32,
        "bigint" | "int8" => DataType::Int64,
        // Floating-point types. Map to the matching Arrow width so the wire type
        // is faithful: real/float4 -> Float32 (advertised as FLOAT4, OID 700);
        // double precision/float8 -> Float64 (FLOAT8, OID 701). Bare `float` is
        // double precision in PostgreSQL.
        "real" | "float4" => DataType::Float32,
        "float" | "double" | "double precision" | "float8" => DataType::Float64,
        "bool" | "boolean" => DataType::Boolean,
        "bytea" => DataType::Binary,
        _ if lower.starts_with("varchar") => DataType::Utf8,
        _ => DataType::Utf8,
    }
}

/// Gives a recorded scan a schema to look column names up in.
///
/// A [`ScanTrace`] keeps column names and type names as plain strings rather
/// than holding a `SchemaRef`, so the schema is rebuilt on demand when a
/// projection index has to be resolved back to a column name.
trait SchemaAccess {
    /// Build the Arrow schema described by this value.
    fn schema(&self) -> SchemaRef;
}

impl SchemaAccess for ScanTrace {
    /// Rebuild a schema from the recorded column names, in the order they are
    /// stored in `column_types`.
    ///
    /// Only the field names are consumed by callers; the field types are
    /// whatever [`map_pg_type`] makes of the recorded type name, which is an
    /// Arrow type name rather than a `PostgreSQL` one, so they are indicative
    /// only.
    fn schema(&self) -> SchemaRef {
        Arc::new(Schema::new(
            self.column_types
                .iter()
                .map(|(name, pg_type)| Field::new(name, map_pg_type(pg_type), true))
                .collect::<Vec<_>>(),
        ))
    }
}

/// One recorded scan of a catalog table: which table was read, which columns
/// were projected, and which filters were pushed down.
///
/// Tests assert on these traces to check that a query only touches the catalog
/// tables and columns it is supposed to.
#[derive(Debug, Clone)]
pub struct ScanTrace {
    /// Name of the table that was scanned.
    table: String,
    /// Column indexes `DataFusion` asked for, or `None` for "all columns".
    projection: Option<Vec<usize>>,
    /// Filter expressions `DataFusion` offered to the provider.
    filters: Vec<Expr>,
    /// Column name -> Arrow type name of the scanned table, kept in a `BTreeMap`
    /// so the recorded trace has a stable, name-sorted order across runs.
    column_types: BTreeMap<String, String>,
}

/// An in-memory table provider that appends a [`ScanTrace`] to a shared log on
/// every scan.
///
/// It delegates all real work to an inner [`MemTable`]; the only added
/// behaviour is the recording, which is what lets tests observe catalog access
/// patterns without a real `PostgreSQL` server.
#[derive(Debug)]
pub struct ScanRecordingMemTable {
    /// Schema of the table, shared with the inner `MemTable`.
    schema: SchemaRef,
    /// Inner provider holding the rows and executing the scans.
    mem: Arc<MemTable>,
    /// Scan log shared by every table in a session.
    scan_traces: Arc<Mutex<Vec<ScanTrace>>>,
    /// Table name reported in each recorded trace.
    table_name: String,
}

impl ScanRecordingMemTable {
    /// Build a provider serving `data` for `table_name` and recording its scans
    /// into the shared `scan_traces` log.
    ///
    /// # Panics
    ///
    /// Panics if any batch in `data` does not match `schema`, which is what
    /// `MemTable::try_new` rejects. Catalog tables are built from the same
    /// schema that produced their batches, so a mismatch is a bug in the
    /// caller rather than a runtime condition to recover from.
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
    /// Schema of the rows this provider serves.
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    /// Catalog tables served from memory are ordinary base tables; views are
    /// registered separately as view providers.
    fn table_type(&self) -> TableType {
        TableType::Base
    }

    /// Accept every filter as inexact, so `DataFusion` still re-applies it after
    /// the scan. The provider does not evaluate filters itself - it only
    /// records them - so claiming exact pushdown would drop the predicate.
    ///
    /// # Errors
    ///
    /// Never returns an error; the `Result` is part of the `TableProvider`
    /// contract.
    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> Result<Vec<TableProviderFilterPushDown>> {
        Ok(vec![TableProviderFilterPushDown::Inexact; filters.len()])
    }

    /// Record the scan (table, projection, filters, column types) in the shared
    /// log, then delegate execution to the inner `MemTable`.
    ///
    /// # Errors
    ///
    /// Returns whatever error the inner `MemTable` scan produces, for instance
    /// a projection index outside the schema.
    ///
    /// # Panics
    ///
    /// Panics if the shared scan log mutex is poisoned by a thread that
    /// panicked while holding it.
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

    /// Materialize `input` and store it in the inner `MemTable`'s single
    /// partition, either replacing the existing rows (`InsertOp::Overwrite`) or
    /// appending to them.
    ///
    /// The rows are concatenated into one batch so the partition always holds
    /// at most a single batch, which keeps the append path a plain read of
    /// `batches[0]`.
    ///
    /// # Errors
    ///
    /// Returns an error if executing `input` fails, or if the produced batches
    /// cannot be concatenated because their schema differs from this table's
    /// schema.
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
        let merged = if insert_op == InsertOp::Overwrite {
            concat_batches(&self.schema, &new_batches)?
        } else {
            let guard = self.mem.batches[0].write().await;
            if guard.is_empty() {
                concat_batches(&self.schema, &new_batches)?
            } else {
                let mut all = vec![guard[0].clone()];
                all.append(&mut new_batches);
                concat_batches(&self.schema, &all)?
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
///
/// Projections are resolved back to column names using the trace's own schema,
/// so the output names columns rather than opaque indexes.
///
/// # Panics
///
/// Panics if the scan log mutex is poisoned by a thread that panicked while
/// holding it.
pub fn log_scan_traces(scan_traces: &Arc<Mutex<Vec<ScanTrace>>>) {
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
                "filters": trace.filters.iter().map(std::string::ToString::to_string).collect::<Vec<_>>(),
                "types": trace.column_types,
            })
        })
        .collect();

    log::info!("{}", serde_json::to_string_pretty(&serialized).unwrap());
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Scalar type names map to their Arrow counterparts, and an unknown name
    /// falls back to `Utf8`.
    #[test]
    fn test_map_pg_type() {
        assert_eq!(map_pg_type("int"), DataType::Int32);
        assert_eq!(map_pg_type("integer"), DataType::Int32);
        assert_eq!(map_pg_type("bigint"), DataType::Int64);
        assert_eq!(map_pg_type("bool"), DataType::Boolean);
        assert_eq!(map_pg_type("varchar(20)"), DataType::Utf8);
        assert_eq!(map_pg_type("unknown"), DataType::Utf8);
    }

    /// Every spelling of the `PostgreSQL` float types maps to the Arrow width
    /// that matches the advertised wire type.
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

    /// Array and vector type names map to lists whose element width matches the
    /// corresponding scalar type, and text arrays keep the `Utf8` default.
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

        // Small-int arrays (`conkey` `_int2`, index keys `_int4`) -> Int32 lists.
        for small in ["_int2", "_int4"] {
            match map_pg_type(small) {
                DataType::List(field) => assert_eq!(field.data_type(), &DataType::Int32),
                other => panic!("unexpected datatype for {small}: {other:?}"),
            }
        }

        // Big-int and oid arrays (`_int8`, `proallargtypes` `_oid`) -> Int64 lists,
        // since oids can exceed Int32.
        for big in ["_int8", "_oid"] {
            match map_pg_type(big) {
                DataType::List(field) => assert_eq!(field.data_type(), &DataType::Int64),
                other => panic!("unexpected datatype for {big}: {other:?}"),
            }
        }
    }
}
