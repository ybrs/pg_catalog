//! Custom optimizer rules for `DataFusion`.
//!
//! Currently strips `pg_get_one()` calls so they don't block other optimizations,
//! which brings planning closer to what `PostgreSQL` does.

use datafusion::{
    common::{tree_node::Transformed, Result},
    logical_expr::{Expr, LogicalPlan},
    optimizer::{ApplyOrder, OptimizerConfig, OptimizerRule},
};

/// Optimizer rule that unwraps `pg_get_one(<expr>)` back to `<expr>`.
///
/// `pg_get_one` marks a scalar subquery that must yield a single value, mirroring how
/// `PostgreSQL` treats such subqueries. Once planning is done the wrapper is opaque to
/// `DataFusion` and would block subquery decorrelation and the rewrites that follow it,
/// so this rule removes it.
#[derive(Debug)]
pub struct StripPgGetOne;

/// Applies the unwrapping over every expression in the plan.
impl OptimizerRule for StripPgGetOne {
    /// The rule's name as it appears in optimizer traces and explain output.
    fn name(&self) -> &'static str {
        "strip_pg_get_one"
    }

    /// Ask the optimiser framework to call us bottom-up on every node, so nested
    /// `pg_get_one` calls are unwrapped inside-out.
    fn apply_order(&self) -> Option<ApplyOrder> {
        Some(ApplyOrder::BottomUp)
    }

    /// Replace every single-argument `pg_get_one` call in this node's expressions with
    /// its argument, leaving all other expressions untouched.
    ///
    /// # Errors
    ///
    /// Returns an error only if `map_expressions` fails while walking the plan; the
    /// rewrite itself cannot fail.
    fn rewrite(
        &self,
        plan: LogicalPlan,
        _conf: &dyn OptimizerConfig,
    ) -> Result<Transformed<LogicalPlan>> {
        plan.map_expressions(|e| {
            match e {
                // unwrap   pggetone(<expr>)   ->   <expr>
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
    use crate::user_functions::{
        register_pg_get_one, register_scalar_regclass_oid, RegClassOidFunc,
    };

    use super::*;
    use arrow::array::{Int64Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use datafusion::catalog::memory::{MemoryCatalogProvider, MemorySchemaProvider};
    use datafusion::catalog::{CatalogProvider, SchemaProvider};
    use datafusion::datasource::MemTable;
    use datafusion::error::Result;
    use datafusion::prelude::*;
    use std::sync::Arc;

    /* TODO:

    postgresql handles number::regclass differently. it just passes them as oid.

    postgres=# select '222222222'::regclass::oid;
    oid
    -----------
     222222222
    (1 row)


     */

    /// Build a session holding a two-row `pg_catalog.pg_class`, the regclass/`pg_get_one`
    /// UDFs and the [`StripPgGetOne`] rule - the minimum needed to plan a correlated
    /// scalar subquery over the catalog.
    ///
    /// # Errors
    ///
    /// Returns an error if a UDF cannot be registered, if the seed record batch does not
    /// match its schema, or if the catalog/schema/table registration is rejected.
    fn make_ctx() -> Result<SessionContext> {
        let config = datafusion::execution::context::SessionConfig::new()
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

    /// A correlated scalar subquery wrapped in `pg_get_one` still plans and executes:
    /// the rule unwraps the marker so decorrelation can run.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_pggetone_correlated_subquery() -> Result<()> {
        use crate::logical_plan_rules::StripPgGetOne;
        let ctx = make_ctx()?;
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
