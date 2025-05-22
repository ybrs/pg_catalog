use datafusion::{
    common::{
        tree_node::Transformed,
        Result,
    },
    logical_expr::{Expr, LogicalPlan},
    optimizer::{ApplyOrder, OptimizerConfig, OptimizerRule},
};

#[derive(Debug)]
pub struct StripPgGetOne;

impl OptimizerRule for StripPgGetOne {
    fn name(&self) -> &str { "strip_pg_get_one" }

    // ask the optimiser framework to call us bottom‑up on every node
    fn apply_order(&self) -> Option<ApplyOrder> { Some(ApplyOrder::BottomUp) }

    fn rewrite(
        &self,
        plan: LogicalPlan,
        _conf: &dyn OptimizerConfig,
    ) -> Result<Transformed<LogicalPlan>> {
        plan.map_expressions(|e| {
            match e {
                // unwrap   pggetone(<expr>)   ➜   <expr>
                Expr::ScalarFunction(sf)
                    if sf.func.name() == "pg_get_one" && sf.args.len() == 1 =>
                {
                    Ok(Transformed::yes(sf.args[0].clone()))
                }
                _ => Ok(Transformed::no(e.clone())),
            }
        })
    }
}


#[cfg(test)]
mod tests {
    use crate::user_functions::{register_pg_get_one, register_scalar_regclass_oid, RegClassOidFunc};

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

    /* TODO:

    postgresql handles number::regclass differently. it just passes them as oid.

    postgres=# select '222222222'::regclass::oid;
    oid
    -----------
     222222222
    (1 row)


     */


    async fn make_ctx() -> Result<SessionContext> {
        let mut config = datafusion::execution::context::SessionConfig::new()
            .with_default_catalog_and_schema("public", "pg_catalog");

        let ctx = SessionContext::new_with_config(config);
        ctx.add_optimizer_rule(Arc::new(StripPgGetOne));

        ctx.register_udtf("regclass_oid", Arc::new(RegClassOidFunc));
        register_scalar_regclass_oid(&ctx)?;
        register_pg_get_one(&ctx)?;
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
        catalog.register_schema("pg_catalog", schema.clone())?;

        let table = MemTable::try_new(batch.schema(), vec![vec![batch]])?;

        schema.register_table("pg_class".parse().unwrap(), Arc::new(table))?;
        Ok(ctx)
    }


    #[tokio::test]
    async fn test_pggetone_correlated_subquery() -> Result<()> {
        use crate::logical_plan_rules::StripPgGetOne;
        let ctx = make_ctx().await?;
        ctx.add_optimizer_rule(Arc::new(StripPgGetOne));
        let batches = ctx
            .sql(
                "SELECT pg_get_one(
                    (SELECT max(relname)
                    FROM pg_catalog.pg_class AS i
                    WHERE i.relname = C.relname)
                ) AS v
                FROM pg_catalog.pg_class AS C
                WHERE C.relname = 'pg_constraint'
                LIMIT 1;",
            )
            .await?
            .collect()
            .await?;
        let col = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(col.value(0), "pg_constraint");
        Ok(())
    }

}