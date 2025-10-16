// Collection of custom UDF and UDTF implementations.
// Provides functions like oid(), pg_get_array and others so queries behave like PostgreSQL.
// Added to extend DataFusion with features required by pg_catalog emulation.
use arrow::array::{
    as_string_array, Array, ArrayRef, StringBuilder,
    TimestampMicrosecondArray,
};
use arrow::datatypes::DataType as ArrowDataType;
use async_trait::async_trait;
use datafusion::arrow::array::{Int64Array, Int64Builder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::{Session, TableFunctionImpl};
use datafusion::common::{plan_err, ScalarValue};
use datafusion::datasource::memory::MemorySourceConfig;
use datafusion::datasource::TableProvider;
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::SessionState;
use datafusion::logical_expr::function::AccumulatorArgs;
use datafusion::logical_expr::{create_udaf, Accumulator};
use datafusion::logical_expr::{ColumnarValue, Volatility};
use datafusion::logical_expr::{Expr, TableType};
use datafusion::prelude::SessionContext;
use datafusion::prelude::*;
use futures::executor::block_on;
use std::sync::Arc;
use tokio::task::block_in_place;

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
        let relname =
            if let Some(Expr::Literal(ScalarValue::Utf8(Some(ref s)), _)) = exprs.first() {
            s.clone()
        } else {
            return plan_err!("regclass_oid requires one string argument");
        };
        let schema = Arc::new(Schema::new(vec![Field::new("oid", DataType::Int64, true)]));
        Ok(Arc::new(RegClassOidTable { schema, relname }))
    }
}

/// Register `oid(text)` which looks up a table OID from `pg_class`.
pub fn register_scalar_regclass_oid(ctx: &SessionContext) -> Result<()> {
    let ctx_arc = Arc::new(ctx.clone());

    let fn_ = Arc::new(move |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        match &args[0] {
            ColumnarValue::Scalar(ScalarValue::Utf8(Some(name))) => {
                let sql = format!(
                    "SELECT oid FROM pg_catalog.pg_class WHERE relname = '{}'",
                    name.replace('\'', "''")
                );

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
                            } else if let Some(arr) =
                                col.as_any().downcast_ref::<arrow::array::Int32Array>()
                            {
                                if arr.is_null(0) {
                                    Ok(None)
                                } else {
                                    Ok(Some(arr.value(0) as i64))
                                }
                            } else {
                                Ok(None)
                            }
                        }
                    })
                })?;

                Ok(ColumnarValue::Scalar(ScalarValue::Int64(opt)))
            }
            ColumnarValue::Scalar(ScalarValue::Utf8(None)) => {
                Ok(ColumnarValue::Scalar(ScalarValue::Int64(None)))
            }
            ColumnarValue::Array(arr) => {
                let arr = as_string_array(arr);
                let mut builder = Int64Builder::with_capacity(arr.len());
                for i in 0..arr.len() {
                    if arr.is_null(i) {
                        builder.append_null();
                        continue;
                    }
                    let name = arr.value(i);
                    let sql = format!(
                        "SELECT oid FROM pg_catalog.pg_class WHERE relname = '{}'",
                        name.replace('\'', "''")
                    );
                    let opt: Option<i64> = block_in_place(|| {
                        block_on(async {
                            let batches = ctx_arc.sql(&sql).await?.collect().await?;
                            if batches.is_empty() || batches[0].num_rows() == 0 {
                                Ok::<Option<i64>, DataFusionError>(None)
                            } else {
                                let col = batches[0].column(0);
                                if let Some(a) = col.as_any().downcast_ref::<Int64Array>() {
                                    if a.is_null(0) {
                                        Ok(None)
                                    } else {
                                        Ok(Some(a.value(0)))
                                    }
                                } else if let Some(a) =
                                    col.as_any().downcast_ref::<arrow::array::Int32Array>()
                                {
                                    if a.is_null(0) {
                                        Ok(None)
                                    } else {
                                        Ok(Some(a.value(0) as i64))
                                    }
                                } else {
                                    Ok(None)
                                }
                            }
                        })
                    })?;
                    if let Some(v) = opt {
                        builder.append_value(v);
                    } else {
                        builder.append_null();
                    }
                }
                Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
            }
            _ => plan_err!("oid expects text"),
        }
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

/// Register `pg_tablespace_location(oid)` which currently always
/// returns NULL as tablespaces are not implemented.
pub fn register_scalar_pg_tablespace_location(ctx: &SessionContext) -> Result<()> {
    // TODO: this always returns empty string for now.
    //   If there is a db supporting tablespaces, this should be done correctly.
    let ctx_arc = Arc::new(ctx.clone());

    let udf = create_udf(
        "pg_tablespace_location",
        vec![ArrowDataType::Utf8],
        ArrowDataType::Utf8,
        Volatility::Immutable,
        std::sync::Arc::new(move |_args| Ok(ColumnarValue::Scalar(ScalarValue::Utf8(None)))),
    )
    .with_aliases(["pg_catalog.pg_tablespace_location"]);
    ctx_arc.register_udf(udf);
    Ok(())
}

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

/// Register a simplified `format_type(oid, typmod)` UDF that produces a
/// human readable type name for common built-in types.
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
                let s = format_type_string(
                    oids.value(i),
                    if mods.is_null(i) {
                        None
                    } else {
                        Some(mods.value(i))
                    },
                );
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

    let udf = create_udf(
        "pg_catalog.format_type",
        vec![ArrowDataType::Int64, ArrowDataType::Int64],
        ArrowDataType::Utf8,
        Volatility::Immutable,
        Arc::new(fun),
    );
    ctx_arc.register_udf(udf);

    Ok(())
}

// pub async fn register_scalar_format_type_with_lookup(ctx: &SessionContext) -> Result<()> {
//     use arrow::array::{ArrayRef, Int32Array, StringArray, StringBuilder};
//     use arrow::datatypes::DataType;
//     use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
//     use std::sync::Arc;

//     // Build a HashMap<oid,i32 -> typname> once
//     let mut map = std::collections::HashMap::<i32, String>::new();
//     if let Some(tbl) = ctx.table("pg_catalog.pg_type") {
//         let batches = tbl.collect().await?;
//         for b in &batches {
//             let oid = b
//                 .column_by_name("oid")
//                 .and_then(|c| c.as_any().downcast_ref::<Int32Array>())
//                 .unwrap();
//             let name = b
//                 .column_by_name("typname")
//                 .and_then(|c| c.as_any().downcast_ref::<StringArray>())
//                 .unwrap();
//             for i in 0..b.num_rows() {
//                 if !oid.is_null(i) && !name.is_null(i) {
//                     map.insert(oid.value(i), name.value(i).to_string());
//                 }
//             }
//         }
//     }

//     // closure used by the UDF
//     let fun = Arc::new(move |args: &[ColumnarValue]| -> Result<ColumnarValue> {
//         let oid = match &args[0] {
//             ColumnarValue::Scalar(ScalarValue::Int32(Some(v))) => *v,
//             ColumnarValue::Array(arr) => {
//                 let a = arr.as_any().downcast_ref::<Int32Array>().unwrap();
//                 if a.is_null(0) { 0 } else { a.value(0) }
//             }
//             _ => 0,
//         };
//         let typname = map.get(&oid).cloned().unwrap_or_else(|| "text".into());
//         Ok(ColumnarValue::Scalar(ScalarValue::Utf8(Some(typname))))
//     });

//     ctx.register_udf(create_udf(
//         "pg_catalog.format_type",
//         vec![DataType::Int32, DataType::Int32],
//         DataType::Utf8,
//         Volatility::Stable,
//         fun,
//     ));
//     Ok(())
// }

/// Implement a basic `pg_get_expr` that simply returns the input
/// expression text without evaluation.
pub fn register_scalar_pg_get_expr(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{cast::as_string_array, ArrayRef, StringBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
        Volatility,
    };
    use std::sync::Arc;

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct PgGetExpr {
        sig: Signature,
    }

    impl PgGetExpr {
        fn new() -> Self {
            Self {
                sig: Signature::one_of(
                    vec![
                        TypeSignature::Exact(vec![DataType::Utf8, DataType::Int64]),
                        TypeSignature::Exact(vec![
                            DataType::Utf8,
                            DataType::Int64,
                            DataType::Boolean,
                        ]),
                    ],
                    Volatility::Immutable,
                ),
            }
        }
    }

    impl ScalarUDFImpl for PgGetExpr {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn name(&self) -> &str {
            "pg_catalog.pg_get_expr"
        }
        fn signature(&self) -> &Signature {
            &self.sig
        }
        fn return_type(&self, _t: &[DataType]) -> Result<DataType> {
            Ok(DataType::Utf8)
        }

        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            let arrays = ColumnarValue::values_to_arrays(&args.args)?; // borrow as slice
            let exprs = as_string_array(&arrays[0]); // need the ?
            let mut b = StringBuilder::with_capacity(exprs.len(), 32 * exprs.len());
            for i in 0..exprs.len() {
                if exprs.is_null(i) {
                    b.append_null();
                } else {
                    b.append_value(exprs.value(i));
                }
            }
            Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
        }
    }

    let udf = ScalarUDF::new_from_impl(PgGetExpr::new()).with_aliases(["pg_get_expr"]);
    ctx.register_udf(udf);
    Ok(())
}

/// Stub implementation of `pg_get_partkeydef` that always returns NULL.
pub fn register_scalar_pg_get_partkeydef(ctx: &SessionContext) -> Result<()> {
    let ctx_arc = Arc::new(ctx.clone());
    let fun = |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let arrays = ColumnarValue::values_to_arrays(args)?;
        let oids = as_int64_array(&arrays[0])?;
        let mut builder = StringBuilder::new();
        for _ in 0..oids.len() {
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

/// Placeholder for `pg_get_statisticsobjdef_columns` which currently
/// returns NULL for all rows.
pub fn register_pg_get_statisticsobjdef_columns(ctx: &SessionContext) -> Result<()> {
    let ctx_arc = Arc::new(ctx.clone());
    let fun = |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let arrays = ColumnarValue::values_to_arrays(args)?;
        let oids = as_int64_array(&arrays[0])?;
        let mut builder = StringBuilder::new();
        for _ in 0..oids.len() {
            builder.append_null();
        }
        Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
    };
    let udf = create_udf(
        "pg_catalog.pg_get_statisticsobjdef_columns",
        vec![ArrowDataType::Int64],
        ArrowDataType::Utf8,
        Volatility::Immutable,
        Arc::new(fun),
    );
    ctx_arc.register_udf(udf);
    Ok(())
}

/// Compatibility stub for `pg_relation_is_publishable` which always
/// returns `true`.
pub fn register_pg_relation_is_publishable(ctx: &SessionContext) -> Result<()> {
    let ctx_arc = Arc::new(ctx.clone());
    for dt in [ArrowDataType::Int64, ArrowDataType::Utf8] {
        let fun = |_args: &[ColumnarValue]| -> Result<ColumnarValue> {
            Ok(ColumnarValue::Scalar(ScalarValue::Boolean(Some(true))))
        };
        let udf = create_udf(
            "pg_catalog.pg_relation_is_publishable",
            vec![dt.clone()],
            ArrowDataType::Boolean,
            Volatility::Immutable,
            Arc::new(fun),
        );
        ctx_arc.register_udf(udf);
    }
    Ok(())
}

/// pg_catalog.has_database_privilege(database, text) -> bool
///
/// Compatibility stub that always returns `true`.
pub fn register_has_database_privilege(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, BooleanBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let len = match args.get(0) {
            Some(ColumnarValue::Array(a)) => a.len(),
            _ => 1,
        };
        let mut b = BooleanBuilder::with_capacity(len);
        for _ in 0..len {
            b.append_value(true);
        }
        Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
    };

    for dt in [DataType::Int32, DataType::Int64, DataType::Utf8] {
        let udf = create_udf(
            "pg_catalog.has_database_privilege",
            vec![dt.clone(), DataType::Utf8],
            DataType::Boolean,
            Volatility::Stable,
            Arc::new(fun),
        )
        .with_aliases(["has_database_privilege"]);
        ctx.register_udf(udf);
    }
    Ok(())
}

/// pg_catalog.has_schema_privilege(schema, text) -> bool
///
/// Compatibility stub that always returns `true`.
pub fn register_has_schema_privilege(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, BooleanBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let len = match args.get(0) {
            Some(ColumnarValue::Array(a)) => a.len(),
            _ => 1,
        };
        let mut b = BooleanBuilder::with_capacity(len);
        for _ in 0..len {
            b.append_value(true);
        }
        Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
    };

    for dt in [DataType::Int32, DataType::Int64, DataType::Utf8] {
        let udf = create_udf(
            "pg_catalog.has_schema_privilege",
            vec![dt.clone(), DataType::Utf8],
            DataType::Boolean,
            Volatility::Stable,
            Arc::new(fun),
        )
        .with_aliases(["has_schema_privilege"]);
        ctx.register_udf(udf);
    }
    Ok(())
}

/// Register `current_schema()` returning the constant `public`.
pub fn register_current_schema(
    ctx: &SessionContext,
    get_current_schemas: Arc<dyn Fn(&SessionContext) -> Vec<String> + Send + Sync>,
) -> Result<()> {
    let ctx_arc = Arc::new(ctx.clone());
    let get_current_schemas = get_current_schemas.clone();

    let udf = create_udf(
        "current_schema",
        vec![],
        ArrowDataType::Utf8,
        Volatility::Immutable,
        {
            let ctx = ctx_arc.clone();
            let get = get_current_schemas.clone();
            std::sync::Arc::new(move |_args| {
                let schema = (get)(&ctx).into_iter().next().unwrap_or_default();
                Ok(ColumnarValue::Scalar(ScalarValue::Utf8(Some(schema))))
            })
        },
    )
    .with_aliases(["pg_catalog.current_schema"]);
    ctx_arc.register_udf(udf);
    Ok(())
}

/// Register `current_schemas(boolean)` returning `[pg_catalog, public]`.
pub fn register_current_schemas(
    ctx: &SessionContext,
    get_current_schemas: Arc<dyn Fn(&SessionContext) -> Vec<String> + Send + Sync>,
) -> Result<()> {
    use arrow::array::{ArrayRef, ListBuilder, StringBuilder};
    use arrow::datatypes::{DataType, Field};
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let ctx_arc = Arc::new(ctx.clone());
    let get_current_schemas = get_current_schemas.clone();

    let fun = move |_args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let schemas = (get_current_schemas)(&ctx_arc);
        let mut builder = ListBuilder::new(StringBuilder::new());
        for s in schemas {
            builder.values().append_value(s);
        }
        builder.append(true);
        Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
    };

    let list_dt = DataType::List(Arc::new(Field::new("item", DataType::Utf8, true)));
    let udf = create_udf(
        "current_schemas",
        vec![DataType::Boolean],
        list_dt.clone(),
        Volatility::Stable,
        Arc::new(fun),
    )
    .with_aliases(["pg_catalog.current_schemas"]);
    ctx.register_udf(udf);
    Ok(())
}

/// Stub for `pg_table_is_visible` which always reports `true`.
pub fn register_scalar_pg_table_is_visible(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, BooleanBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let len = match &args[0] {
            ColumnarValue::Array(a) => a.len(),
            ColumnarValue::Scalar(_) => 1,
        };
        let mut b = BooleanBuilder::with_capacity(len);
        for _ in 0..len {
            b.append_value(true);
        }
        Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
    };

    ctx.register_udf(create_udf(
        "pg_catalog.pg_table_is_visible",
        vec![DataType::Int32],
        DataType::Boolean,
        Volatility::Stable,
        Arc::new(fun),
    ));
    Ok(())
}

/// Register `pg_get_userbyid(oid)` returning NULL for now.
pub fn register_scalar_pg_get_userbyid(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, StringBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let len = match &args[0] {
            ColumnarValue::Array(a) => a.len(),
            ColumnarValue::Scalar(_) => 1,
        };
        let mut b = StringBuilder::with_capacity(len, 8 * len);
        for _ in 0..len {
            b.append_value("postgres");
        }
        Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
    };

    ctx.register_udf(create_udf(
        "pg_catalog.pg_get_userbyid",
        vec![DataType::Int32], // one OID argument
        DataType::Utf8,
        Volatility::Stable,
        Arc::new(fun),
    ));
    Ok(())
}

/// Register `pg_encoding_to_char(int)` returning the encoding name as text.
pub fn register_scalar_pg_encoding_to_char(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, StringBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let len = match &args[0] {
            ColumnarValue::Array(a) => a.len(),
            ColumnarValue::Scalar(_) => 1,
        };
        let mut b = StringBuilder::with_capacity(len, 8 * len);
        for _ in 0..len {
            b.append_value("UTF8");
        }
        Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
    };

    ctx.register_udf(create_udf(
        "pg_catalog.pg_encoding_to_char",
        vec![DataType::Int32], // single OID argument
        DataType::Utf8,
        Volatility::Stable,
        Arc::new(fun),
    ));
    Ok(())
}

/// Register the `array_to_string` function used for array formatting.
pub fn register_scalar_array_to_string(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{
        Array, ArrayRef, GenericListArray, OffsetSizeTrait, StringArray, StringBuilder,
    };
    use arrow::datatypes::{DataType, Field};
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
        Volatility,
    };
    use std::sync::Arc;

    fn build_list<O: OffsetSizeTrait>(
        arr: ArrayRef,
        delim: &str,
        null_rep: &Option<String>,
    ) -> Result<ColumnarValue> {
        let l = arr.as_any().downcast_ref::<GenericListArray<O>>().unwrap();
        let strings = l.values().as_any().downcast_ref::<StringArray>().unwrap();
        let offsets = l.value_offsets();
        let mut out = StringBuilder::with_capacity(l.len(), 32 * l.len());
        for i in 0..l.len() {
            if l.is_null(i) {
                out.append_null();
                continue;
            }
            let mut parts = Vec::new();
            let start = offsets[i].to_usize().unwrap();
            let end = offsets[i + 1].to_usize().unwrap();
            for idx in start..end {
                if strings.is_null(idx) {
                    if let Some(ref nr) = null_rep {
                        parts.push(nr.as_str())
                    }
                } else {
                    parts.push(strings.value(idx))
                }
            }
            out.append_value(parts.join(delim));
        }
        Ok(ColumnarValue::Array(Arc::new(out.finish()) as ArrayRef))
    }

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct ArrayToString {
        sig: Signature,
    }

    impl ArrayToString {
        fn new() -> Self {
            let list = DataType::List(Arc::new(Field::new("item", DataType::Utf8, true)));
            Self {
                sig: Signature::one_of(
                    vec![
                        TypeSignature::Exact(vec![list.clone(), DataType::Utf8]),
                        TypeSignature::Exact(vec![list, DataType::Utf8, DataType::Utf8]),
                        //
                        TypeSignature::Exact(vec![DataType::Utf8, DataType::Utf8]),
                        TypeSignature::Exact(vec![DataType::Utf8, DataType::Utf8, DataType::Utf8]),
                    ],
                    Volatility::Stable,
                ),
            }
        }
    }

    impl ScalarUDFImpl for ArrayToString {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn name(&self) -> &str {
            "pg_catalog.array_to_string"
        }
        fn signature(&self) -> &Signature {
            &self.sig
        }
        fn return_type(&self, _: &[DataType]) -> Result<DataType> {
            Ok(DataType::Utf8)
        }

        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            let delim = match &args.args[1] {
                ColumnarValue::Scalar(ScalarValue::Utf8(Some(s))) => s.clone(),
                _ => "".to_string(),
            };
            let null_rep = if args.args.len() == 3 {
                match &args.args[2] {
                    ColumnarValue::Scalar(ScalarValue::Utf8(opt)) => opt.clone(),
                    _ => None,
                }
            } else {
                None
            };

            match &args.args[0] {
                ColumnarValue::Array(a) if a.as_any().is::<GenericListArray<i32>>() => {
                    build_list::<i32>(a.clone(), &delim, &null_rep)
                }
                ColumnarValue::Array(a) if a.as_any().is::<GenericListArray<i64>>() => {
                    build_list::<i64>(a.clone(), &delim, &null_rep)
                }
                ColumnarValue::Array(a) if a.as_any().is::<StringArray>() => {
                    let sa = a.as_any().downcast_ref::<StringArray>().unwrap();
                    let mut b = StringBuilder::with_capacity(sa.len(), 32 * sa.len());
                    for i in 0..sa.len() {
                        if sa.is_null(i) {
                            b.append_null();
                        } else {
                            b.append_value(sa.value(i));
                        }
                    }
                    Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
                }
                ColumnarValue::Scalar(ScalarValue::List(list)) => {
                    if list.is_null(0) {
                        return Ok(ColumnarValue::Scalar(ScalarValue::Utf8(None)));
                    }

                    let elem = list.value(0);
                    let sa = elem.as_any().downcast_ref::<StringArray>().unwrap();

                    let mut parts = Vec::new();
                    for i in 0..sa.len() {
                        if sa.is_null(i) {
                            if let Some(ref nr) = null_rep {
                                parts.push(nr.clone());
                            }
                        } else {
                            parts.push(sa.value(i).to_string());
                        }
                    }
                    let joined = parts.join(&delim);
                    Ok(ColumnarValue::Scalar(ScalarValue::Utf8(Some(joined))))
                }
                ColumnarValue::Scalar(ScalarValue::Utf8(Some(s))) => {
                    let mut b = StringBuilder::with_capacity(1, s.len());
                    b.append_value(s);
                    Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
                }
                _ => Err(DataFusionError::Plan(
                    "unsupported argument to array_to_string".into(),
                )),
            }
        }

        fn is_nullable(
            &self,
            _args: &[Expr],
            _schema: &dyn datafusion::common::ExprSchema,
        ) -> bool {
            true
        }

        fn aliases(&self) -> &[String] {
            &[]
        }

        fn simplify(
            &self,
            args: Vec<Expr>,
            _info: &dyn datafusion::logical_expr::simplify::SimplifyInfo,
        ) -> Result<datafusion::logical_expr::simplify::ExprSimplifyResult> {
            Ok(datafusion::logical_expr::simplify::ExprSimplifyResult::Original(args))
        }

        fn short_circuits(&self) -> bool {
            false
        }

        fn evaluate_bounds(
            &self,
            _input: &[&datafusion::logical_expr::interval_arithmetic::Interval],
        ) -> Result<datafusion::logical_expr::interval_arithmetic::Interval> {
            // We cannot assume the input datatype is the same of output type.
            datafusion::logical_expr::interval_arithmetic::Interval::make_unbounded(&DataType::Null)
        }

        fn propagate_constraints(
            &self,
            _interval: &datafusion::logical_expr::interval_arithmetic::Interval,
            _inputs: &[&datafusion::logical_expr::interval_arithmetic::Interval],
        ) -> Result<Option<Vec<datafusion::logical_expr::interval_arithmetic::Interval>>> {
            Ok(Some(std::vec![]))
        }

        fn output_ordering(
            &self,
            inputs: &[datafusion::logical_expr::sort_properties::ExprProperties],
        ) -> Result<datafusion::logical_expr::sort_properties::SortProperties> {
            if !self.preserves_lex_ordering(inputs)? {
                return Ok(datafusion::logical_expr::sort_properties::SortProperties::Unordered);
            }

            let Some(first_order) = inputs.first().map(|p| &p.sort_properties) else {
                return Ok(datafusion::logical_expr::sort_properties::SortProperties::Singleton);
            };

            if inputs
                .iter()
                .skip(1)
                .all(|input| &input.sort_properties == first_order)
            {
                Ok(*first_order)
            } else {
                Ok(datafusion::logical_expr::sort_properties::SortProperties::Unordered)
            }
        }

        fn preserves_lex_ordering(
            &self,
            _inputs: &[datafusion::logical_expr::sort_properties::ExprProperties],
        ) -> Result<bool> {
            Ok(false)
        }

        fn coerce_types(&self, _arg_types: &[DataType]) -> Result<Vec<DataType>> {
            datafusion::common::not_impl_err!(
                "Function {} does not implement coerce_types",
                self.name()
            )
        }

        fn documentation(&self) -> Option<&datafusion::logical_expr::Documentation> {
            None
        }
    }

    ctx.register_udf(ScalarUDF::new_from_impl(ArrayToString::new()));
    Ok(())
}

/// Register the helper function `pg_get_one` used for planner rewrites.
pub fn register_pg_get_one(ctx: &SessionContext) -> Result<()> {
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, Volatility,
    };

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct PgGetOne {
        sig: Signature,
    }

    impl PgGetOne {
        fn new() -> Self {
            Self {
                sig: Signature::any(1, Volatility::Stable),
            }
        }
    }

    impl ScalarUDFImpl for PgGetOne {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn name(&self) -> &str {
            "pg_get_one"
        }
        fn signature(&self) -> &Signature {
            &self.sig
        }
        fn return_type(&self, arg_types: &[DataType]) -> Result<DataType> {
            Ok(arg_types.get(0).cloned().unwrap_or(DataType::Null))
        }
        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            Ok(args.args.into_iter().next().unwrap())
        }
    }

    let udf = ScalarUDF::new_from_impl(PgGetOne::new()).with_aliases(["pg_catalog.pg_get_one"]);
    ctx.register_udf(udf);
    Ok(())
}

#[derive(Debug)]
struct ArrayCollector {
    vals: Vec<ScalarValue>,
    dt: DataType,
}

impl ArrayCollector {
    fn new(dt: DataType) -> Self {
        Self {
            vals: Vec::new(),
            dt,
        }
    }
}

impl Accumulator for ArrayCollector {
    // ---------- state ----------
    fn state(&mut self) -> Result<Vec<ScalarValue>> {
        let arr = ScalarValue::new_list_from_iter(
            self.vals.clone().into_iter(),
            &self.dt,
            /* contains_null = */ true,
        );
        Ok(vec![ScalarValue::List(arr)])
    }

    // ---------- input tuples ----------
    fn update_batch(&mut self, values: &[ArrayRef]) -> Result<()> {
        for i in 0..values[0].len() {
            self.vals.push(ScalarValue::try_from_array(&values[0], i)?);
        }
        Ok(())
    }

    // ---------- merge partial states ----------
    fn merge_batch(&mut self, states: &[ArrayRef]) -> Result<()> {
        for row in 0..states[0].len() {
            let sv = ScalarValue::try_from_array(&states[0], row)?;
            if let ScalarValue::List(arc) = sv {
                let list = arc.as_ref();
                for idx in 0..list.len() {
                    let inner = list.value(idx);
                    for j in 0..inner.len() {
                        self.vals.push(ScalarValue::try_from_array(&inner, j)?);
                    }
                }
            }
        }
        Ok(())
    }

    // ---------- final result ----------
    fn evaluate(&mut self) -> Result<ScalarValue> {
        let arr = ScalarValue::new_list_from_iter(
            std::mem::take(&mut self.vals).into_iter(),
            &self.dt,
            true,
        );
        Ok(ScalarValue::List(arr))
    }

    // ---------- memory footprint ----------
    fn size(&self) -> usize {
        // very rough – 24 bytes per value
        24 * self.vals.len()
    }
}

/// Register the `array_agg` aggregate function and its pg_catalog alias.
pub fn register_array_agg(ctx: &SessionContext) -> Result<()> {
    use datafusion_functions_aggregate::array_agg::array_agg_udaf;
    let udaf = array_agg_udaf();
    ctx.register_udaf((*udaf).clone());
    ctx.register_udaf((*udaf).clone().with_aliases(["pg_catalog.array_agg"]));
    Ok(())
}

/// Register the table function `pg_get_array` used to materialize
/// results of `ARRAY(subquery)` rewrites.
pub fn register_pg_get_array(ctx: &SessionContext) -> Result<()> {
    use arrow::datatypes::{DataType, Field};
    use datafusion::logical_expr::Volatility;
    use std::sync::Arc;

    // factory that builds a new accumulator for the concrete argument type
    let factory = |args: AccumulatorArgs| -> Result<Box<dyn Accumulator>> {
        // the datatype of the *first* argument as planned for this agg-call
        let dt = args
            .exprs
            .first() // pg_get_array takes exactly one arg
            .ok_or_else(|| DataFusionError::Internal("pg_get_array expects one argument".into()))?
            .data_type(args.schema)?; // ask the expression for its type

        Ok(Box::new(ArrayCollector::new(dt)))
    };

    let element_dt = DataType::Utf8; // we only expose UTF-8 today
    let list_dt = DataType::List(Arc::new(Field::new("item", element_dt.clone(), true)));

    let udaf = create_udaf(
        "pg_get_array",            // name
        vec![element_dt],          // input types
        Arc::new(list_dt.clone()), // return type
        Volatility::Immutable,     // volatility
        Arc::new(factory),         // accumulator factory
        Arc::new(vec![list_dt]),   // state type
    );

    ctx.register_udaf(udaf.clone());
    ctx.register_udaf(udaf.with_aliases(["pg_catalog.pg_get_array"]));
    Ok(())
}

/// Convert an oidvector stored as text into an array of BIGINT oids.
pub fn register_oidvector_to_array(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{as_string_array, ArrayRef, Int64Builder, ListBuilder};
    use arrow::datatypes::{DataType, Field};
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let arrays = ColumnarValue::values_to_arrays(args)?;
        let arr = as_string_array(&arrays[0]);

        let mut builder = ListBuilder::new(Int64Builder::new());
        for i in 0..arr.len() {
            if arr.is_null(i) {
                builder.append(false);
                continue;
            }
            let txt = arr.value(i);
            if !txt.trim().is_empty() {
                for tok in txt.split_whitespace() {
                    let oid: i64 = tok.parse().map_err(|_| {
                        DataFusionError::Execution(format!("invalid oid value '{}'", tok))
                    })?;
                    builder.values().append_value(oid);
                }
            }
            builder.append(true);
        }
        Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
    };

    let list_dt = DataType::List(Arc::new(Field::new("item", DataType::Int64, true)));
    let udf = create_udf(
        "oidvector_to_array",
        vec![DataType::Utf8],
        list_dt.clone(),
        Volatility::Immutable,
        Arc::new(fun),
    )
    .with_aliases(["pg_catalog.oidvector_to_array"]);
    ctx.register_udf(udf);
    Ok(())
}

#[derive(Debug)]
struct PostmasterStartTimeTable {
    schema: SchemaRef,
    ts: i64,
}

#[async_trait]
impl TableProvider for PostmasterStartTimeTable {
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
        _session: &dyn Session,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> Result<Arc<dyn datafusion::physical_plan::ExecutionPlan>> {
        let arr = TimestampMicrosecondArray::from(vec![Some(self.ts)]);
        let batch = RecordBatch::try_new(self.schema.clone(), vec![Arc::new(arr)])?;
        Ok(MemorySourceConfig::try_new_exec(
            &[vec![batch]],
            self.schema.clone(),
            projection.cloned(),
        )?)
    }
}

#[derive(Debug)]
struct PostmasterStartTimeFunc {
    schema: SchemaRef,
    ts: i64,
}

impl TableFunctionImpl for PostmasterStartTimeFunc {
    fn call(&self, _exprs: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        Ok(Arc::new(PostmasterStartTimeTable {
            schema: self.schema.clone(),
            ts: self.ts,
        }))
    }
}

/// Register `pg_postmaster_start_time()` returning the current system
/// time. Both a table function and a scalar variant are installed.
pub fn register_pg_postmaster_start_time(ctx: &SessionContext) -> Result<()> {
    use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as i64;

    let schema = Arc::new(Schema::new(vec![Field::new(
        "pg_postmaster_start_time",
        DataType::Timestamp(TimeUnit::Microsecond, None),
        true,
    )]));
    ctx.register_udtf(
        "pg_postmaster_start_time",
        Arc::new(PostmasterStartTimeFunc {
            schema: schema.clone(),
            ts,
        }),
    );
    ctx.register_udtf(
        "pg_catalog.pg_postmaster_start_time",
        Arc::new(PostmasterStartTimeFunc {
            schema: schema.clone(),
            ts,
        }),
    );
    let fun = {
        let t = ts;
        Arc::new(move |_args: &[ColumnarValue]| -> Result<ColumnarValue> {
            Ok(ColumnarValue::Scalar(ScalarValue::TimestampMicrosecond(
                Some(t),
                None,
            )))
        })
    };
    let ty = DataType::Timestamp(TimeUnit::Microsecond, None);
    ctx.register_udf(create_udf(
        "pg_postmaster_start_time",
        vec![],
        ty.clone(),
        Volatility::Stable,
        fun.clone(),
    ));
    ctx.register_udf(create_udf(
        "pg_catalog.pg_postmaster_start_time",
        vec![],
        ty,
        Volatility::Stable,
        fun,
    ));
    Ok(())
}

/// Register a trivial `pg_age` implementation used by some catalog views.
pub fn register_scalar_pg_age(ctx: &SessionContext) -> Result<()> {
    use arrow::datatypes::DataType;
    use datafusion::common::ScalarValue;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    // one closure – we don’t care about the argument, just return 1
    let fun = |_args: &[ColumnarValue]| -> Result<ColumnarValue> {
        Ok(ColumnarValue::Scalar(ScalarValue::Int64(Some(1))))
    };

    // accept BIGINT *or* TEXT
    for dt in [DataType::Int64, DataType::Utf8] {
        let udf = create_udf(
            "pg_catalog.age", // ← exact name Postgres uses
            vec![dt],
            DataType::Int64, // always returns BIGINT
            Volatility::Stable,
            Arc::new(fun),
        );
        ctx.register_udf(udf);
    }
    Ok(())
}

/// pg_catalog.pg_is_in_recovery() → BOOL
///
/// We don’t do physical recovery, so just return `false`.
pub fn register_scalar_pg_is_in_recovery(ctx: &SessionContext) -> Result<()> {
    use arrow::datatypes::DataType;
    use datafusion::common::ScalarValue;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |_args: &[ColumnarValue]| -> Result<ColumnarValue> {
        Ok(ColumnarValue::Scalar(ScalarValue::Boolean(Some(false))))
    };

    // zero-argument signature
    let udf = create_udf(
        "pg_catalog.pg_is_in_recovery", // full, schema-qualified name
        vec![],                         // no arguments
        DataType::Boolean,              // returns BOOL
        Volatility::Stable,             // it never changes inside a session
        Arc::new(fun),
    );
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.txid_current()  →  BIGINT
///
/// We don’t run a real MVCC engine, so we fake a transaction counter that
/// ticks up every time the function is invoked.
pub fn register_scalar_txid_current(ctx: &SessionContext) -> Result<()> {
    use arrow::datatypes::DataType;
    use datafusion::common::ScalarValue;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use once_cell::sync::Lazy;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    // global ever-increasing counter (starts at 1 just for fun)
    static NEXT_TXID: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(1));

    let fun = |_args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let val = NEXT_TXID.fetch_add(1, Ordering::SeqCst) as i64; // BIGINT
        Ok(ColumnarValue::Scalar(ScalarValue::Int64(Some(val))))
    };

    let udf = create_udf(
        "pg_catalog.txid_current", // full, schema-qualified name
        vec![],                    // zero arguments
        DataType::Int64,           // returns BIGINT
        Volatility::Stable,        // stays the same within a single statement
        Arc::new(fun),
    );
    ctx.register_udf(udf);

    // also expose an unqualified name
    ctx.register_udf(create_udf(
        "txid_current",
        vec![],
        DataType::Int64,
        Volatility::Stable,
        Arc::new(fun),
    ));

    Ok(())
}

/// pg_catalog.quote_ident(text) → text
///
/// Minimal implementation that simply returns the input verbatim.
pub fn register_quote_ident(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{as_string_array, ArrayRef, StringBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let arrays = ColumnarValue::values_to_arrays(args)?;
        let arr = as_string_array(&arrays[0]);
        let mut b = StringBuilder::with_capacity(arr.len(), arr.len() * 4);
        for i in 0..arr.len() {
            if arr.is_null(i) {
                b.append_null();
            } else {
                b.append_value(arr.value(i));
            }
        }
        Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
    };

    let udf = create_udf(
        "pg_catalog.quote_ident",
        vec![DataType::Utf8],
        DataType::Utf8,
        Volatility::Stable,
        Arc::new(fun),
    )
    .with_aliases(["quote_ident"]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.translate(text, text, text) → text
///
/// Implements a basic character translation similar to PostgreSQL's translate.
pub fn register_translate(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{as_string_array, ArrayRef, StringBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let arrays = ColumnarValue::values_to_arrays(args)?;
        let src = as_string_array(&arrays[0]);
        let from = as_string_array(&arrays[1]);
        let to = as_string_array(&arrays[2]);
        let mut b = StringBuilder::with_capacity(src.len(), src.len() * 4);
        for i in 0..src.len() {
            if src.is_null(i) || from.is_null(i) || to.is_null(i) {
                b.append_null();
                continue;
            }
            let s = src.value(i);
            let f = from.value(i);
            let t = to.value(i);
            let mut out = String::with_capacity(s.len());
            for ch in s.chars() {
                if let Some(pos) = f.chars().position(|c| c == ch) {
                    if let Some(rep) = t.chars().nth(pos) {
                        out.push(rep);
                    }
                } else {
                    out.push(ch);
                }
            }
            b.append_value(out);
        }
        Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
    };

    let udf = create_udf(
        "pg_catalog.translate",
        vec![DataType::Utf8, DataType::Utf8, DataType::Utf8],
        DataType::Utf8,
        Volatility::Stable,
        Arc::new(fun),
    )
    .with_aliases(["translate"]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.upper(text) -> text
///
/// Simple uppercase implementation.
pub fn register_upper(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{as_string_array, ArrayRef, StringBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let arrays = ColumnarValue::values_to_arrays(args)?;
        let arr = as_string_array(&arrays[0]);
        let mut b = StringBuilder::with_capacity(arr.len(), arr.len() * 4);
        for i in 0..arr.len() {
            if arr.is_null(i) {
                b.append_null();
            } else {
                b.append_value(arr.value(i).to_uppercase());
            }
        }
        Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
    };

    let udf = create_udf(
        "pg_catalog.upper",
        vec![DataType::Utf8],
        DataType::Utf8,
        Volatility::Stable,
        Arc::new(fun),
    )
    .with_aliases(["upper"]);
    ctx.register_udf(udf);
    Ok(())
}

/// version() -> text
///
/// Returns a PostgreSQL-style server version string.
pub fn register_version_fn(ctx: &SessionContext) -> Result<()> {
    use crate::server::SERVER_VERSION;
    use arrow::datatypes::DataType;
    use datafusion::common::ScalarValue;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |_args: &[ColumnarValue]| -> Result<ColumnarValue> {
        Ok(ColumnarValue::Scalar(ScalarValue::Utf8(Some(format!(
            "PostgreSQL {SERVER_VERSION}"
        )))))
    };

    let udf = create_udf(
        "version",
        vec![],
        DataType::Utf8,
        Volatility::Stable,
        Arc::new(fun),
    )
    .with_aliases(["pg_catalog.version"]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.pg_get_viewdef(oid [, bool]) → text
///
/// Returns NULL placeholder for now.
pub fn register_pg_get_viewdef(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, StringBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
        Volatility,
    };
    use std::sync::Arc;

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct PgGetViewDef {
        sig: Signature,
    }

    impl PgGetViewDef {
        fn new() -> Self {
            Self {
                sig: Signature::one_of(
                    vec![TypeSignature::Any(1), TypeSignature::Any(2)],
                    Volatility::Stable,
                ),
            }
        }
    }

    impl ScalarUDFImpl for PgGetViewDef {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn name(&self) -> &str {
            "pg_catalog.pg_get_viewdef"
        }
        fn signature(&self) -> &Signature {
            &self.sig
        }
        fn return_type(&self, _t: &[DataType]) -> Result<DataType> {
            Ok(DataType::Utf8)
        }
        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            let len = match args.args.first() {
                Some(ColumnarValue::Array(a)) => a.len(),
                _ => 1,
            };
            let mut b = StringBuilder::with_capacity(len, len);
            for _ in 0..len {
                b.append_null();
            }
            Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
        }
    }

    let udf = ScalarUDF::new_from_impl(PgGetViewDef::new()).with_aliases(["pg_get_viewdef"]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.pg_get_function_arguments(oid) → text
pub fn register_pg_get_function_arguments(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, StringBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let len = match args.first() {
            Some(ColumnarValue::Array(a)) => a.len(),
            _ => 1,
        };
        let mut b = StringBuilder::with_capacity(len, len);
        for _ in 0..len {
            b.append_null();
        }
        Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
    };

    let udf = create_udf(
        "pg_catalog.pg_get_function_arguments",
        vec![DataType::Int64],
        DataType::Utf8,
        Volatility::Stable,
        Arc::new(fun),
    )
    .with_aliases(["pg_get_function_arguments"]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.pg_get_indexdef(oid) → text
pub fn register_pg_get_indexdef(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, StringBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let len = match args.first() {
            Some(ColumnarValue::Array(a)) => a.len(),
            _ => 1,
        };
        let mut b = StringBuilder::with_capacity(len, len);
        for _ in 0..len {
            b.append_null();
        }
        Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
    };

    let udf = create_udf(
        "pg_catalog.pg_get_indexdef",
        vec![DataType::Int64],
        DataType::Utf8,
        Volatility::Stable,
        Arc::new(fun),
    )
    .with_aliases(["pg_get_indexdef"]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.pg_get_function_result(oid) → text
pub fn register_pg_get_function_result(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, StringBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let len = match args.first() {
            Some(ColumnarValue::Array(a)) => a.len(),
            _ => 1,
        };
        let mut b = StringBuilder::with_capacity(len, len);
        for _ in 0..len {
            b.append_null();
        }
        Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
    };

    let udf = create_udf(
        "pg_catalog.pg_get_function_result",
        vec![DataType::Int64],
        DataType::Utf8,
        Volatility::Stable,
        Arc::new(fun),
    )
    .with_aliases(["pg_get_function_result"]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.pg_get_function_sqlbody(oid) → text
pub fn register_pg_get_function_sqlbody(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, StringBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let len = match args.first() {
            Some(ColumnarValue::Array(a)) => a.len(),
            _ => 1,
        };
        let mut b = StringBuilder::with_capacity(len, len);
        for _ in 0..len {
            b.append_null();
        }
        Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
    };

    let udf = create_udf(
        "pg_catalog.pg_get_function_sqlbody",
        vec![DataType::Int64],
        DataType::Utf8,
        Volatility::Stable,
        Arc::new(fun),
    )
    .with_aliases(["pg_get_function_sqlbody"]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.encode(bytea, text) -> text
///
/// Placeholder implementation returning NULL.
pub fn register_encode(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, StringBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let len = match args.first() {
            Some(ColumnarValue::Array(a)) => a.len(),
            _ => 1,
        };
        let mut b = StringBuilder::with_capacity(len, len);
        for _ in 0..len {
            b.append_null();
        }
        Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
    };

    let udf = create_udf(
        "pg_catalog.encode",
        vec![DataType::Binary, DataType::Utf8],
        DataType::Utf8,
        Volatility::Stable,
        Arc::new(fun),
    )
    .with_aliases(["encode"]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.pg_get_triggerdef(oid [, bool]) -> text
///
/// Returns NULL placeholder.
pub fn register_pg_get_triggerdef(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, StringBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
        Volatility,
    };
    use std::sync::Arc;

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct PgGetTriggerDef {
        sig: Signature,
    }

    impl PgGetTriggerDef {
        fn new() -> Self {
            Self {
                sig: Signature::one_of(
                    vec![TypeSignature::Any(1), TypeSignature::Any(2)],
                    Volatility::Stable,
                ),
            }
        }
    }

    impl ScalarUDFImpl for PgGetTriggerDef {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn name(&self) -> &str {
            "pg_catalog.pg_get_triggerdef"
        }
        fn signature(&self) -> &Signature {
            &self.sig
        }
        fn return_type(&self, _t: &[DataType]) -> Result<DataType> {
            Ok(DataType::Utf8)
        }
        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            let len = match args.args.first() {
                Some(ColumnarValue::Array(a)) => a.len(),
                _ => 1,
            };
            let mut b = StringBuilder::with_capacity(len, len);
            for _ in 0..len {
                b.append_null();
            }
            Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
        }
    }

    let udf = ScalarUDF::new_from_impl(PgGetTriggerDef::new()).with_aliases(["pg_get_triggerdef"]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.pg_get_ruledef(oid [, bool]) -> text
///
/// Returns NULL placeholder.
pub fn register_pg_get_ruledef(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, StringBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
        Volatility,
    };
    use std::sync::Arc;

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct PgGetRuleDef {
        sig: Signature,
    }

    impl PgGetRuleDef {
        fn new() -> Self {
            Self {
                sig: Signature::one_of(
                    vec![TypeSignature::Any(1), TypeSignature::Any(2)],
                    Volatility::Stable,
                ),
            }
        }
    }

    impl ScalarUDFImpl for PgGetRuleDef {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn name(&self) -> &str {
            "pg_catalog.pg_get_ruledef"
        }
        fn signature(&self) -> &Signature {
            &self.sig
        }
        fn return_type(&self, _t: &[DataType]) -> Result<DataType> {
            Ok(DataType::Utf8)
        }
        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            let len = match args.args.first() {
                Some(ColumnarValue::Array(a)) => a.len(),
                _ => 1,
            };
            let mut b = StringBuilder::with_capacity(len, len);
            for _ in 0..len {
                b.append_null();
            }
            Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
        }
    }

    let udf = ScalarUDF::new_from_impl(PgGetRuleDef::new()).with_aliases(["pg_get_ruledef"]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.pg_available_extension_versions() -> TABLE
///
/// Returns information about available extension versions. For now this
/// implementation returns an empty result set but exposes the expected
/// columns so queries referencing the function succeed.
pub fn register_pg_available_extension_versions(ctx: &SessionContext) -> Result<()> {
    use arrow::datatypes::{DataType, Field, Schema};
    use std::sync::Arc;

    let schema = Arc::new(Schema::new(vec![
        Field::new("name", DataType::Utf8, true),
        Field::new("version", DataType::Utf8, true),
        Field::new("superuser", DataType::Boolean, true),
        Field::new("trusted", DataType::Boolean, true),
        Field::new("relocatable", DataType::Boolean, true),
        Field::new("schema", DataType::Utf8, true),
        Field::new(
            "requires",
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
            true,
        ),
        Field::new("comment", DataType::Utf8, true),
    ]));

    #[derive(Debug)]
    struct TableFn {
        schema: SchemaRef,
    }

    #[async_trait]
    impl TableProvider for TableFn {
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
            _session: &dyn Session,
            projection: Option<&Vec<usize>>,
            _filters: &[Expr],
            _limit: Option<usize>,
        ) -> Result<Arc<dyn datafusion::physical_plan::ExecutionPlan>> {
            let batch = RecordBatch::new_empty(self.schema.clone());
            Ok(MemorySourceConfig::try_new_exec(
                &[vec![batch]],
                self.schema.clone(),
                projection.cloned(),
            )?)
        }
    }

    #[derive(Debug)]
    struct Func {
        schema: SchemaRef,
    }

    impl TableFunctionImpl for Func {
        fn call(&self, exprs: &[Expr]) -> Result<Arc<dyn TableProvider>> {
            if !exprs.is_empty() {
                return plan_err!("pg_available_extension_versions takes no arguments");
            }
            Ok(Arc::new(TableFn {
                schema: self.schema.clone(),
            }))
        }
    }

    ctx.register_udtf(
        "pg_available_extension_versions",
        Arc::new(Func {
            schema: schema.clone(),
        }),
    );
    ctx.register_udtf(
        "pg_catalog.pg_available_extension_versions",
        Arc::new(Func { schema }),
    );
    Ok(())
}

/// pg_catalog.pg_get_keywords() -> TABLE
///
/// Returns PostgreSQL keywords and their categories. For now this
/// implementation exposes the expected columns but returns an empty
/// result set so that tools relying on the function can execute
/// successfully.
pub fn register_pg_get_keywords(ctx: &SessionContext) -> Result<()> {
    use arrow::datatypes::{DataType, Field, Schema};
    use std::sync::Arc;

    let schema = Arc::new(Schema::new(vec![
        Field::new("word", DataType::Utf8, true),
        Field::new("catcode", DataType::Utf8, true),
        Field::new("catdesc", DataType::Utf8, true),
    ]));

    #[derive(Debug)]
    struct TableFn {
        schema: SchemaRef,
    }

    #[async_trait]
    impl TableProvider for TableFn {
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
            _session: &dyn Session,
            projection: Option<&Vec<usize>>,
            _filters: &[Expr],
            _limit: Option<usize>,
        ) -> Result<Arc<dyn datafusion::physical_plan::ExecutionPlan>> {
            let batch = RecordBatch::new_empty(self.schema.clone());
            Ok(MemorySourceConfig::try_new_exec(
                &[vec![batch]],
                self.schema.clone(),
                projection.cloned(),
            )?)
        }
    }

    #[derive(Debug)]
    struct Func {
        schema: SchemaRef,
    }

    impl TableFunctionImpl for Func {
        fn call(&self, exprs: &[Expr]) -> Result<Arc<dyn TableProvider>> {
            if !exprs.is_empty() {
                return plan_err!("pg_get_keywords takes no arguments");
            }
            Ok(Arc::new(TableFn {
                schema: self.schema.clone(),
            }))
        }
    }

    ctx.register_udtf(
        "pg_get_keywords",
        Arc::new(Func {
            schema: schema.clone(),
        }),
    );
    ctx.register_udtf("pg_catalog.pg_get_keywords", Arc::new(Func { schema }));
    Ok(())
}

/// Register `pg_relation_size(oid)` returning zero for now.
pub fn register_pg_relation_size(ctx: &SessionContext) -> Result<()> {
    use arrow::datatypes::DataType;
    use datafusion::common::ScalarValue;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |_args: &[ColumnarValue]| -> Result<ColumnarValue> {
        Ok(ColumnarValue::Scalar(ScalarValue::Int64(Some(0))))
    };

    let udf = create_udf(
        "pg_catalog.pg_relation_size",
        vec![DataType::Int64],
        DataType::Int64,
        Volatility::Stable,
        Arc::new(fun),
    )
    .with_aliases(["pg_relation_size"]);
    ctx.register_udf(udf);
    Ok(())
}

/// Register `pg_total_relation_size(oid)` returning zero for now.
pub fn register_pg_total_relation_size(ctx: &SessionContext) -> Result<()> {
    use arrow::datatypes::DataType;
    use datafusion::common::ScalarValue;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |_args: &[ColumnarValue]| -> Result<ColumnarValue> {
        Ok(ColumnarValue::Scalar(ScalarValue::Int64(Some(0))))
    };

    let udf = create_udf(
        "pg_catalog.pg_total_relation_size",
        vec![DataType::Int64],
        DataType::Int64,
        Volatility::Stable,
        Arc::new(fun),
    )
    .with_aliases(["pg_total_relation_size"]);
    ctx.register_udf(udf);
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::scalar_to_cte::rewrite_subquery_as_cte;

    use super::*;
    use arrow::array::{Int64Array, ListArray, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use datafusion::catalog::memory::{MemoryCatalogProvider, MemorySchemaProvider};
    use datafusion::catalog::{CatalogProvider, SchemaProvider};
    use datafusion::datasource::MemTable;
    use datafusion::error::Result;
    use std::sync::Arc;

    /* TODO:

    postgresql handles number::regclass differently. it just passes them as oid.

    postgres=# select '222222222'::regclass::oid;
    oid
    -----------
     222222222
    (1 row)


     */

    async fn make_ctx() -> Result<SessionContext> {
        let config = datafusion::execution::context::SessionConfig::new()
            .with_default_catalog_and_schema("public", "pg_catalog");

        let ctx = SessionContext::new_with_config(config);
        ctx.register_udtf("regclass_oid", Arc::new(RegClassOidFunc));
        register_scalar_regclass_oid(&ctx)?;
        register_pg_get_one(&ctx)?;
        register_pg_get_array(&ctx)?;
        register_array_agg(&ctx)?;
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
    async fn test_regclass_with_oid() -> Result<()> {
        let ctx = make_ctx().await?;
        let batches = ctx
            .sql("SELECT * FROM regclass_oid('pg_constraint');")
            .await?
            .collect()
            .await?;
        let col = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
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
        let col = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
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
        let col = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
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
        let col = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
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
        assert!(batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap()
            .is_null(0));
        Ok(())
    }

    #[tokio::test]
    async fn test_pggetone_constant() -> Result<()> {
        let ctx = make_ctx().await?;
        let batches = ctx
            .sql("SELECT pg_get_one('hello') AS v;")
            .await?
            .collect()
            .await?;
        let col = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(col.value(0), "hello");
        Ok(())
    }

    #[tokio::test]
    async fn test_pggetone_subquery() -> Result<()> {
        let ctx = make_ctx().await?;
        let batches = ctx
            .sql("SELECT pg_get_one((SELECT relname FROM pg_catalog.pg_class LIMIT 1)) AS v;")
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

    #[tokio::test]
    async fn test_pg_get_array_constant() -> Result<()> {
        let ctx = make_ctx().await?;
        let batches = ctx
            .sql("SELECT pg_get_array('hello') AS v;")
            .await?
            .collect()
            .await?;
        let list = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<ListArray>()
            .unwrap();
        let inner = list.value(0);
        let inner = inner.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(inner.value(0), "hello");
        Ok(())
    }

    #[tokio::test]
    async fn test_pg_get_array_subquery() -> Result<()> {
        let ctx = make_ctx().await?;

        let sql = rewrite_subquery_as_cte(
            "SELECT pg_get_array((SELECT relname FROM pg_catalog.pg_class order by 1)) AS v;",
        );
        let batches = ctx.sql(&sql).await?.collect().await?;

        let list = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<ListArray>()
            .unwrap();
        log::debug!("test_pg_get_array_subquery {:?}", list);
        let inner = list.value(0);
        let inner = inner.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(inner.value(0), "pg_constraint");
        Ok(())
    }

    #[tokio::test]
    async fn test_pg_catalog_array_agg_alias() -> Result<()> {
        use arrow::array::ListArray;

        let ctx = make_ctx().await?;

        let sql =
            "SELECT pg_catalog.array_agg(relname ORDER BY relname) AS v FROM pg_catalog.pg_class";
        let batches = ctx.sql(sql).await?.collect().await?;
        let list = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<ListArray>()
            .unwrap();
        assert_eq!(list.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_pg_postmaster_start_time_fn() -> Result<()> {
        use arrow::array::TimestampMicrosecondArray;
        let ctx = SessionContext::new();
        register_pg_postmaster_start_time(&ctx)?;
        let batches = ctx
            .sql("SELECT pg_postmaster_start_time()")
            .await?
            .collect()
            .await?;
        let arr = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<TimestampMicrosecondArray>()
            .unwrap();
        assert!(!arr.is_null(0));
        Ok(())
    }

    #[tokio::test]
    async fn test_pg_age_always_one() -> datafusion::error::Result<()> {
        use arrow::array::Int64Array;

        // 1️⃣  fresh context
        let ctx = SessionContext::new();

        // 2️⃣  register the helper we just added
        register_scalar_pg_age(&ctx)?;

        // 3️⃣  run any query that invokes the function
        let batches = ctx
            .sql("SELECT pg_catalog.age(123::BIGINT) AS v;")
            .await?
            .collect()
            .await?;

        // 4️⃣  assert we got the constant 1 back
        let arr = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();

        assert_eq!(arr.value(0), 1);
        Ok(())
    }

    #[tokio::test]
    async fn pg_is_in_recovery_always_false() -> Result<()> {
        let ctx = SessionContext::new();
        register_scalar_pg_is_in_recovery(&ctx)?;

        let batches = ctx
            .sql("SELECT pg_catalog.pg_is_in_recovery()")
            .await?
            .collect()
            .await?;
        let arr = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::BooleanArray>()
            .unwrap();
        assert_eq!(arr.value(0), false);
        Ok(())
    }

    #[tokio::test]
    async fn txid_current_ticks_up() -> Result<()> {
        let ctx = SessionContext::new();
        register_scalar_txid_current(&ctx)?;

        let v1: i64 = ctx
            .sql("SELECT pg_catalog.txid_current()")
            .await?
            .collect()
            .await?[0]
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap()
            .value(0);
        let v2: i64 = ctx
            .sql("SELECT pg_catalog.txid_current()")
            .await?
            .collect()
            .await?[0]
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap()
            .value(0);

        assert!(v2 == v1 + 1);
        Ok(())
    }

    #[tokio::test]
    async fn available_extension_versions_empty() -> Result<()> {
        let ctx = SessionContext::new();
        register_pg_available_extension_versions(&ctx)?;
        let batches = ctx
            .sql("SELECT * FROM pg_available_extension_versions()")
            .await?
            .collect()
            .await?;
        assert_eq!(batches[0].num_rows(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn pg_get_keywords_empty() -> Result<()> {
        let ctx = SessionContext::new();
        register_pg_get_keywords(&ctx)?;
        let batches = ctx
            .sql("SELECT * FROM pg_get_keywords()")
            .await?
            .collect()
            .await?;
        assert_eq!(batches[0].num_rows(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn relation_size_returns_zero() -> Result<()> {
        use arrow::array::Int64Array;
        let ctx = SessionContext::new();
        register_pg_relation_size(&ctx)?;
        let batches = ctx
            .sql("SELECT pg_catalog.pg_relation_size(1)")
            .await?
            .collect()
            .await?;
        let arr = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(arr.value(0), 0);
        Ok(())
    }

    #[tokio::test]
    async fn total_relation_size_returns_zero() -> Result<()> {
        use arrow::array::Int64Array;
        let ctx = SessionContext::new();
        register_pg_total_relation_size(&ctx)?;
        let batches = ctx
            .sql("SELECT pg_catalog.pg_total_relation_size(1)")
            .await?
            .collect()
            .await?;
        let arr = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(arr.value(0), 0);
        Ok(())
    }

    #[tokio::test]
    async fn encode_returns_null() -> Result<()> {
        use arrow::array::StringArray;
        let ctx = SessionContext::new();
        register_encode(&ctx)?;
        let batches = ctx
            .sql("SELECT pg_catalog.encode(NULL::bytea, 'escape')")
            .await?
            .collect()
            .await?;
        assert!(batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .is_null(0));
        Ok(())
    }

    #[tokio::test]
    async fn pg_get_triggerdef_returns_null() -> Result<()> {
        use arrow::array::StringArray;
        let ctx = SessionContext::new();
        register_pg_get_triggerdef(&ctx)?;
        let batches = ctx
            .sql("SELECT pg_catalog.pg_get_triggerdef(1)")
            .await?
            .collect()
            .await?;
        assert!(batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .is_null(0));
        Ok(())
    }

    #[tokio::test]
    async fn upper_converts_text() -> Result<()> {
        use arrow::array::StringArray;
        let ctx = SessionContext::new();
        register_upper(&ctx)?;
        let batches = ctx
            .sql("SELECT pg_catalog.upper('abc')")
            .await?
            .collect()
            .await?;
        let arr = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(arr.value(0), "ABC");
        Ok(())
    }

    #[tokio::test]
    async fn pg_get_ruledef_returns_null() -> Result<()> {
        use arrow::array::StringArray;
        let ctx = SessionContext::new();
        register_pg_get_ruledef(&ctx)?;
        let batches = ctx
            .sql("SELECT pg_catalog.pg_get_ruledef(1)")
            .await?
            .collect()
            .await?;
        assert!(batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .is_null(0));
        Ok(())
    }

    #[tokio::test]
    async fn oidvector_to_array_parses() -> Result<()> {
        use arrow::array::{Int64Array, ListArray};
        let ctx = SessionContext::new();
        register_oidvector_to_array(&ctx)?;
        let batches = ctx
            .sql("SELECT oidvector_to_array('1 2 3') AS v")
            .await?
            .collect()
            .await?;
        let list = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<ListArray>()
            .unwrap();
        let inner = list.value(0);
        let inner = inner.as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(inner.values(), &[1, 2, 3]);
        Ok(())
    }

    #[tokio::test]
    async fn current_schemas_returns_defaults() -> Result<()> {
        use arrow::array::{ListArray, StringArray};
        let ctx = SessionContext::new();

        register_current_schemas(
            &ctx,
            Arc::new(|_| vec!["pg_catalog".to_string(), "public".to_string()]),
        )?;

        let batches = ctx
            .sql("SELECT current_schemas(true) AS v")
            .await?
            .collect()
            .await?;
        let list = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<ListArray>()
            .unwrap();
        let inner = list.value(0);
        let inner = inner.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(inner.value(0), "pg_catalog");
        assert_eq!(inner.value(1), "public");
        Ok(())
    }

    #[tokio::test]
    async fn current_schema_uses_callable() -> Result<()> {
        use arrow::array::StringArray;
        let ctx = SessionContext::new();

        register_current_schema(
            &ctx,
            Arc::new(|_| vec!["myschema".to_string(), "other".to_string()]),
        )?;

        let batches = ctx
            .sql("SELECT current_schema() AS v")
            .await?
            .collect()
            .await?;
        let arr = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(arr.value(0), "myschema");
        Ok(())
    }

    #[tokio::test]
    async fn has_database_privilege_always_true() -> Result<()> {
        use arrow::array::BooleanArray;
        let ctx = SessionContext::new();
        register_has_database_privilege(&ctx)?;
        let batches = ctx
            .sql("SELECT pg_catalog.has_database_privilege(1, 'CREATE')")
            .await?
            .collect()
            .await?;
        let arr = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .unwrap();
        assert!(arr.value(0));

        let batches = ctx
            .sql("SELECT pg_catalog.has_database_privilege('pgtry', 'CONNECT')")
            .await?
            .collect()
            .await?;
        let arr = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .unwrap();
        assert!(arr.value(0));
        Ok(())
    }

    #[tokio::test]
    async fn has_schema_privilege_always_true() -> Result<()> {
        use arrow::array::BooleanArray;
        let ctx = SessionContext::new();
        register_has_schema_privilege(&ctx)?;
        let batches = ctx
            .sql("SELECT pg_catalog.has_schema_privilege(1, 'CREATE')")
            .await?
            .collect()
            .await?;
        let arr = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .unwrap();
        assert!(arr.value(0));

        let batches = ctx
            .sql("SELECT pg_catalog.has_schema_privilege('public', 'USAGE')")
            .await?
            .collect()
            .await?;
        let arr = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .unwrap();
        assert!(arr.value(0));
        Ok(())
    }
}
