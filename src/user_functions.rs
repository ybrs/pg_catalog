use async_trait::async_trait;
use datafusion::arrow::array::Int64Array;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::{Session, TableFunctionImpl};
use datafusion::common::{plan_err, ScalarValue};
use datafusion::datasource::memory::MemorySourceConfig;
use datafusion::datasource::TableProvider;
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::{Expr, TableType};
use datafusion::prelude::*;
use std::sync::Arc;
use datafusion::execution::SessionState;
use datafusion::logical_expr::{ColumnarValue, Volatility};
use arrow::array::{as_string_array, Array, ArrayRef, StringBuilder};
use futures::executor::block_on;
use tokio::task::block_in_place;
use arrow::datatypes::DataType as ArrowDataType;

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
pub struct RegClassOidFunc;

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

pub fn register_scalar_regclass_oid(ctx: &SessionContext) -> Result<()> {
    let ctx_arc = Arc::new(ctx.clone());

    let fn_ = Arc::new(move |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let name = match &args[0] {
            ColumnarValue::Scalar(ScalarValue::Utf8(Some(s))) => s.clone(),
            ColumnarValue::Scalar(ScalarValue::Utf8(None)) => {
                return Ok(ColumnarValue::Scalar(ScalarValue::Int64(None)))
            }
            _ => return plan_err!("oid expects text"),
        };

        let sql = format!(
            "SELECT oid FROM pg_catalog.pg_class WHERE relname = '{}'",
            name.replace('\'', "''")
        );

        println!("udf query {:?}", sql);

        let opt: Option<i64> = block_in_place(|| {
            block_on(async {
                let batches = ctx_arc.sql(&sql).await?.collect().await?;
                if batches.is_empty() || batches[0].num_rows() == 0 {
                    Ok::<Option<i64>, DataFusionError>(None)
                } else {
                    let col = batches[0].column(0);
                    if let Some(arr) = col.as_any().downcast_ref::<Int64Array>() {
                        if arr.is_null(0) {
                            Ok(None)
                        } else {
                            Ok(Some(arr.value(0)))
                        }
                    } else if let Some(arr) = col.as_any().downcast_ref::<arrow::array::Int32Array>() {
                        if arr.is_null(0) {
                            Ok(None)
                        } else {
                            Ok(Some(arr.value(0) as i64))
                        }
                    } else {
                        // any other type ⇒ return NULL, don't panic
                        Ok(None)
                    }
                }
            })
        })?;

        Ok(ColumnarValue::Scalar(ScalarValue::Int64(opt)))
    });

    let udf = create_udf(
        "oid",
        vec![DataType::Utf8],
        DataType::Int64,
        Volatility::Immutable,
        fn_,
    );
    ctx.register_udf(udf);
    Ok(())
}



pub fn register_scalar_pg_tablespace_location(ctx: &SessionContext) -> Result<()> {
    // TODO: this always returns empty string for now.
    //   If there is a db supporting tablespaces, this should be done correctly.
    let ctx_arc = Arc::new(ctx.clone());

    let udf = create_udf(
        "pg_tablespace_location",
        vec![ArrowDataType::Utf8],
        ArrowDataType::Utf8,
        Volatility::Immutable,
        {
            std::sync::Arc::new(move |args| {
                Ok(ColumnarValue::Scalar(ScalarValue::Utf8(None)))
            })
        },
    );
    ctx_arc.register_udf(udf);
    Ok(())
}

// /// FORMAT_TYPE function
// pub fn register_scalar_format_type(ctx: &SessionContext) -> Result<()> {
//     // TODO: this just returns some known types, it should actually run a query
//     let ctx_arc = Arc::new(ctx.clone());
//     let udf = create_udf(
//         "format_type",
//         vec![ArrowDataType::Int64, ArrowDataType::Int64],
//         ArrowDataType::Utf8,
//         Volatility::Immutable,
//         Arc::new(|args| {
//             let oid = match &args[0] {
//                 ColumnarValue::Scalar(ScalarValue::Int64(v)) => *v,
//                 _ => return Err(DataFusionError::Execution("first arg must be INT8 scalar".into())),
//             };
//             let typmod = match &args[1] {
//                 ColumnarValue::Scalar(ScalarValue::Int64(v)) => *v,
//                 _ => return Err(DataFusionError::Execution("second arg must be INT8 scalar".into())),
//             };
//             let s = match oid {
//                 Some(16) => "boolean".to_string(),
//                 Some(20) => "bigint".to_string(),
//                 Some(21) => "smallint".to_string(),
//                 Some(23) => "integer".to_string(),
//                 Some(25) => "text".to_string(),
//                 Some(1043) => {
//                     if let Some(tm) = typmod {
//                         if tm >= 0 { format!("character varying({})", tm - 4) } else { "character varying".to_string() }
//                     } else { "character varying".to_string() }
//                 }
//                 _ => oid.map(|o| o.to_string()).unwrap_or_default(),
//             };
//             Ok(ColumnarValue::Scalar(ScalarValue::Utf8(Some(s))))
//         }),
//     );
//     ctx_arc.register_udf(udf);
//     Ok(())
// }


fn format_type_string(oid: i64, typmod: Option<i64>) -> String {
    match oid {
        16 => "boolean".to_string(),
        20 => "bigint".to_string(),
        21 => "smallint".to_string(),
        23 => "integer".to_string(),
        25 => "text".to_string(),
        1043 => {
            if let Some(tm) = typmod {
                if tm >= 0 {
                    format!("character varying({})", tm - 4)
                } else {
                    "character varying".to_string()
                }
            } else {
                "character varying".to_string()
            }
        }
        _ => oid.to_string(),
    }
}
use datafusion::common::cast::as_int64_array;


pub fn register_scalar_format_type(ctx: &SessionContext) -> Result<()> {
    let ctx_arc = Arc::new(ctx.clone());
    let fun = |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let arrays = ColumnarValue::values_to_arrays(args)?;
        let oids = as_int64_array(&arrays[0])?;
        let mods = as_int64_array(&arrays[1])?;
        let mut builder = StringBuilder::new();
        for i in 0..oids.len() {
            if oids.is_null(i) {
                builder.append_null();
            } else {
                let s = format_type_string(oids.value(i), if mods.is_null(i) { None } else { Some(mods.value(i)) });
                builder.append_value(&s);
            }
        }
        Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
    };
    let udf = create_udf(
        "format_type",
        vec![ArrowDataType::Int64, ArrowDataType::Int64],
        ArrowDataType::Utf8,
        Volatility::Immutable,
        Arc::new(fun),
    );
    ctx_arc.register_udf(udf);
    Ok(())
}


pub fn register_scalar_pg_get_expr(ctx: &SessionContext) -> Result<()> {
    let ctx_arc = Arc::new(ctx.clone());
    let fun = |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let arrays = ColumnarValue::values_to_arrays(args)?;
        let exprs = as_string_array(&arrays[0]);
        let mut builder = StringBuilder::new();
        for i in 0..exprs.len() {
            if exprs.is_null(i) {
                builder.append_null();
            } else {
                builder.append_value(exprs.value(i));
            }
        }
        Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
    };
    let udf = create_udf(
        "pg_catalog.pg_get_expr",
        vec![ArrowDataType::Utf8, ArrowDataType::Int64],
        ArrowDataType::Utf8,
        Volatility::Immutable,
        Arc::new(fun),
    );
    ctx_arc.register_udf(udf);
    Ok(())
}

pub fn register_scalar_pg_get_partkeydef(ctx: &SessionContext) -> Result<()> {
    let ctx_arc = Arc::new(ctx.clone());
    let fun = |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let arrays = ColumnarValue::values_to_arrays(args)?;
        let oids = as_int64_array(&arrays[0])?;
        let mut builder = StringBuilder::new();
        for i in 0..oids.len() {
            builder.append_null();
        }
        Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
    };
    let udf = create_udf(
        "pg_catalog.pg_get_partkeydef",
        vec![ArrowDataType::Int64],
        ArrowDataType::Utf8,
        Volatility::Immutable,
        Arc::new(fun),
    );
    ctx_arc.register_udf(udf);
    Ok(())
}


pub fn register_current_schema(ctx: &SessionContext) -> Result<()> {
    // TODO: this always returns public
    //   If there is a db supporting tablespaces, this should be done correctly.
    let ctx_arc = Arc::new(ctx.clone());

    let udf = create_udf(
        "current_schema",
        vec![],
        ArrowDataType::Utf8,
        Volatility::Immutable,
        {
            std::sync::Arc::new(move |_args| {
                Ok(ColumnarValue::Scalar(ScalarValue::Utf8(Some("public".to_string()))))
            })
        },
    );
    ctx_arc.register_udf(udf);
    Ok(())
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
        ctx.register_udtf("regclass_oid", Arc::new(RegClassOidFunc));
        register_scalar_regclass_oid(&ctx)?;
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

    #[tokio::test(flavor = "multi_thread")]
    async fn test_regclass_scalar_ok() -> Result<()> {
        let ctx = make_ctx().await?;
        let batches = ctx
            .sql("SELECT oid('pg_constraint') AS v;")
            .await?
            .collect()
            .await?;
        let col = batches[0].column(0).as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(col.value(0), 2606);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_regclass_scalar_null() -> Result<()> {
        let ctx = make_ctx().await?;
        let batches = ctx
            .sql("SELECT oid('does_not_exist') AS v;")
            .await?
            .collect()
            .await?;
        assert!(batches[0].column(0).as_any().downcast_ref::<Int64Array>().unwrap().is_null(0));
        Ok(())
    }

}
