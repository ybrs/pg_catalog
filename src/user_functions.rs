// Collection of custom UDF and UDTF implementations.
// Provides functions like oid(), pg_get_array and others so queries behave like PostgreSQL.
// Added to extend DataFusion with features required by pg_catalog emulation.
use arrow::array::{as_string_array, Array, ArrayRef, StringBuilder, TimestampMicrosecondArray};
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
use datafusion::logical_expr::{
    ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
    Volatility,
};
use datafusion::logical_expr::{Expr, TableType};
use datafusion::prelude::SessionContext;
use datafusion::prelude::*;
use once_cell::sync::Lazy;
use std::future::Future;
use std::sync::Arc;

/// A dedicated multi-threaded runtime used to drive the small synchronous
/// catalog lookups some scalar UDFs perform (e.g. `pg_get_userbyid`, `oid(text)`).
///
/// These UDFs are synchronous but must run a catalog SQL query, and they may be
/// invoked from within ANY caller runtime: a current-thread runtime (as
/// `#[tokio::test]` uses) where `tokio::task::block_in_place` would panic, or a
/// worker of a multi-threaded runtime where re-entering `block_on` would
/// deadlock. Driving the query on this separate runtime — and blocking the
/// caller on a plain std channel — is safe regardless of the caller's flavor.
static CATALOG_QUERY_RT: Lazy<tokio::runtime::Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to build the pg_catalog query runtime")
});

/// Drive `future` to completion from a synchronous context, regardless of the
/// caller's tokio runtime flavor. The future runs on [`CATALOG_QUERY_RT`] while
/// the calling thread blocks on a std channel until it finishes.
fn run_catalog_query<F, T>(future: F) -> T
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    CATALOG_QUERY_RT.spawn(async move {
        let _ = tx.send(future.await);
    });
    rx.recv()
        .expect("pg_catalog query task ended without producing a result")
}

#[derive(Debug)]
struct RegClassOidTable {
    schema: SchemaRef,
    relname: String,
}

#[async_trait]
impl TableProvider for RegClassOidTable {
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
        let relname = if let Some(Expr::Literal(ScalarValue::Utf8(Some(ref s)), _)) = exprs.first()
        {
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

    let lookup_oid_fn = Arc::new(move |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        match &args[0] {
            ColumnarValue::Scalar(ScalarValue::Utf8(Some(name))) => {
                let sql = format!(
                    "SELECT oid FROM pg_catalog.pg_class WHERE relname = '{}'",
                    name.replace('\'', "''")
                );

                let opt: Option<i64> = {
                    let ctx = ctx_arc.clone();
                    run_catalog_query(async move {
                        let batches = ctx.sql(&sql).await?.collect().await?;
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
                }?;

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
                    let opt: Option<i64> = {
                        let ctx = ctx_arc.clone();
                        run_catalog_query(async move {
                            let batches = ctx.sql(&sql).await?.collect().await?;
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
                    }?;
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
        lookup_oid_fn,
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

/// pg_catalog.pg_has_role(\[user,\] role, privilege) -> bool
///
/// Compatibility stub that always returns `true`: the emulated single
/// superuser is treated as a member of every role. This unblocks the many
/// information_schema role/privilege views that filter rows with
/// `pg_has_role(...)`. Covers both the 2-argument `pg_has_role(role, privilege)`
/// and 3-argument `pg_has_role(user, role, privilege)` forms, with `user`/`role`
/// given either as an OID (int) or a role name (text).
pub fn register_pg_has_role(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, BooleanArray};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
        Volatility,
    };
    use std::sync::Arc;

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct PgHasRole {
        sig: Signature,
    }

    impl PgHasRole {
        fn new() -> Self {
            // Accept `pg_has_role(role, priv)` and `pg_has_role(user, role, priv)`.
            // role/user may be an OID (int) or a name (text); since this is a stub
            // that ignores its inputs, accept any argument types.
            Self {
                sig: Signature::one_of(
                    vec![TypeSignature::Any(2), TypeSignature::Any(3)],
                    Volatility::Stable,
                ),
            }
        }
    }

    impl ScalarUDFImpl for PgHasRole {
        fn name(&self) -> &str {
            "pg_catalog.pg_has_role"
        }
        fn signature(&self) -> &Signature {
            &self.sig
        }
        fn return_type(&self, _t: &[DataType]) -> Result<DataType> {
            Ok(DataType::Boolean)
        }
        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            // The emulated single superuser is a member of every role -> always true.
            let arrays = ColumnarValue::values_to_arrays(&args.args)?;
            let len = arrays.first().map(|a| a.len()).unwrap_or(1);
            Ok(ColumnarValue::Array(
                Arc::new(BooleanArray::from(vec![true; len])) as ArrayRef,
            ))
        }
    }

    let udf = ScalarUDF::new_from_impl(PgHasRole::new()).with_aliases(["pg_has_role"]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.pg_is_other_temp_schema(oid) -> bool
///
/// Compatibility stub that always returns `false`: we emulate a single session
/// with no other backends, so no schema is "another session's temp schema".
/// Many information_schema views filter rows with
/// `NOT pg_is_other_temp_schema(...)`.
pub fn register_pg_is_other_temp_schema(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, BooleanArray};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
        Volatility,
    };
    use std::sync::Arc;

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct PgIsOtherTempSchema {
        sig: Signature,
    }

    impl PgIsOtherTempSchema {
        fn new() -> Self {
            // One OID argument, given as int or name -> accept any single arg.
            Self {
                sig: Signature::one_of(vec![TypeSignature::Any(1)], Volatility::Stable),
            }
        }
    }

    impl ScalarUDFImpl for PgIsOtherTempSchema {
        fn name(&self) -> &str {
            "pg_catalog.pg_is_other_temp_schema"
        }
        fn signature(&self) -> &Signature {
            &self.sig
        }
        fn return_type(&self, _t: &[DataType]) -> Result<DataType> {
            Ok(DataType::Boolean)
        }
        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            let arrays = ColumnarValue::values_to_arrays(&args.args)?;
            let len = arrays.first().map(|a| a.len()).unwrap_or(1);
            Ok(ColumnarValue::Array(
                Arc::new(BooleanArray::from(vec![false; len])) as ArrayRef,
            ))
        }
    }

    let udf = ScalarUDF::new_from_impl(PgIsOtherTempSchema::new())
        .with_aliases(["pg_is_other_temp_schema"]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.pg_my_temp_schema() -> oid
///
/// Compatibility stub returning `0`: we emulate a session with no temp schema,
/// and PostgreSQL returns `0` (InvalidOid) when the session has none.
pub fn register_pg_my_temp_schema(ctx: &SessionContext) -> Result<()> {
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |_args: &[ColumnarValue]| -> Result<ColumnarValue> {
        Ok(ColumnarValue::Scalar(ScalarValue::Int32(Some(0))))
    };
    let udf = create_udf(
        "pg_catalog.pg_my_temp_schema",
        vec![],
        ArrowDataType::Int32,
        Volatility::Stable,
        Arc::new(fun),
    )
    .with_aliases(["pg_my_temp_schema"]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.getdatabaseencoding() -> name
///
/// Compatibility stub returning `'UTF8'` (the catalog is generated as UTF8).
pub fn register_getdatabaseencoding(ctx: &SessionContext) -> Result<()> {
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |_args: &[ColumnarValue]| -> Result<ColumnarValue> {
        Ok(ColumnarValue::Scalar(ScalarValue::Utf8(Some(
            "UTF8".to_string(),
        ))))
    };
    let udf = create_udf(
        "pg_catalog.getdatabaseencoding",
        vec![],
        ArrowDataType::Utf8,
        Volatility::Stable,
        Arc::new(fun),
    )
    .with_aliases(["getdatabaseencoding"]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.format(formatstr, ...args) -> text
///
/// PostgreSQL's `format()` string-formatting function. Implements the conversion
/// specifiers used by the catalog views and the common cases:
///
/// * `%s` — the argument as a string (NULL renders as empty, per PostgreSQL),
/// * `%I` — the argument as a quoted SQL identifier,
/// * `%L` — the argument as a quoted SQL literal (NULL renders as `NULL`),
/// * `%%` — a literal percent sign.
///
/// Arguments are consumed left to right (positional `%n$` specifiers are not
/// supported — no catalog view uses them). Used by the `check_constraints` view
/// (`format('%s IS NOT NULL', ...)`).
pub fn register_format(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, StringArray, StringBuilder};
    use arrow::compute::cast;
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, Volatility,
    };
    use std::sync::Arc;

    /// Render one `format()` call given the format string and the already
    /// string-coerced arguments (NULL = `None`) for a single row.
    fn render_row(fmt: &str, args: &[Option<String>]) -> String {
        let mut out = String::with_capacity(fmt.len());
        let mut chars = fmt.chars().peekable();
        let mut next_arg = 0usize;
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => out.push('%'),
                Some('s') => {
                    let v = args.get(next_arg).and_then(|o| o.clone());
                    next_arg += 1;
                    out.push_str(v.as_deref().unwrap_or(""));
                }
                Some('I') => {
                    let v = args.get(next_arg).and_then(|o| o.clone()).unwrap_or_default();
                    next_arg += 1;
                    // Double-quote and escape embedded quotes (a safe superset of
                    // PostgreSQL's "quote only if needed").
                    out.push('"');
                    out.push_str(&v.replace('"', "\"\""));
                    out.push('"');
                }
                Some('L') => {
                    let v = args.get(next_arg).and_then(|o| o.clone());
                    next_arg += 1;
                    match v {
                        None => out.push_str("NULL"),
                        Some(s) => {
                            out.push('\'');
                            out.push_str(&s.replace('\'', "''"));
                            out.push('\'');
                        }
                    }
                }
                Some(other) => {
                    // Unknown specifier: emit verbatim rather than failing.
                    out.push('%');
                    out.push(other);
                }
                None => out.push('%'),
            }
        }
        out
    }

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct Format {
        sig: Signature,
    }

    impl ScalarUDFImpl for Format {
        /// The function name.
        fn name(&self) -> &str {
            "format"
        }
        /// Variadic: a format string followed by any number of arguments.
        fn signature(&self) -> &Signature {
            &self.sig
        }
        /// Always text.
        fn return_type(&self, _t: &[DataType]) -> Result<DataType> {
            Ok(DataType::Utf8)
        }
        /// Coerce every argument to a string column, then render each row.
        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            let n = args.number_rows;
            let arrays = ColumnarValue::values_to_arrays(&args.args)?;
            if arrays.is_empty() {
                return Err(DataFusionError::Internal(
                    "format() requires at least the format string".to_string(),
                ));
            }
            // Cast every column to Utf8 so we can read string values uniformly.
            // A length-1 column is a broadcast scalar.
            let cols: Vec<ArrayRef> = arrays
                .iter()
                .map(|a| cast(a, &DataType::Utf8))
                .collect::<std::result::Result<_, _>>()?;
            let as_str: Vec<&StringArray> = cols
                .iter()
                .map(|c| c.as_any().downcast_ref::<StringArray>().expect("cast to Utf8"))
                .collect();

            let val_at = |col: &StringArray, row: usize| -> Option<String> {
                let idx = if col.len() == 1 { 0 } else { row };
                if idx < col.len() && !col.is_null(idx) {
                    Some(col.value(idx).to_string())
                } else {
                    None
                }
            };

            let mut b = StringBuilder::new();
            for row in 0..n {
                match val_at(as_str[0], row) {
                    None => b.append_null(), // NULL format string -> NULL result
                    Some(fmt) => {
                        let row_args: Vec<Option<String>> =
                            as_str[1..].iter().map(|c| val_at(c, row)).collect();
                        b.append_value(render_row(&fmt, &row_args));
                    }
                }
            }
            Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
        }
    }

    let udf_impl = Format {
        sig: Signature::variadic_any(Volatility::Immutable),
    };
    let udf = ScalarUDF::new_from_impl(udf_impl).with_aliases(["pg_catalog.format"]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.pg_relation_is_updatable(relation, include_triggers) -> int4
///
/// Compatibility stub returning `0` (the bitmask for "not updatable"). The
/// information_schema view columns derived from it (`is_updatable`, etc.) then
/// read as `'NO'`, which is a safe default for an emulated read-mostly catalog.
pub fn register_pg_relation_is_updatable(ctx: &SessionContext) -> Result<()> {
    register_int_stub(ctx, "pg_catalog.pg_relation_is_updatable", 2, Some(0))
}

/// pg_catalog.information_schema._pg_char_max_length(typid, typmod) -> int4
///
/// Compatibility stub returning NULL: we don't derive a character maximum
/// length, and NULL is the correct value for non-character types anyway, so the
/// `character_maximum_length` column simply reads as NULL.
pub fn register_pg_char_max_length(ctx: &SessionContext) -> Result<()> {
    register_int_stub(ctx, "information_schema._pg_char_max_length", 2, None)
}

/// information_schema._pg_char_octet_length(typid, typmod) -> int4
///
/// Compatibility stub returning NULL (we don't derive octet lengths), so the
/// `character_octet_length` column reads as NULL. Used by the `domains` view.
pub fn register_pg_char_octet_length(ctx: &SessionContext) -> Result<()> {
    register_int_stub(ctx, "information_schema._pg_char_octet_length", 2, None)
}

/// information_schema._pg_index_position(indexoid, column) -> smallint
///
/// Compatibility stub returning NULL (we don't resolve a column's position
/// within an index), so `position_in_unique_constraint` reads as NULL. Used by
/// the `key_column_usage` view.
pub fn register_pg_index_position(ctx: &SessionContext) -> Result<()> {
    register_int_stub(ctx, "information_schema._pg_index_position", 2, None)
}

/// The remaining `information_schema._pg_*` numeric/datetime type-introspection
/// helpers, all returning NULL int4 stubs. They derive precision/scale facts we
/// don't model, and NULL is the correct value for types they don't apply to, so
/// the corresponding columns (numeric_precision, numeric_scale, ...) read NULL.
/// Used by `domains` (with scalar args). `_pg_interval_type` returns NULL text.
pub fn register_pg_numeric_helpers(ctx: &SessionContext) -> Result<()> {
    register_int_stub(ctx, "information_schema._pg_numeric_precision", 2, None)?;
    register_int_stub(ctx, "information_schema._pg_numeric_precision_radix", 2, None)?;
    register_int_stub(ctx, "information_schema._pg_numeric_scale", 2, None)?;
    register_int_stub(ctx, "information_schema._pg_datetime_precision", 2, None)?;
    register_null_text_stub(ctx, "information_schema._pg_interval_type", 2)?;
    Ok(())
}

/// Register `information_schema._pg_truetypid` and `_pg_truetypmod`.
///
/// In PostgreSQL these take two *whole-row* composite arguments — a
/// `pg_attribute` row and a `pg_type` row — and resolve a column's "true" type:
/// when the column's type is a domain (`typtype = 'd'`) they return the domain's
/// base type id / typmod, otherwise the attribute's own `atttypid` / `atttypmod`.
///
/// DataFusion has no composite/record scalar type, so it cannot bind `a.*` /
/// `t.*` as single arguments. The `rewrite_pg_truetypid_composite_args` pass
/// therefore expands each call into the three scalar columns the body actually
/// reads — `(atttypid|atttypmod, typtype, base)` — and these UDFs implement the
/// `CASE WHEN typtype = 'd' THEN base ELSE own END` selection over them. Used by
/// the `columns` and `attributes` information_schema views.
pub fn register_pg_truetypid_helpers(ctx: &SessionContext) -> Result<()> {
    register_truetype_select(ctx, "information_schema._pg_truetypid")?;
    register_truetype_select(ctx, "information_schema._pg_truetypmod")?;
    Ok(())
}

/// Register one "true type" selector under `qualified` (plus its bare alias).
///
/// The UDF takes three arguments `(own, typtype, base)`: per row it returns
/// `base` when `typtype = 'd'` (a domain) and `own` otherwise. `own` and `base`
/// carry the same type (both oid for `_pg_truetypid`, both int4 for
/// `_pg_truetypmod`), and that type is preserved on output.
fn register_truetype_select(ctx: &SessionContext, qualified: &'static str) -> Result<()> {
    use arrow::array::StringArray;
    use arrow::compute::cast;
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
        Volatility,
    };

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct TrueType {
        qualified: String,
        sig: Signature,
    }

    impl ScalarUDFImpl for TrueType {
        /// The fully-qualified function name.
        fn name(&self) -> &str {
            &self.qualified
        }
        /// The argument signature: exactly three arguments of any type.
        fn signature(&self) -> &Signature {
            &self.sig
        }
        /// The result type mirrors the first argument (`own`), which always
        /// shares its type with the third (`base`).
        fn return_type(&self, arg_types: &[DataType]) -> Result<DataType> {
            Ok(arg_types.first().cloned().unwrap_or(DataType::Int32))
        }
        /// For each row pick `base` (arg 2) when `typtype` (arg 1) equals `'d'`,
        /// otherwise `own` (arg 0). Built row-wise via `ScalarValue` so the
        /// concrete element type of `own`/`base` is preserved untouched.
        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            let arrays = ColumnarValue::values_to_arrays(&args.args)?;
            if arrays.len() != 3 {
                return Err(DataFusionError::Internal(format!(
                    "{} expects 3 arguments, got {}",
                    self.qualified,
                    arrays.len()
                )));
            }
            let len = arrays[0].len();
            let typtype = cast(&arrays[1], &DataType::Utf8)?;
            let typtype = typtype
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("cast to Utf8 yields StringArray");
            let mut picked = Vec::with_capacity(len);
            for i in 0..len {
                let is_domain = !typtype.is_null(i) && typtype.value(i) == "d";
                let src = if is_domain { &arrays[2] } else { &arrays[0] };
                picked.push(ScalarValue::try_from_array(src, i)?);
            }
            let out = ScalarValue::iter_to_array(picked)?;
            Ok(ColumnarValue::Array(out))
        }
    }

    let udf_impl = TrueType {
        qualified: qualified.to_string(),
        sig: Signature::one_of(vec![TypeSignature::Any(3)], Volatility::Immutable),
    };
    let bare = qualified.rsplit('.').next().unwrap_or(qualified);
    let udf = ScalarUDF::new_from_impl(udf_impl).with_aliases([bare]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.pg_get_function_arg_default(func oid, argnum int) -> text
///
/// Compatibility stub returning NULL: we don't model per-parameter default
/// expressions, so the `parameter_default` column of the `parameters`
/// information_schema view reads as NULL (the correct value for a parameter
/// without a default).
pub fn register_pg_get_function_arg_default(ctx: &SessionContext) -> Result<()> {
    register_null_text_stub(ctx, "pg_catalog.pg_get_function_arg_default", 2)
}

/// pg_catalog.pg_column_is_updatable(relation, column, include_triggers) -> bool
///
/// Compatibility stub returning `false`, the per-column counterpart of
/// [`register_pg_relation_is_updatable`] (which returns the "not updatable"
/// bitmask). The `columns` information_schema view's `is_updatable` column then
/// reads `'NO'`, a safe default for an emulated read-mostly catalog.
pub fn register_pg_column_is_updatable(ctx: &SessionContext) -> Result<()> {
    register_bool_stub(ctx, "pg_catalog.pg_column_is_updatable", 3, false)
}

/// Register a scalar stub under the fully-qualified `qualified` (plus its bare
/// last-segment alias) taking exactly `arity` args of any type and returning the
/// constant boolean `value`, broadcast over the input length.
fn register_bool_stub(
    ctx: &SessionContext,
    qualified: &'static str,
    arity: usize,
    value: bool,
) -> Result<()> {
    use arrow::array::{ArrayRef, BooleanArray};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
        Volatility,
    };
    use std::sync::Arc;

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct BoolStub {
        qualified: String,
        value: bool,
        sig: Signature,
    }

    impl ScalarUDFImpl for BoolStub {
        /// The fully-qualified function name.
        fn name(&self) -> &str {
            &self.qualified
        }
        /// The argument signature: exactly `arity` arguments of any type.
        fn signature(&self) -> &Signature {
            &self.sig
        }
        /// Always boolean.
        fn return_type(&self, _t: &[DataType]) -> Result<DataType> {
            Ok(DataType::Boolean)
        }
        /// The constant boolean, one value per input row.
        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            let arrays = ColumnarValue::values_to_arrays(&args.args)?;
            let len = arrays.first().map(|a| a.len()).unwrap_or(1);
            Ok(ColumnarValue::Array(
                Arc::new(BooleanArray::from(vec![self.value; len])) as ArrayRef,
            ))
        }
    }

    let udf_impl = BoolStub {
        qualified: qualified.to_string(),
        value,
        sig: Signature::one_of(vec![TypeSignature::Any(arity)], Volatility::Stable),
    };
    let bare = qualified.rsplit('.').next().unwrap_or(qualified);
    let udf = ScalarUDF::new_from_impl(udf_impl).with_aliases([bare]);
    ctx.register_udf(udf);
    Ok(())
}

/// Register a scalar stub under `qualified` (plus its bare alias) taking `arity`
/// args of any type and returning NULL text.
fn register_null_text_stub(
    ctx: &SessionContext,
    qualified: &'static str,
    arity: usize,
) -> Result<()> {
    use arrow::array::{ArrayRef, StringBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
        Volatility,
    };
    use std::sync::Arc;

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct NullText {
        qualified: String,
        sig: Signature,
    }

    impl ScalarUDFImpl for NullText {
        fn name(&self) -> &str {
            &self.qualified
        }
        fn signature(&self) -> &Signature {
            &self.sig
        }
        fn return_type(&self, _t: &[DataType]) -> Result<DataType> {
            Ok(DataType::Utf8)
        }
        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            let arrays = ColumnarValue::values_to_arrays(&args.args)?;
            let len = arrays.first().map(|a| a.len()).unwrap_or(1);
            let mut b = StringBuilder::new();
            for _ in 0..len {
                b.append_null();
            }
            Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
        }
    }

    let udf_impl = NullText {
        qualified: qualified.to_string(),
        sig: Signature::one_of(vec![TypeSignature::Any(arity)], Volatility::Stable),
    };
    let bare = qualified.rsplit('.').next().unwrap_or(qualified);
    let udf = ScalarUDF::new_from_impl(udf_impl).with_aliases([bare]);
    ctx.register_udf(udf);
    Ok(())
}

/// Register a scalar stub under the fully-qualified function name `qualified`
/// (plus its bare last-segment alias) that takes exactly `arity` arguments of
/// any type and returns the constant int4 `value` (or NULL when `value` is
/// `None`), broadcast over the input length.
fn register_int_stub(
    ctx: &SessionContext,
    qualified: &'static str,
    arity: usize,
    value: Option<i32>,
) -> Result<()> {
    use arrow::array::{ArrayRef, Int32Array};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
        Volatility,
    };
    use std::sync::Arc;

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct IntStub {
        qualified: String,
        value: Option<i32>,
        sig: Signature,
    }

    impl ScalarUDFImpl for IntStub {
        fn name(&self) -> &str {
            &self.qualified
        }
        fn signature(&self) -> &Signature {
            &self.sig
        }
        fn return_type(&self, _t: &[DataType]) -> Result<DataType> {
            Ok(DataType::Int32)
        }
        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            let arrays = ColumnarValue::values_to_arrays(&args.args)?;
            let len = arrays.first().map(|a| a.len()).unwrap_or(1);
            Ok(ColumnarValue::Array(
                Arc::new(Int32Array::from(vec![self.value; len])) as ArrayRef,
            ))
        }
    }

    let udf_impl = IntStub {
        qualified: qualified.to_string(),
        value,
        sig: Signature::one_of(vec![TypeSignature::Any(arity)], Volatility::Stable),
    };
    let bare = qualified.rsplit('.').next().unwrap_or(qualified);
    let udf = ScalarUDF::new_from_impl(udf_impl).with_aliases([bare]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.pg_options_to_table(options text[]) -> setof (option_name, option_value)
///
/// PostgreSQL set-returning function that splits each `"name=value"` option
/// string into a row. DataFusion can't host a set-returning function in the
/// projection, so we model the result as a **scalar** function returning
/// `List<Struct{option_name, option_value}>`; the `rewrite_srf_to_unnest` pass
/// then `unnest`s it so `(pg_options_to_table(x)).option_name` works. The
/// argument is the catalog's `_text` array (Arrow `List<Utf8>`).
pub fn register_pg_options_to_table(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, ListArray, ListBuilder, StringBuilder, StructBuilder};
    use arrow::datatypes::{DataType, Field, Fields};
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
        Volatility,
    };
    use std::sync::Arc;

    fn item_fields() -> Fields {
        vec![
            Field::new("option_name", DataType::Utf8, true),
            Field::new("option_value", DataType::Utf8, true),
        ]
        .into()
    }

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct PgOptionsToTable {
        sig: Signature,
    }

    impl ScalarUDFImpl for PgOptionsToTable {
        fn name(&self) -> &str {
            "pg_catalog.pg_options_to_table"
        }
        fn signature(&self) -> &Signature {
            &self.sig
        }
        fn return_type(&self, _t: &[DataType]) -> Result<DataType> {
            Ok(DataType::List(Arc::new(Field::new(
                "item",
                DataType::Struct(item_fields()),
                true,
            ))))
        }
        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            let arrays = ColumnarValue::values_to_arrays(&args.args)?;
            let input = arrays[0].as_any().downcast_ref::<ListArray>();
            let fields = item_fields();
            let mut builder = ListBuilder::new(StructBuilder::new(
                fields,
                vec![
                    Box::new(StringBuilder::new()),
                    Box::new(StringBuilder::new()),
                ],
            ));
            let len = arrays.first().map(|a| a.len()).unwrap_or(0);
            for i in 0..len {
                let opts = input.filter(|a| !a.is_null(i)).map(|a| a.value(i));
                match opts {
                    None => builder.append_null(),
                    Some(opts) => {
                        if let Some(strs) = opts.as_any().downcast_ref::<arrow::array::StringArray>()
                        {
                            let struct_builder = builder.values();
                            for j in 0..strs.len() {
                                if strs.is_null(j) {
                                    continue;
                                }
                                let s = strs.value(j);
                                let (name, value) = s.split_once('=').unwrap_or((s, ""));
                                struct_builder.field_builder::<StringBuilder>(0)
                                    .unwrap()
                                    .append_value(name);
                                struct_builder.field_builder::<StringBuilder>(1)
                                    .unwrap()
                                    .append_value(value);
                                struct_builder.append(true);
                            }
                        }
                        builder.append(true);
                    }
                }
            }
            Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
        }
    }

    let udf = ScalarUDF::new_from_impl(PgOptionsToTable {
        sig: Signature::one_of(vec![TypeSignature::Any(1)], Volatility::Immutable),
    })
    .with_aliases(["pg_options_to_table"]);
    ctx.register_udf(udf);
    Ok(())
}

/// information_schema._pg_expandarray(arr) -> setof (x, n)
///
/// PostgreSQL set-returning helper that expands an array into rows of
/// `(x = element, n = 1-based ordinal)`. Modeled as a scalar function returning
/// `List<Struct{x, n}>` so `(_pg_expandarray(a)).x` works via the
/// `rewrite_srf_to_unnest` pass. The catalog's arrays arrive as `List<Utf8>`, so
/// `x` is text and `n` is int4.
pub fn register_pg_expandarray(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, Int32Builder, ListArray, ListBuilder, StringBuilder, StructBuilder};
    use arrow::datatypes::{DataType, Field, Fields};
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
        Volatility,
    };
    use std::sync::Arc;

    fn item_fields() -> Fields {
        vec![
            Field::new("x", DataType::Utf8, true),
            Field::new("n", DataType::Int32, true),
        ]
        .into()
    }

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct PgExpandArray {
        sig: Signature,
    }

    impl ScalarUDFImpl for PgExpandArray {
        fn name(&self) -> &str {
            "information_schema._pg_expandarray"
        }
        fn signature(&self) -> &Signature {
            &self.sig
        }
        fn return_type(&self, _t: &[DataType]) -> Result<DataType> {
            Ok(DataType::List(Arc::new(Field::new(
                "item",
                DataType::Struct(item_fields()),
                true,
            ))))
        }
        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            let arrays = ColumnarValue::values_to_arrays(&args.args)?;
            let input = arrays[0].as_any().downcast_ref::<ListArray>();
            let mut builder = ListBuilder::new(StructBuilder::new(
                item_fields(),
                vec![Box::new(StringBuilder::new()), Box::new(Int32Builder::new())],
            ));
            let len = arrays.first().map(|a| a.len()).unwrap_or(0);
            for i in 0..len {
                let elems = input.filter(|a| !a.is_null(i)).map(|a| a.value(i));
                match elems {
                    None => builder.append_null(),
                    Some(elems) => {
                        if let Some(strs) =
                            elems.as_any().downcast_ref::<arrow::array::StringArray>()
                        {
                            let struct_builder = builder.values();
                            for j in 0..strs.len() {
                                let x = struct_builder.field_builder::<StringBuilder>(0).unwrap();
                                if strs.is_null(j) {
                                    x.append_null();
                                } else {
                                    x.append_value(strs.value(j));
                                }
                                struct_builder.field_builder::<Int32Builder>(1)
                                    .unwrap()
                                    .append_value((j + 1) as i32);
                                struct_builder.append(true);
                            }
                        }
                        builder.append(true);
                    }
                }
            }
            Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
        }
    }

    let udf = ScalarUDF::new_from_impl(PgExpandArray {
        sig: Signature::one_of(vec![TypeSignature::Any(1)], Volatility::Immutable),
    })
    .with_aliases(["_pg_expandarray"]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.aclexplode(acl) -> setof (grantor, grantee, privilege_type, is_grantable)
///
/// Compatibility stub returning an **empty** set: this catalog does not model
/// per-object access privileges, so there are no grants to explode. The
/// information_schema privilege views (table_privileges, etc.) then plan and run,
/// returning no rows — which is accurate for an emulated catalog with no ACLs.
/// Modeled as a scalar function returning an empty `List<Struct{...}>` so the
/// inline `(aclexplode(x)).grantee` form unnests to zero rows.
pub fn register_aclexplode(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, BooleanBuilder, Int32Builder, ListBuilder, StringBuilder, StructBuilder};
    use arrow::datatypes::{DataType, Field, Fields};
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
        Volatility,
    };
    use std::sync::Arc;

    fn item_fields() -> Fields {
        vec![
            Field::new("grantor", DataType::Int32, true),
            Field::new("grantee", DataType::Int32, true),
            Field::new("privilege_type", DataType::Utf8, true),
            Field::new("is_grantable", DataType::Boolean, true),
        ]
        .into()
    }

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct AclExplode {
        sig: Signature,
    }

    impl ScalarUDFImpl for AclExplode {
        fn name(&self) -> &str {
            "pg_catalog.aclexplode"
        }
        fn signature(&self) -> &Signature {
            &self.sig
        }
        fn return_type(&self, _t: &[DataType]) -> Result<DataType> {
            Ok(DataType::List(Arc::new(Field::new(
                "item",
                DataType::Struct(item_fields()),
                true,
            ))))
        }
        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            // Every row gets an empty list (no grants).
            let arrays = ColumnarValue::values_to_arrays(&args.args)?;
            let len = arrays.first().map(|a| a.len()).unwrap_or(1);
            let mut builder = ListBuilder::new(StructBuilder::new(
                item_fields(),
                vec![
                    Box::new(Int32Builder::new()),
                    Box::new(Int32Builder::new()),
                    Box::new(StringBuilder::new()),
                    Box::new(BooleanBuilder::new()),
                ],
            ));
            for _ in 0..len {
                builder.append(true); // empty list element
            }
            Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
        }
    }

    let udf = ScalarUDF::new_from_impl(AclExplode {
        sig: Signature::one_of(vec![TypeSignature::Any(1)], Volatility::Immutable),
    })
    .with_aliases(["aclexplode"]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.acldefault(type, owner) -> aclitem[]
///
/// Compatibility stub returning an **empty** ACL array. Paired with the
/// `aclexplode` stub, the information_schema privilege views' usual
/// `COALESCE(relacl, acldefault(...))` yields an empty ACL, which explodes to no
/// rows. The result is a `List<Utf8>` matching how the catalog stores `_aclitem`.
pub fn register_acldefault(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, ListBuilder, StringBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
        Volatility,
    };
    use std::sync::Arc;

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct AclDefault {
        sig: Signature,
    }

    impl ScalarUDFImpl for AclDefault {
        fn name(&self) -> &str {
            "pg_catalog.acldefault"
        }
        fn signature(&self) -> &Signature {
            &self.sig
        }
        fn return_type(&self, _t: &[DataType]) -> Result<DataType> {
            Ok(DataType::List(Arc::new(arrow::datatypes::Field::new(
                "item",
                DataType::Utf8,
                true,
            ))))
        }
        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            let arrays = ColumnarValue::values_to_arrays(&args.args)?;
            let len = arrays.first().map(|a| a.len()).unwrap_or(1);
            let mut builder = ListBuilder::new(StringBuilder::new());
            for _ in 0..len {
                builder.append(true); // empty acl array
            }
            Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
        }
    }

    let udf = ScalarUDF::new_from_impl(AclDefault {
        sig: Signature::one_of(vec![TypeSignature::Any(2)], Volatility::Immutable),
    })
    .with_aliases(["acldefault"]);
    ctx.register_udf(udf);
    Ok(())
}

/// Register a `has_<object>_privilege` compatibility stub that always returns
/// `true` (the emulated single superuser holds every privilege).
///
/// One flexible UDF per function name handles all real call shapes:
/// `has_*_privilege(object, privilege)` and
/// `has_*_privilege(user, object, privilege)`, with user/object given as an OID
/// (int) or a name (text). `base_name` is the unqualified function name (e.g.
/// `has_table_privilege`); it is registered under `pg_catalog.<base_name>` with
/// the bare name as an alias.
fn register_has_privilege_stub(ctx: &SessionContext, base_name: &'static str) -> Result<()> {
    use arrow::array::{ArrayRef, BooleanArray};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
        Volatility,
    };
    use std::sync::Arc;

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct HasPrivilege {
        qualified: String,
        sig: Signature,
    }

    impl ScalarUDFImpl for HasPrivilege {
        fn name(&self) -> &str {
            &self.qualified
        }
        fn signature(&self) -> &Signature {
            &self.sig
        }
        fn return_type(&self, _t: &[DataType]) -> Result<DataType> {
            Ok(DataType::Boolean)
        }
        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            let arrays = ColumnarValue::values_to_arrays(&args.args)?;
            let len = arrays.first().map(|a| a.len()).unwrap_or(1);
            Ok(ColumnarValue::Array(
                Arc::new(BooleanArray::from(vec![true; len])) as ArrayRef,
            ))
        }
    }

    let udf_impl = HasPrivilege {
        qualified: format!("pg_catalog.{base_name}"),
        sig: Signature::one_of(
            vec![TypeSignature::Any(2), TypeSignature::Any(3)],
            Volatility::Stable,
        ),
    };
    let udf = ScalarUDF::new_from_impl(udf_impl).with_aliases([base_name]);
    ctx.register_udf(udf);
    Ok(())
}

/// Register the full `has_*_privilege` family as always-true compatibility stubs.
///
/// PostgreSQL has one such function per object class; the information_schema
/// privilege views call most of them. The emulated single superuser holds every
/// privilege, so each returns `true`. (`has_database_privilege` /
/// `has_schema_privilege` keep their existing dedicated registrations.)
pub fn register_has_privilege_family(ctx: &SessionContext) -> Result<()> {
    for name in [
        "has_table_privilege",
        "has_column_privilege",
        "has_any_column_privilege",
        "has_type_privilege",
        "has_sequence_privilege",
        "has_function_privilege",
        "has_server_privilege",
        "has_foreign_data_wrapper_privilege",
        "has_tablespace_privilege",
        "has_language_privilege",
        "has_parameter_privilege",
    ] {
        register_has_privilege_stub(ctx, name)?;
    }
    Ok(())
}

/// pg_catalog.nameconcatoid(name, oid) -> text
///
/// PostgreSQL helper that builds a unique label by appending an object's OID to
/// its name (used by the routine/parameter information_schema views to make
/// `specific_name`s unique). Returns `"<name>_<oid>"`.
pub fn register_nameconcatoid(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, StringBuilder};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
        Volatility,
    };
    use std::sync::Arc;

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct NameConcatOid {
        sig: Signature,
    }

    impl ScalarUDFImpl for NameConcatOid {
        fn name(&self) -> &str {
            "pg_catalog.nameconcatoid"
        }
        fn signature(&self) -> &Signature {
            &self.sig
        }
        fn return_type(&self, _t: &[DataType]) -> Result<DataType> {
            Ok(DataType::Utf8)
        }
        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            use arrow::array::Array;
            let arrays = ColumnarValue::values_to_arrays(&args.args)?;
            let names = arrays[0].as_any().downcast_ref::<arrow::array::StringArray>();
            let len = arrays.first().map(|a| a.len()).unwrap_or(1);
            // The oid column may arrive as int or text; stringify generically.
            let oid_str = |i: usize| -> Option<String> {
                let a = &arrays[1];
                if a.is_null(i) {
                    return None;
                }
                if let Some(s) = a.as_any().downcast_ref::<arrow::array::StringArray>() {
                    Some(s.value(i).to_string())
                } else if let Some(v) = a.as_any().downcast_ref::<arrow::array::Int32Array>() {
                    Some(v.value(i).to_string())
                } else if let Some(v) = a.as_any().downcast_ref::<arrow::array::Int64Array>() {
                    Some(v.value(i).to_string())
                } else {
                    None
                }
            };
            let mut b = StringBuilder::with_capacity(len, 32 * len);
            for i in 0..len {
                let name = names.and_then(|n| (!n.is_null(i)).then(|| n.value(i).to_string()));
                match (name, oid_str(i)) {
                    (Some(n), Some(o)) => b.append_value(format!("{n}_{o}")),
                    _ => b.append_null(),
                }
            }
            Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
        }
    }

    let udf = ScalarUDF::new_from_impl(NameConcatOid {
        sig: Signature::one_of(vec![TypeSignature::Any(2)], Volatility::Immutable),
    })
    .with_aliases(["nameconcatoid"]);
    ctx.register_udf(udf);
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

#[allow(dead_code)]
fn fetch_user_by_oid(ctx: Arc<SessionContext>, oid: i64) -> Result<String> {
    run_catalog_query(async move {
        {
            let query = format!(
                "SELECT rolname FROM pg_catalog.pg_authid WHERE oid = {} LIMIT 1",
                oid
            );
            let df = ctx.sql(&query).await?;
            let batches = df.collect().await?;
            for batch in batches {
                if batch.num_rows() == 0 {
                    continue;
                }
                let col = batch.column(0);
                let arr = col
                    .as_any()
                    .downcast_ref::<arrow::array::StringArray>()
                    .ok_or_else(|| {
                        DataFusionError::Execution(
                            "pg_catalog.pg_authid.rolname must be text".to_string(),
                        )
                    })?;
                if !arr.is_null(0) {
                    return Ok(arr.value(0).to_string());
                }
            }
            Ok(format!("unknown (OID={oid})"))
        }
    })
}

/// Read an OID value out of an integer Arrow column at row `index`, widening any
/// signed/unsigned 32/64-bit integer to `i64`. Returns `None` for NULL or a
/// non-integer column.
fn oid_at(column: &ArrayRef, index: usize) -> Result<Option<i64>> {
    let scalar = ScalarValue::try_from_array(column, index)?;
    Ok(match scalar {
        ScalarValue::Int32(v) => v.map(i64::from),
        ScalarValue::Int64(v) => v,
        ScalarValue::UInt32(v) => v.map(i64::from),
        // A UInt64 above i64::MAX can't be a valid OID (OIDs are u32); treat it
        // as "no OID" rather than wrapping to a negative value that would
        // mis-resolve. `i64::try_from(..).ok()` yields None on overflow.
        ScalarValue::UInt64(v) => v.and_then(|val| i64::try_from(val).ok()),
        _ => None,
    })
}

/// Resolve a set of role OIDs to their `rolname`s with a SINGLE catalog query.
///
/// This is the batched replacement for calling [`fetch_user_by_oid`] once per
/// row: `pg_get_userbyid` over a column (e.g. `pg_tables.tableowner`) would
/// otherwise run one `pg_authid` query per row — O(rows) catalog queries. Here
/// the distinct OIDs are looked up together. OIDs absent from `pg_authid` are
/// simply missing from the returned map (callers substitute a placeholder); an
/// empty input short-circuits without querying.
fn fetch_users_by_oids(
    ctx: Arc<SessionContext>,
    oids: &[i64],
) -> Result<std::collections::HashMap<i64, String>> {
    use std::collections::HashMap;

    let mut out: HashMap<i64, String> = HashMap::new();
    if oids.is_empty() {
        return Ok(out);
    }

    let in_list = oids
        .iter()
        .map(|o| o.to_string())
        .collect::<Vec<_>>()
        .join(", ");

    run_catalog_query(async move {
        let query =
            format!("SELECT oid, rolname FROM pg_catalog.pg_authid WHERE oid IN ({in_list})");
        let df = ctx.sql(&query).await?;
        let batches = df.collect().await?;
        for batch in batches {
            if batch.num_rows() == 0 {
                continue;
            }
            let oid_col = batch.column(0);
            let name_col = batch
                .column(1)
                .as_any()
                .downcast_ref::<arrow::array::StringArray>()
                .ok_or_else(|| {
                    DataFusionError::Execution(
                        "pg_catalog.pg_authid.rolname must be text".to_string(),
                    )
                })?;
            for i in 0..batch.num_rows() {
                if name_col.is_null(i) {
                    continue;
                }
                if let Some(oid) = oid_at(oid_col, i)? {
                    out.insert(oid, name_col.value(i).to_string());
                }
            }
        }
        Ok::<_, DataFusionError>(out)
    })
}

struct PgGetUserById {
    sig: Signature,
    ctx: Arc<SessionContext>,
}

impl PgGetUserById {
    fn new(ctx: Arc<SessionContext>) -> Self {
        Self {
            sig: Signature::one_of(
                vec![
                    TypeSignature::Exact(vec![ArrowDataType::Int32]),
                    TypeSignature::Exact(vec![ArrowDataType::Int64]),
                    TypeSignature::Exact(vec![ArrowDataType::UInt32]),
                    TypeSignature::Exact(vec![ArrowDataType::UInt64]),
                ],
                Volatility::Stable,
            ),
            ctx,
        }
    }

    #[allow(dead_code)]
    fn lookup(&self, oid: i64) -> Result<String> {
        fetch_user_by_oid(self.ctx.clone(), oid)
    }
}

impl std::fmt::Debug for PgGetUserById {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgGetUserById").finish()
    }
}

impl PartialEq for PgGetUserById {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.ctx, &other.ctx)
    }
}

impl Eq for PgGetUserById {}

impl std::hash::Hash for PgGetUserById {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.ctx) as usize).hash(state);
    }
}

impl ScalarUDFImpl for PgGetUserById {
    fn name(&self) -> &str {
        "pg_catalog.pg_get_userbyid"
    }

    fn signature(&self) -> &Signature {
        &self.sig
    }

    fn return_type(&self, _arg_types: &[ArrowDataType]) -> Result<ArrowDataType> {
        Ok(ArrowDataType::Utf8)
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let arrays = ColumnarValue::values_to_arrays(&args.args)?;
        let arr = &arrays[0];
        let len = arr.len();

        // Decode every row's OID first, then resolve the DISTINCT ones with a
        // single catalog query. The previous implementation looked each row up
        // individually, turning pg_get_userbyid(<column>) into one pg_authid
        // query per row (e.g. ~400ms for `SELECT * FROM pg_tables`).
        let mut oids: Vec<Option<i64>> = Vec::with_capacity(len);
        for i in 0..len {
            let scalar = ScalarValue::try_from_array(arr, i)?;
            let oid = match scalar {
                ScalarValue::Int32(v) => v.map(i64::from),
                ScalarValue::Int64(v) => v,
                ScalarValue::UInt32(v) => v.map(i64::from),
                // See `oid_at`: an out-of-i64-range UInt64 is not a valid OID, so
                // map it to None instead of wrapping to a wrong (negative) value.
                ScalarValue::UInt64(v) => v.and_then(|val| i64::try_from(val).ok()),
                ScalarValue::Null => None,
                _ => {
                    return plan_err!("pg_get_userbyid expects an OID argument");
                }
            };
            oids.push(oid);
        }

        let mut distinct: Vec<i64> = oids.iter().flatten().copied().collect();
        distinct.sort_unstable();
        distinct.dedup();
        let names = fetch_users_by_oids(self.ctx.clone(), &distinct)?;

        let mut builder = StringBuilder::with_capacity(len, 16 * len.max(1));
        for oid in oids {
            match oid {
                Some(oid) => {
                    let name = names
                        .get(&oid)
                        .cloned()
                        .unwrap_or_else(|| format!("unknown (OID={oid})"));
                    builder.append_value(&name);
                }
                None => builder.append_null(),
            }
        }

        Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
    }
}

/// Register `pg_get_userbyid(oid)` which returns the role name for the
/// provided OID or "unknown (OID=…)" when no match is found.
pub fn register_scalar_pg_get_userbyid(ctx: &SessionContext) -> Result<()> {
    let udf = ScalarUDF::new_from_impl(PgGetUserById::new(Arc::new(ctx.clone())))
        .with_aliases(["pg_get_userbyid"]);
    ctx.register_udf(udf);
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
                    let string_array = a.as_any().downcast_ref::<StringArray>().unwrap();
                    let mut b = StringBuilder::with_capacity(string_array.len(), 32 * string_array.len());
                    for i in 0..string_array.len() {
                        if string_array.is_null(i) {
                            b.append_null();
                        } else {
                            b.append_value(string_array.value(i));
                        }
                    }
                    Ok(ColumnarValue::Array(Arc::new(b.finish()) as ArrayRef))
                }
                ColumnarValue::Scalar(ScalarValue::List(list)) => {
                    if list.is_null(0) {
                        return Ok(ColumnarValue::Scalar(ScalarValue::Utf8(None)));
                    }

                    let elem = list.value(0);
                    let string_array = elem.as_any().downcast_ref::<StringArray>().unwrap();

                    let mut parts = Vec::new();
                    for i in 0..string_array.len() {
                        if string_array.is_null(i) {
                            if let Some(ref nr) = null_rep {
                                parts.push(nr.clone());
                            }
                        } else {
                            parts.push(string_array.value(i).to_string());
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
            _info: &datafusion::logical_expr::simplify::SimplifyContext,
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
    collected_values: Vec<ScalarValue>,
    element_type: DataType,
}

impl ArrayCollector {
    fn new(element_type: DataType) -> Self {
        Self {
            collected_values: Vec::new(),
            element_type,
        }
    }
}

impl Accumulator for ArrayCollector {
    // ---------- state ----------
    fn state(&mut self) -> Result<Vec<ScalarValue>> {
        let arr = ScalarValue::new_list_from_iter(
            self.collected_values.clone().into_iter(),
            &self.element_type,
            /* contains_null = */ true,
        );
        Ok(vec![ScalarValue::List(arr)])
    }

    // ---------- input tuples ----------
    fn update_batch(&mut self, values: &[ArrayRef]) -> Result<()> {
        for i in 0..values[0].len() {
            self.collected_values.push(ScalarValue::try_from_array(&values[0], i)?);
        }
        Ok(())
    }

    // ---------- merge partial states ----------
    fn merge_batch(&mut self, states: &[ArrayRef]) -> Result<()> {
        for row in 0..states[0].len() {
            let scalar = ScalarValue::try_from_array(&states[0], row)?;
            if let ScalarValue::List(arc) = scalar {
                let list = arc.as_ref();
                for idx in 0..list.len() {
                    let inner = list.value(idx);
                    for j in 0..inner.len() {
                        self.collected_values.push(ScalarValue::try_from_array(&inner, j)?);
                    }
                }
            }
        }
        Ok(())
    }

    // ---------- final result ----------
    fn evaluate(&mut self) -> Result<ScalarValue> {
        let arr = ScalarValue::new_list_from_iter(
            std::mem::take(&mut self.collected_values).into_iter(),
            &self.element_type,
            true,
        );
        Ok(ScalarValue::List(arr))
    }

    // ---------- memory footprint ----------
    fn size(&self) -> usize {
        // very rough – 24 bytes per value
        24 * self.collected_values.len()
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
    let make_array_collector = |args: AccumulatorArgs| -> Result<Box<dyn Accumulator>> {
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
        Arc::new(make_array_collector), // accumulator factory
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
    struct ExtensionVersionsTable {
        schema: SchemaRef,
    }

    #[async_trait]
    impl TableProvider for ExtensionVersionsTable {
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
    struct ExtensionVersionsTableFunc {
        schema: SchemaRef,
    }

    impl TableFunctionImpl for ExtensionVersionsTableFunc {
        fn call(&self, exprs: &[Expr]) -> Result<Arc<dyn TableProvider>> {
            if !exprs.is_empty() {
                return plan_err!("pg_available_extension_versions takes no arguments");
            }
            Ok(Arc::new(ExtensionVersionsTable {
                schema: self.schema.clone(),
            }))
        }
    }

    ctx.register_udtf(
        "pg_available_extension_versions",
        Arc::new(ExtensionVersionsTableFunc {
            schema: schema.clone(),
        }),
    );
    ctx.register_udtf(
        "pg_catalog.pg_available_extension_versions",
        Arc::new(ExtensionVersionsTableFunc { schema }),
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
    struct KeywordsTable {
        schema: SchemaRef,
    }

    #[async_trait]
    impl TableProvider for KeywordsTable {
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
    struct KeywordsTableFunc {
        schema: SchemaRef,
    }

    impl TableFunctionImpl for KeywordsTableFunc {
        fn call(&self, exprs: &[Expr]) -> Result<Arc<dyn TableProvider>> {
            if !exprs.is_empty() {
                return plan_err!("pg_get_keywords takes no arguments");
            }
            Ok(Arc::new(KeywordsTable {
                schema: self.schema.clone(),
            }))
        }
    }

    ctx.register_udtf(
        "pg_get_keywords",
        Arc::new(KeywordsTableFunc {
            schema: schema.clone(),
        }),
    );
    ctx.register_udtf("pg_catalog.pg_get_keywords", Arc::new(KeywordsTableFunc { schema }));
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
    use arrow::array::{Int32Array, Int64Array, ListArray, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use datafusion::catalog::memory::{MemoryCatalogProvider, MemorySchemaProvider};
    use datafusion::catalog::{CatalogProvider, SchemaProvider};
    use datafusion::datasource::MemTable;
    use datafusion::error::Result;
    use std::sync::Arc;

    #[test]
    fn test_oid_at_handles_unsigned_columns() {
        use arrow::array::{ArrayRef, UInt32Array, UInt64Array};

        // A UInt64 above i64::MAX is not a valid OID -> None (no wrap to a
        // negative value that would mis-resolve a role).
        let too_big = Arc::new(UInt64Array::from(vec![u64::MAX])) as ArrayRef;
        assert_eq!(oid_at(&too_big, 0).unwrap(), None);

        // In-range unsigned values widen to i64 correctly.
        let u64_ok = Arc::new(UInt64Array::from(vec![27735u64])) as ArrayRef;
        assert_eq!(oid_at(&u64_ok, 0).unwrap(), Some(27735i64));
        let u32_ok = Arc::new(UInt32Array::from(vec![10u32])) as ArrayRef;
        assert_eq!(oid_at(&u32_ok, 0).unwrap(), Some(10i64));
    }

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

    #[tokio::test(flavor = "multi_thread")]
    async fn pg_get_userbyid_reads_pg_authid() -> Result<()> {
        let config = datafusion::execution::context::SessionConfig::new()
            .with_default_catalog_and_schema("pg_catalog", "pg_catalog");
        let ctx = SessionContext::new_with_config(config);

        let schema = Arc::new(Schema::new(vec![
            Field::new("oid", DataType::Int32, false),
            Field::new("rolname", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from(vec![10])),
                Arc::new(StringArray::from(vec!["sysuser"])),
            ],
        )?;
        let table = MemTable::try_new(schema, vec![vec![batch]])?;

        let catalog = Arc::new(MemoryCatalogProvider::new());
        ctx.register_catalog("pg_catalog", catalog.clone());
        let schema_provider = Arc::new(MemorySchemaProvider::new());
        catalog.register_schema("pg_catalog", schema_provider.clone())?;
        schema_provider.register_table("pg_authid".parse().unwrap(), Arc::new(table))?;

        register_scalar_pg_get_userbyid(&ctx)?;

        let batches = ctx
            .sql("SELECT pg_get_userbyid(10)")
            .await?
            .collect()
            .await?;
        let col = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(col.value(0), "sysuser");

        let batches = ctx
            .sql("SELECT pg_get_userbyid(111110)")
            .await?
            .collect()
            .await?;
        let col = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(col.value(0), "unknown (OID=111110)");
        Ok(())
    }
}
