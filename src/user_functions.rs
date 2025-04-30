use async_trait::async_trait;
use datafusion::arrow::array::Int64Array;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::{Session, TableFunctionImpl};
use datafusion::common::{plan_err, ScalarValue};
use datafusion::datasource::memory::MemorySourceConfig;
use datafusion::datasource::TableProvider;
use datafusion::error::Result;
use datafusion::logical_expr::{Expr, TableType};
use datafusion::prelude::*;
use std::sync::Arc;
use datafusion::execution::SessionState;

#[derive(Debug)]
struct RegClassOidTable {
    schema: SchemaRef,
    relname: String,
}

#[async_trait]
impl TableProvider for RegClassOidTable {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        session: &dyn Session,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> Result<Arc<dyn datafusion::physical_plan::ExecutionPlan>> {


        let state = if let Some(s) = session.as_any().downcast_ref::<SessionState>() {
            s.clone()
        } else {
            return plan_err!("failed to downcast Session to SessionState");
        };

        let ctx = SessionContext::new_with_state(state);

        let query = format!(
            "SELECT oid FROM pg_catalog.pg_class WHERE relname = '{}'",
            self.relname
        );
        let df = ctx.sql(&query).await?;
        let mut batches = df.collect().await?;
        if batches.is_empty() {
            let empty_array = Int64Array::from(vec![Option::<i64>::None]);
            let empty_batch =
                RecordBatch::try_new(self.schema.clone(), vec![Arc::new(empty_array)])?;
            batches.push(empty_batch);
        }
        Ok(MemorySourceConfig::try_new_exec(
            &[batches],
            self.schema(),
            projection.cloned(),
        )?)
    }
}

#[derive(Debug)]
struct RegClassOidFunc;

impl TableFunctionImpl for RegClassOidFunc {
    fn call(&self, exprs: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        let relname = if let Some(Expr::Literal(ScalarValue::Utf8(Some(ref s)))) = exprs.first() {
            s.clone()
        } else {
            return plan_err!("regclass_oid requires one string argument");
        };
        let schema = Arc::new(Schema::new(vec![Field::new("oid", DataType::Int64, true)]));
        Ok(Arc::new(RegClassOidTable { schema, relname }))
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Int64Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use datafusion::catalog::memory::{MemoryCatalogProvider, MemorySchemaProvider};
    use datafusion::datasource::MemTable;
    use datafusion::error::Result;
    use datafusion::prelude::*;
    use std::sync::Arc;
    use datafusion::catalog::{CatalogProvider, SchemaProvider};

    async fn make_ctx() -> Result<SessionContext> {
        let mut config = datafusion::execution::context::SessionConfig::new()
            .with_default_catalog_and_schema("public", "pg_catalog");

        let ctx = SessionContext::new_with_config(config);
        ctx.register_udtf("regclass_oid", Arc::new(RegClassOidFunc));

        let relname = StringArray::from(vec!["pg_constraint", "demo"]);
        let oid = Int64Array::from(vec![2606i64, 9999i64]);
        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("relname", DataType::Utf8, false),
                Field::new("oid", DataType::Int64, false),
            ])),
            vec![Arc::new(relname), Arc::new(oid)],
        )?;

        let catalog = Arc::new(MemoryCatalogProvider::new());
        ctx.register_catalog("public", catalog.clone());

        let schema = Arc::new(MemorySchemaProvider::new());
        catalog.register_schema("pg_catalog", schema.clone());

        let table = MemTable::try_new(batch.schema(), vec![vec![batch]])?;

        schema.register_table("pg_class".parse().unwrap(), Arc::new(table))?;
        Ok(ctx)
    }

    #[tokio::test]
    async fn test_regclass_with_oid() -> Result<()> {
        let ctx = make_ctx().await?;
        let batches = ctx
            .sql("SELECT * FROM regclass_oid('pg_constraint');")
            .await?
            .collect()
            .await?;
        let col = batches[0].column(0).as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(col.value(0), 2606);
        Ok(())
    }

    #[tokio::test]
    async fn test_query_without_function() -> Result<()> {
        let ctx = make_ctx().await?;
        let batches = ctx
            .sql("SELECT oid FROM pg_catalog.pg_class WHERE relname = 'pg_constraint';")
            .await?
            .collect()
            .await?;
        let col = batches[0].column(0).as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(col.value(0), 2606);
        Ok(())
    }

    #[tokio::test]
    async fn test_regclass_oid_arithmetic() -> Result<()> {
        let ctx = make_ctx().await?;
        let batches = ctx
            .sql("SELECT oid + 1 AS n FROM regclass_oid('pg_constraint');")
            .await?
            .collect()
            .await?;
        let col = batches[0].column(0).as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(col.value(0), 2607);
        Ok(())
    }
}
