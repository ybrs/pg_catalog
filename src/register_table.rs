//! Helper for putting an empty, schema-only table into a session.
//!
//! Tests and embedders need a table that exists and has the right column types but
//! holds no data, so a catalog query can be planned against it without materializing
//! any rows.

use std::sync::Arc;

use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use datafusion::catalog::{
    CatalogProvider, MemoryCatalogProvider, MemorySchemaProvider, SchemaProvider,
};
use datafusion::datasource::MemTable;
use datafusion::error::Result;
use datafusion::execution::context::SessionContext;

/// Register a new datafusion memtable in the given catalog and schema.
/// Creates the catalog or schema if it does not exist.
/// Example:
/// ```text
///  use datafusion_pg_catalog::register_table::register_table;
///  register_table(
///     &ctx,
///     "crm",
///     "crm",
///     "users",
///     vec![
///         ("id", DataType::Int32, false),
///         ("name", DataType::Utf8, true),
///     ],
/// )?;
/// let _ = dispatch_query(&ctx, "SELECT 1", None, None, |_c, _q, _p, _t| {
///     async { Ok((Vec::new(), Arc::new(Schema::empty()))) }
/// })
/// .await?;
/// ```
///
/// # Errors
///
/// Returns an error if the schema cannot be registered under the catalog, or if the
/// table name is already taken in that schema - both are reported by the underlying
/// `MemoryCatalogProvider`/`MemorySchemaProvider`.
pub fn register_table(
    ctx: &SessionContext,
    catalog_name: &str,
    schema_name: &str,
    table_name: &str,
    columns: Vec<(&str, DataType, bool)>,
) -> Result<()> {
    let catalog: Arc<dyn CatalogProvider> = if let Some(cat) = ctx.catalog(catalog_name) {
        cat
    } else {
        let cat = Arc::new(MemoryCatalogProvider::new());
        ctx.register_catalog(catalog_name, cat.clone());
        cat
    };

    let schema: Arc<dyn SchemaProvider> = if let Some(sch) = catalog.schema(schema_name) {
        sch
    } else {
        let sch = Arc::new(MemorySchemaProvider::new());
        catalog.register_schema(schema_name, sch.clone())?;
        sch
    };

    let fields: Vec<Field> = columns
        .into_iter()
        .map(|(name, dt, nullable)| Field::new(name, dt, nullable))
        .collect();
    let table_schema = Arc::new(Schema::new(fields));

    // use an empty record batch so the table exists
    let batch = RecordBatch::new_empty(table_schema.clone());
    let table = MemTable::try_new(table_schema, vec![vec![batch]])?;

    schema.register_table(table_name.to_string(), Arc::new(table))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::datatypes::DataType;

    /// A registered table is queryable, holds no rows, and keeps the per-column
    /// nullability it was declared with.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_register_table() -> Result<()> {
        let config = datafusion::execution::context::SessionConfig::new()
            .with_default_catalog_and_schema("crm", "crm");
        let ctx = SessionContext::new_with_config(config);

        register_table(
            &ctx,
            "crm",
            "crm",
            "mytable",
            vec![
                ("id", DataType::Int32, false),
                ("name", DataType::Utf8, true),
            ],
        )?;

        let catalog = ctx.catalog("crm").unwrap();
        let schema = catalog.schema("crm").unwrap();
        assert!(schema.table_names().contains(&"mytable".to_string()));

        let df = ctx.sql("select count(*) from crm.mytable").await?;
        let batches = df.collect().await?;
        let count = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap()
            .value(0);
        assert_eq!(count, 0);

        // verify nullability settings
        let table = schema.table("mytable").await?.expect("table should exist");
        let schema = table.schema();
        let fields = schema.fields();
        assert!(!fields[0].is_nullable());
        assert!(fields[1].is_nullable());

        Ok(())
    }
}
