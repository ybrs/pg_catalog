use std::any::Any;
use std::sync::Arc;

use arrow::array::Array;
use arrow::datatypes::{Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use datafusion::common::{DFSchemaRef, Result};
use datafusion::common::tree_node::{Transformed, TreeNode};
use datafusion::error::DataFusionError;
use datafusion::execution::{
    context::SessionContext,
    
    session_state::SessionStateBuilder,
    TaskContext,
};
use datafusion::logical_expr::{
    Expr, Join, JoinConstraint, JoinType, LogicalPlan, Subquery,
};
use datafusion::optimizer::{OptimizerConfig, OptimizerRule};
use datafusion::physical_expr::{EquivalenceProperties, PhysicalExpr};
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::{
    DisplayAs, Distribution, ExecutionPlan, Partitioning, PlanProperties, SendableRecordBatchStream, Statistics
};
use futures::StreamExt;
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::DisplayFormatType;
use datafusion::physical_plan::ExecutionPlanProperties;

/// TODO: this didnt really work ! there is invariants check for subquery and i cant get away from it without forking.
/// apparently that rule is not in physical opt rules or logical rules. so couldn't find a proper way to side skip that. 
/// this file is only for historical reasons.



#[derive(Debug, Default)]
pub struct ExistsIneqSemiJoin;

impl OptimizerRule for ExistsIneqSemiJoin {
    fn name(&self) -> &str {
        "exists_ineq_semi_join"
    }
    fn rewrite(
        &self,
        plan: LogicalPlan,
        _cfg: &dyn OptimizerConfig,
    ) -> Result<Transformed<LogicalPlan>> {
        plan.transform_up(|node| match node {
            LogicalPlan::Filter(filter) => {
                if let Expr::Exists(exists) = &filter.predicate {
                    let Subquery { subquery, .. } = &exists.subquery;
                    if let LogicalPlan::Filter(sub_f) = subquery.as_ref() {
                        let left = filter.input.clone();
                        let right = sub_f.input.clone();
                        let schema: DFSchemaRef = left.schema().clone();
                        let joined = LogicalPlan::Join(Join {
                            left,
                            right,
                            on: vec![],
                            filter: Some(sub_f.predicate.clone()),
                            join_type: JoinType::LeftSemi,
                            join_constraint: JoinConstraint::On,
                            schema,
                            null_equals_null: false,
                        });
                        return Ok(Transformed::yes(joined));
                    }
                }
                Ok(Transformed::no(LogicalPlan::Filter(filter)))
            }
            _ => Ok(Transformed::no(node)),
        })
    }
}


#[derive(Debug)]
struct BatchExec {
    data: Vec<RecordBatch>,
    schema: SchemaRef,
    props: PlanProperties, 
}

impl BatchExec {
    fn new(data: Vec<RecordBatch>, schema: SchemaRef) -> Self {

        let props = PlanProperties::new(
            EquivalenceProperties::new(schema.clone()),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Final,
            Boundedness::Bounded,
        );
        Self { data, schema, props }
    }
}

impl DisplayAs for BatchExec {
    fn fmt_as(&self, _: DisplayFormatType, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "BatchExec")
    }
}

use futures::stream;

impl ExecutionPlan for BatchExec {
    fn as_any(&self) -> &dyn Any { self }
    fn schema(&self) -> SchemaRef { self.schema.clone() }
    fn statistics(&self) -> Result<datafusion::common::Statistics, DataFusionError> { Ok(Statistics::default()) }
    fn children(&self) -> Vec<&Arc<(dyn ExecutionPlan + 'static)>> { vec![] }
    
    
    fn with_new_children(
        self: Arc<Self>,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(self as Arc<dyn ExecutionPlan>)
    }

    
    fn execute(&self, _: usize, _: Arc<TaskContext>) -> Result<SendableRecordBatchStream> {
        let schema = self.schema.clone();
        let batches = self.data.clone();
        let stream = stream::iter(batches.into_iter().map(Ok));
        Ok(Box::pin(RecordBatchStreamAdapter::new(schema, stream)))
    }
    
    fn name(&self) -> &str { "BatchExec" }
    fn properties(&self) -> &PlanProperties { &self.props }

}



#[derive(Clone)]
#[derive(Debug)]
pub struct CorrelatedScalarSubqueryExec {
    left: Arc<dyn ExecutionPlan>,
    right: Arc<dyn ExecutionPlan>,
    schema: SchemaRef,
    on: Vec<(Arc<dyn PhysicalExpr>, Arc<dyn PhysicalExpr>)>,
    residual: Option<Arc<dyn PhysicalExpr>>,
    props: PlanProperties,
}

impl CorrelatedScalarSubqueryExec {
    pub fn new(
        left: Arc<dyn ExecutionPlan>,
        right: Arc<dyn ExecutionPlan>,
        on: Vec<(Arc<dyn PhysicalExpr>, Arc<dyn PhysicalExpr>)>,
        residual: Option<Arc<dyn PhysicalExpr>>,
    ) -> Self {
        let schema = SchemaRef::new(Schema::new(
            left.schema()
                .fields()
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
        ));
        let props = PlanProperties::new(
            EquivalenceProperties::new(schema.clone()),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Final,
            Boundedness::Bounded,
        );

        Self { left, right, schema, on, residual, props }
    }
}

impl DisplayAs for CorrelatedScalarSubqueryExec {
    fn fmt_as(&self, t: DisplayFormatType, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default => write!(f, "CorrelatedScalarSubqueryExec"),
            DisplayFormatType::Verbose => todo!(),
            DisplayFormatType::TreeRender => todo!(),
        }
    }
}

impl ExecutionPlan for CorrelatedScalarSubqueryExec {
    fn as_any(&self) -> &dyn Any { self }
    fn schema(&self) -> SchemaRef { self.schema.clone() }
    fn statistics(&self) -> Result<datafusion::common::Statistics, DataFusionError> { Ok(Statistics::default()) }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![&self.left, &self.right]
    }

    fn with_new_children(self: Arc<CorrelatedScalarSubqueryExec>, children: Vec<Arc<dyn ExecutionPlan>>) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(Arc::new(Self::new(children[0].clone(), children[1].clone(), self.on.clone(), self.residual.clone())))
    }

    fn execute(&self, partition: usize, ctx: Arc<TaskContext>) -> Result<SendableRecordBatchStream> {
        let left_stream = self.left.execute(partition, ctx.clone())?;
        let right_partitions = self.right.output_partitioning().partition_count();
        let mut right_batches = vec![];
        for p in 0..right_partitions {
            let mut s = self.right.execute(p, ctx.clone())?;
            while let Some(b) = futures::executor::block_on(s.next()) {
                right_batches.push(b?);
            }
        }
        let right_exec: Arc<dyn ExecutionPlan> = Arc::new(BatchExec::new(right_batches.clone(), self.right.schema()));

        let rctx = ctx.clone();
        let on = self.on.clone();
        let residual = self.residual.clone();
        let schema = self.schema.clone();
        let stream = futures::stream::try_unfold(
            (left_stream, right_exec, on, residual, rctx),
            move |(mut ls, re, on, residual, ctx)| async move {
                match ls.next().await {
                    Some(Ok(batch)) => {
                        let mut scalars = Vec::new();
                        for row in 0..batch.num_rows() {
                            let mut found = None;
                            let mut rs = re.execute(0, ctx.clone())?;
                            while let Some(rb) = rs.next().await {
                                let rb = rb?;
                                if rb.num_rows() == 0 { continue; }
                                let mut ok = true;
                                for (lexpr, rexpr) in &on {
                                    let l_ref = lexpr.evaluate(&batch)?
                                        .into_array(batch.num_rows())?;
                                    let r_ref = rexpr.evaluate(&rb)?
                                        .into_array(rb.num_rows())?;
                                
                                    let l_arr = l_ref
                                        .as_any()
                                        .downcast_ref::<arrow::array::Int32Array>()
                                        .unwrap();
                                
                                    let r_arr = r_ref
                                        .as_any()
                                        .downcast_ref::<arrow::array::Int32Array>()
                                        .unwrap();
                                
                                    if l_arr.is_null(row)
                                        || r_arr.is_null(0)
                                        || l_arr.value(row) != r_arr.value(0)
                                    {
                                        ok = false;
                                        break;
                                    }
                                }
                                if ok {
                                    if let Some(pred) = &residual {

                                        let arr = pred.evaluate(&rb)?
                                        .into_array(rb.num_rows())?;
                                        let bools = arr
                                            .as_any()
                                            .downcast_ref::<arrow::array::BooleanArray>()
                                            .unwrap();
                                        if !bools.value(0) { continue; }

                                    }
                                    found = Some(rb.column(0).slice(0,1));
                                    break;
                                }
                            }
                            scalars.push(found.unwrap_or_else(|| arrow::array::new_null_array(&arrow::datatypes::DataType::Null,1)));
                        }
                        let mut cols = batch.columns().to_vec();
                        cols.extend(scalars);
                        let mut fields = batch.schema().fields().to_vec();

                        fields.push(Arc::new(arrow::datatypes::Field::new(
                            "scalar",
                            arrow::datatypes::DataType::Null,
                            true,
                        )));
                        


                        let out = RecordBatch::try_new(Arc::new(Schema::new(fields)), cols)?;
                        Ok(Some((out, (ls, re, on, residual, ctx))))
                    }
                    _ => Ok(None),
                }
            },
        );
        Ok(Box::pin(RecordBatchStreamAdapter::new(schema, stream)))
    }
    
    fn name(&self) -> &str { "CorrelatedScalarSubqueryExec" }
    fn properties(&self) -> &PlanProperties {
        &self.props
    }

    fn static_name() -> &'static str
    where
        Self: Sized,
    {
        let full_name = std::any::type_name::<Self>();
        let maybe_start_idx = full_name.rfind(':');
        match maybe_start_idx {
            Some(start_idx) => &full_name[start_idx + 1..],
            None => "UNKNOWN",
        }
    }
    
    fn check_invariants(&self, _check: datafusion::physical_plan::execution_plan::InvariantLevel) -> Result<()> {
        Ok(())
    }
    
    fn required_input_distribution(&self) -> Vec<datafusion::physical_plan::Distribution> {
        std::vec![Distribution::UnspecifiedDistribution; self.children().len()]
    }
    
    fn required_input_ordering(&self) -> Vec<Option<datafusion::physical_expr::LexRequirement>> {
        std::vec![None; self.children().len()]
    }
    
    fn maintains_input_order(&self) -> Vec<bool> {
        std::vec![false; self.children().len()]
    }
    
    fn benefits_from_input_partitioning(&self) -> Vec<bool> {
        // By default try to maximize parallelism with more CPUs if
        // possible
        self.required_input_distribution()
            .into_iter()
            .map(|dist| !std::matches!(dist, Distribution::SinglePartition))
            .collect()
    }
    
    fn repartitioned(
        &self,
        _target_partitions: usize,
        _config: &datafusion::config::ConfigOptions,
    ) -> Result<Option<Arc<dyn ExecutionPlan>>> {
        Ok(None)
    }
    
    fn metrics(&self) -> Option<datafusion::physical_plan::metrics::MetricsSet> {
        None
    }
    
    fn supports_limit_pushdown(&self) -> bool {
        false
    }
    
    fn with_fetch(&self, _limit: Option<usize>) -> Option<Arc<dyn ExecutionPlan>> {
        None
    }
    
    fn fetch(&self) -> Option<usize> {
        None
    }
    
    fn cardinality_effect(&self) -> datafusion::physical_plan::execution_plan::CardinalityEffect {
        datafusion::physical_plan::execution_plan::CardinalityEffect::Unknown
    }
    
    fn try_swapping_with_projection(
        &self,
        _projection: &datafusion::physical_plan::projection::ProjectionExec,
    ) -> Result<Option<Arc<dyn ExecutionPlan>>> {
        Ok(None)
    }
}

pub fn new_ctx_without_scalar_validator() -> SessionContext {
    let tmp_state = SessionStateBuilder::new().with_default_features().build();
    let mut analyzers = tmp_state.analyzer().rules.clone();
    let mut optimizers = tmp_state.optimizer().rules.clone();
    analyzers.retain(|r| r.name() != "subquery_validator");
    optimizers.push(Arc::new(ExistsIneqSemiJoin::default()));
    let state = SessionStateBuilder::new()
        .with_analyzer_rules(analyzers)
        .with_optimizer_rules(optimizers)
        .with_default_features()
        .build();

    for r in &state.analyzer().rules { println!("Analyzer: {}", r.name()); }

    SessionContext::new_with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Int32Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use datafusion::datasource::MemTable;
    use std::sync::Arc;

    #[tokio::test]
    async fn scalar_subquery_runs() -> datafusion::common::Result<()> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("other", DataType::Int32, false),
        ]));
        let outer = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from(vec![1, 1, 2])),
                Arc::new(Int32Array::from(vec![10, 20, 10])),
            ],
        )?;
        let inner = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from(vec![1, 2])),
                Arc::new(Int32Array::from(vec![15, 20])),
            ],
        )?;
        let ctx = new_ctx_without_scalar_validator();
        ctx.register_table(
            "a",
            Arc::new(MemTable::try_new(schema.clone(), vec![vec![outer]])?),
        )?;
        ctx.register_table(
            "b",
            Arc::new(MemTable::try_new(schema, vec![vec![inner]])?),
        )?;
        let df = ctx
            .sql(
                "SELECT a.id,
                        (SELECT b.other
                         FROM b
                         WHERE b.id = a.id AND b.other <> a.other limit 1) AS c2
                 FROM a
                 ORDER BY id, other",
            )
            .await?;
        let batches = df.collect().await?;
        assert_eq!(batches[0].num_rows(), 3);
        Ok(())
    }
}
