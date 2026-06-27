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
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

/// A dedicated multi-threaded runtime used to drive catalog sub-queries only when
/// the caller is NOT already on a multi-threaded runtime (a current-thread runtime,
/// or no runtime), where [`tokio::task::block_in_place`] would panic. Its workers
/// are themselves multi-threaded, so nested catalog UDFs spawned from here stay
/// deadlock-free via the `block_in_place` branch. Production never reaches this (it
/// is `#[tokio::main]`); it exists so any embedder flavor is safe.
static CATALOG_FALLBACK_RT: Lazy<tokio::runtime::Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build the pg_catalog fallback runtime")
});

/// Drive `future` to completion from a synchronous context (a DataFusion scalar
/// UDF) without starving the runtime under nesting, on any caller flavor.
///
/// The future is always spawned as an ordinary task; the caller blocks on a channel
/// for its result. WHERE it is spawned and HOW the caller blocks depend on the
/// caller's runtime:
///
/// - On a multi-threaded runtime (production `#[tokio::main]`, and tests using
///   `#[tokio::test(flavor = "multi_thread")]`): spawn on the current runtime and
///   block inside [`tokio::task::block_in_place`], which hands the worker back so the
///   scheduler spawns/borrows a replacement. Every nested catalog query does the
///   same, so the runtime grows its worker set with the nesting depth instead of
///   parking a fixed-size pool - composed catalog UDFs cannot deadlock at any depth.
/// - On a current-thread runtime, or no runtime at all (where `block_in_place` would
///   panic): drive the future on a dedicated multi-threaded runtime
///   [`CATALOG_FALLBACK_RT`] and block the caller on a plain channel recv. Nested
///   catalog UDFs invoked while that future runs are then on a multi-threaded
///   runtime and take the `block_in_place` branch above, so nesting stays
///   deadlock-free there too.
///
/// This keeps the bridge safe for any consumer (e.g. a current-thread embedder)
/// without ever blocking a fixed-size pool, and without using OS threads directly.
fn run_catalog_query<F, T>(future: F) -> T
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    use tokio::runtime::{Handle, RuntimeFlavor};

    let on_multi_thread = matches!(
        Handle::try_current().map(|h| h.runtime_flavor()),
        Ok(RuntimeFlavor::MultiThread)
    );

    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    if on_multi_thread {
        Handle::current().spawn(async move {
            let _ = tx.send(future.await);
        });
        tokio::task::block_in_place(move || {
            rx.recv()
                .expect("pg_catalog query task ended without producing a result")
        })
    } else {
        CATALOG_FALLBACK_RT.spawn(async move {
            let _ = tx.send(future.await);
        });
        rx.recv()
            .expect("pg_catalog query task ended without producing a result")
    }
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

/// Look up the OID of the relation named `relname` in `pg_catalog.pg_class`,
/// returning `None` when no such relation exists or its OID is NULL.
///
/// Runs `SELECT oid FROM pg_catalog.pg_class WHERE relname = '...'` through the
/// catalog runtime (single quotes in the name are escaped) and reads column 0 of
/// the first row via [`oid_at`], which widens whichever integer width the planner
/// produced for the OID column.
fn query_relname_oid(ctx: Arc<SessionContext>, relname: &str) -> Result<Option<i64>> {
    let sql = format!(
        "SELECT oid FROM pg_catalog.pg_class WHERE relname = '{}'",
        relname.replace('\'', "''")
    );
    run_catalog_query(async move {
        let batches = ctx.sql(&sql).await?.collect().await?;
        if batches.is_empty() || batches[0].num_rows() == 0 {
            return Ok::<Option<i64>, DataFusionError>(None);
        }
        oid_at(batches[0].column(0), 0)
    })
}

/// Register `oid(text)` which looks up a table OID from `pg_class`.
pub fn register_scalar_regclass_oid(ctx: &SessionContext) -> Result<()> {
    let ctx_arc = Arc::new(ctx.clone());

    let lookup_oid_fn = Arc::new(move |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        match &args[0] {
            ColumnarValue::Scalar(ScalarValue::Utf8(Some(name))) => {
                let opt = query_relname_oid(ctx_arc.clone(), name)?;
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
                    match query_relname_oid(ctx_arc.clone(), arr.value(i))? {
                        Some(v) => builder.append_value(v),
                        None => builder.append_null(),
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

/// The current session user reported by `current_user` / `session_user`, read at call
/// time. A live catalog view is planned eagerly at startup and captures the UDF
/// instances its body references, so these UDFs must read a mutable slot rather than a
/// value baked in at registration - otherwise a view body's `CURRENT_USER` would freeze
/// to the startup value. The connection handler keeps it current via [`set_session_user`].
static SESSION_USER: Lazy<std::sync::RwLock<String>> =
    Lazy::new(|| std::sync::RwLock::new("postgres".to_string()));

/// Set the user reported by `current_user` / `session_user` / `current_role`.
pub fn set_session_user(user: &str) {
    *SESSION_USER.write().expect("session user slot poisoned") = user.to_string();
}

/// Register `current_user` / `session_user` / `current_role` (and their
/// `pg_catalog`-qualified aliases) as no-argument UDFs reporting the current session
/// user. They read the mutable [`SESSION_USER`] slot at call time, so a view body's
/// `CURRENT_USER` - planned eagerly at startup, before any client connects - both plans
/// then and resolves to the querying connection's user at execution.
pub fn register_session_identity(ctx: &SessionContext) -> Result<()> {
    for (name, alias) in [
        ("current_user", "pg_catalog.current_user"),
        ("session_user", "pg_catalog.session_user"),
        ("current_role", "pg_catalog.current_role"),
    ] {
        let udf = create_udf(
            name,
            vec![],
            ArrowDataType::Utf8,
            Volatility::Stable,
            Arc::new(|_args| {
                let user = SESSION_USER
                    .read()
                    .expect("session user slot poisoned")
                    .clone();
                Ok(ColumnarValue::Scalar(ScalarValue::Utf8(Some(user))))
            }),
        )
        .with_aliases([alias]);
        ctx.register_udf(udf);
    }
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

use datafusion::common::cast::as_int64_array;

/// Render the SQL name of the type whose `pg_type.typname` is `typname`, applying
/// `typmod` where the type carries a modifier (length, precision, scale).
///
/// Mirrors the canonical-name table inside PostgreSQL's `format_type`: a fixed set
/// of built-in types print their SQL-standard spelling (`int4` -> `integer`,
/// `timestamptz` -> `timestamp with time zone`), and everything else prints its
/// bare `typname` (`name` -> `name`, a user type -> its own name). `typmod` is the
/// stored modifier; for `varchar`/`bpchar` it is `length + VARHDRSZ(4)`, for
/// `numeric` it packs precision and scale, and for the date/time types it is the
/// fractional-seconds precision.
fn sql_name_for_typname(typname: &str, typmod: Option<i64>) -> String {
    /// Fractional-seconds precision suffix, e.g. `(2)`, or empty when unmodified.
    fn precision_suffix(typmod: Option<i64>) -> String {
        match typmod {
            Some(p) if p >= 0 => format!("({})", p),
            _ => String::new(),
        }
    }
    match typname {
        "bool" => "boolean".to_string(),
        "int2" => "smallint".to_string(),
        "int4" => "integer".to_string(),
        "int8" => "bigint".to_string(),
        "float4" => "real".to_string(),
        "float8" => "double precision".to_string(),
        "char" => "\"char\"".to_string(),
        "varchar" => match typmod {
            Some(tm) if tm >= 0 => format!("character varying({})", tm - 4),
            _ => "character varying".to_string(),
        },
        "bpchar" => match typmod {
            Some(tm) if tm >= 0 => format!("character({})", tm - 4),
            _ => "character".to_string(),
        },
        "numeric" => match typmod {
            Some(tm) if tm >= 4 => {
                let bits = tm - 4;
                format!("numeric({},{})", (bits >> 16) & 0xFFFF, bits & 0xFFFF)
            }
            _ => "numeric".to_string(),
        },
        "timestamp" => format!("timestamp{} without time zone", precision_suffix(typmod)),
        "timestamptz" => format!("timestamp{} with time zone", precision_suffix(typmod)),
        "time" => format!("time{} without time zone", precision_suffix(typmod)),
        "timetz" => format!("time{} with time zone", precision_suffix(typmod)),
        "interval" => "interval".to_string(),
        "bit" => "bit".to_string(),
        "varbit" => "bit varying".to_string(),
        other if is_reserved_type_keyword(other) => format!("\"{}\"", other),
        other => other.to_string(),
    }
}

/// Whether `format_type` would double-quote this `typname` because it is a SQL
/// reserved word (e.g. the `any` pseudo-type prints as `"any"`). `char` is quoted
/// by its own arm above. New names that show up quoted in the snapshots get added
/// here - the content snapshot test flags any that are missing.
fn is_reserved_type_keyword(typname: &str) -> bool {
    matches!(typname, "any")
}

/// Render `format_type(oid, typmod)` using a `pg_type.oid -> typname` lookup.
///
/// Array types (whose `typname` starts with `_`) print their element type's SQL
/// name followed by `[]`. An OID absent from `by_oid` (no such `pg_type` row)
/// falls back to the bare OID text, matching the previous behavior for unknowns.
fn format_type_name(oid: i64, typmod: Option<i64>, by_oid: &HashMap<i64, String>) -> String {
    match by_oid.get(&oid) {
        Some(typname) if typname.starts_with('_') => {
            format!("{}[]", sql_name_for_typname(&typname[1..], None))
        }
        Some(typname) => sql_name_for_typname(typname, typmod),
        None => oid.to_string(),
    }
}

/// Read `pg_catalog.pg_type` into an `oid -> typname` map. The catalog is static
/// once loaded, so this snapshot stays valid for the session's lifetime.
async fn load_typname_by_oid(ctx: &SessionContext) -> Result<HashMap<i64, String>> {
    use arrow::array::{Int64Array, StringArray};
    let mut by_oid = HashMap::new();
    let df = ctx
        .sql("SELECT CAST(oid AS BIGINT) AS oid, typname FROM pg_catalog.pg_type")
        .await?;
    for batch in df.collect().await? {
        let oids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| DataFusionError::Internal("pg_type.oid not Int64".into()))?;
        let names = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| DataFusionError::Internal("pg_type.typname not Utf8".into()))?;
        for i in 0..batch.num_rows() {
            if !oids.is_null(i) && !names.is_null(i) {
                by_oid.insert(oids.value(i), names.value(i).to_string());
            }
        }
    }
    Ok(by_oid)
}

/// Register `format_type(oid, typmod)` (and its `pg_catalog.` alias) backed by a
/// real `pg_type.typname` lookup, so it resolves every type the catalog knows -
/// not just a hand-picked handful - to its SQL name. Falls back to the OID text
/// for an unknown OID.
pub async fn register_scalar_format_type(ctx: &SessionContext) -> Result<()> {
    let by_oid = Arc::new(load_typname_by_oid(ctx).await?);
    let fun = move |args: &[ColumnarValue]| -> Result<ColumnarValue> {
        let arrays = ColumnarValue::values_to_arrays(args)?;
        let oids = as_int64_array(&arrays[0])?;
        let mods = as_int64_array(&arrays[1])?;
        let mut builder = StringBuilder::new();
        for i in 0..oids.len() {
            if oids.is_null(i) {
                builder.append_null();
            } else {
                let typmod = (!mods.is_null(i)).then(|| mods.value(i));
                builder.append_value(format_type_name(oids.value(i), typmod, &by_oid));
            }
        }
        Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
    };
    let udf = create_udf(
        "format_type",
        vec![ArrowDataType::Int64, ArrowDataType::Int64],
        ArrowDataType::Utf8,
        Volatility::Stable,
        Arc::new(fun),
    )
    .with_aliases(["pg_catalog.format_type"]);
    ctx.register_udf(udf);
    Ok(())
}

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
    register_null_text_stub(ctx, "pg_catalog.pg_get_partkeydef", 1)
}

/// Placeholder for `pg_get_statisticsobjdef_columns` which currently
/// returns NULL for all rows.
pub fn register_pg_get_statisticsobjdef_columns(ctx: &SessionContext) -> Result<()> {
    register_null_text_stub(ctx, "pg_catalog.pg_get_statisticsobjdef_columns", 1)
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

/// Register an always-`true` `(object, privilege) -> bool` compatibility stub
/// under `pg_catalog.<base_name>` (with the bare name as an alias), accepting the
/// object argument as an OID (`Int32`/`Int64`) or a name (`Utf8`).
fn register_always_true_object_privilege(ctx: &SessionContext, base_name: &'static str) -> Result<()> {
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
            &format!("pg_catalog.{base_name}"),
            vec![dt.clone(), DataType::Utf8],
            DataType::Boolean,
            Volatility::Stable,
            Arc::new(fun),
        )
        .with_aliases([base_name]);
        ctx.register_udf(udf);
    }
    Ok(())
}

/// pg_catalog.has_database_privilege(database, text) -> bool
///
/// Compatibility stub that always returns `true`.
pub fn register_has_database_privilege(ctx: &SessionContext) -> Result<()> {
    register_always_true_object_privilege(ctx, "has_database_privilege")
}

/// pg_catalog.has_schema_privilege(schema, text) -> bool
///
/// Compatibility stub that always returns `true`.
pub fn register_has_schema_privilege(ctx: &SessionContext) -> Result<()> {
    register_always_true_object_privilege(ctx, "has_schema_privilege")
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
/// * `%s` - the argument as a string (NULL renders as empty, per PostgreSQL),
/// * `%I` - the argument as a quoted SQL identifier,
/// * `%L` - the argument as a quoted SQL literal (NULL renders as `NULL`),
/// * `%%` - a literal percent sign.
///
/// Arguments are consumed left to right (positional `%n$` specifiers are not
/// supported - no catalog view uses them). Used by the `check_constraints` view
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
                    let v = args
                        .get(next_arg)
                        .and_then(|o| o.clone())
                        .unwrap_or_default();
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
                .map(|c| {
                    c.as_any()
                        .downcast_ref::<StringArray>()
                        .expect("cast to Utf8")
                })
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

/// information_schema._pg_char_max_length(typid, typmod) -> int4
///
/// Computes the declared character maximum length from the type OID and typmod
/// (see [`pg_char_max_length`]), populating `character_maximum_length` in the
/// `columns` / `domains` views.
pub fn register_pg_char_max_length(ctx: &SessionContext) -> Result<()> {
    register_type_fact_int_fn(
        ctx,
        "information_schema._pg_char_max_length",
        pg_char_max_length,
    )
}

/// information_schema._pg_char_octet_length(typid, typmod) -> int4
///
/// Computes the maximum length in bytes from the type OID and typmod (see
/// [`pg_char_octet_length`]), populating `character_octet_length` in the
/// `columns` / `domains` views.
pub fn register_pg_char_octet_length(ctx: &SessionContext) -> Result<()> {
    register_type_fact_int_fn(
        ctx,
        "information_schema._pg_char_octet_length",
        pg_char_octet_length,
    )
}

/// information_schema._pg_index_position(indexoid, column) -> smallint
///
/// Compatibility stub returning NULL (we don't resolve a column's position
/// within an index), so `position_in_unique_constraint` reads as NULL. Used by
/// the `key_column_usage` view.
pub fn register_pg_index_position(ctx: &SessionContext) -> Result<()> {
    register_int_stub(ctx, "information_schema._pg_index_position", 2, None)
}

/// The `information_schema._pg_*` numeric/datetime type-introspection helpers,
/// each computing its fact from the type OID and typmod (see the `pg_numeric_*` /
/// `pg_datetime_precision` formulas). They populate `numeric_precision`,
/// `numeric_precision_radix`, `numeric_scale`, and `datetime_precision` in the
/// `columns` / `domains` views. `_pg_interval_type` (the interval field
/// qualifier, e.g. `YEAR TO MONTH`) is still a NULL text stub.
pub fn register_pg_numeric_helpers(ctx: &SessionContext) -> Result<()> {
    register_type_fact_int_fn(
        ctx,
        "information_schema._pg_numeric_precision",
        pg_numeric_precision,
    )?;
    register_type_fact_int_fn(
        ctx,
        "information_schema._pg_numeric_precision_radix",
        pg_numeric_precision_radix,
    )?;
    register_type_fact_int_fn(
        ctx,
        "information_schema._pg_numeric_scale",
        pg_numeric_scale,
    )?;
    register_type_fact_int_fn(
        ctx,
        "information_schema._pg_datetime_precision",
        pg_datetime_precision,
    )?;
    register_null_text_stub(ctx, "information_schema._pg_interval_type", 2)?;
    Ok(())
}

/// Register `information_schema._pg_truetypid` and `_pg_truetypmod`.
///
/// In PostgreSQL these take two *whole-row* composite arguments - a
/// `pg_attribute` row and a `pg_type` row - and resolve a column's "true" type:
/// when the column's type is a domain (`typtype = 'd'`) they return the domain's
/// base type id / typmod, otherwise the attribute's own `atttypid` / `atttypmod`.
///
/// DataFusion has no composite/record scalar type, so it cannot bind `a.*` /
/// `t.*` as single arguments. The `rewrite_pg_truetypid_composite_args` pass
/// therefore expands each call into the three scalar columns the body actually
/// reads - `(atttypid|atttypmod, typtype, base)` - and these UDFs implement the
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
    register_null_text_stub_accepting_arities(ctx, qualified, &[arity])
}

/// Register a scalar stub under `qualified` (plus its bare alias) that returns
/// NULL text and accepts a call at any of the given `arities` (each an argument
/// count of any types). Used for PostgreSQL functions that are exposed with
/// several arities, e.g. `pg_get_triggerdef(oid)` and `pg_get_triggerdef(oid,
/// bool)`.
fn register_null_text_stub_accepting_arities(
    ctx: &SessionContext,
    qualified: &'static str,
    arities: &[usize],
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

    let accepted_arities = arities
        .iter()
        .map(|&arity| TypeSignature::Any(arity))
        .collect();
    let udf_impl = NullText {
        qualified: qualified.to_string(),
        sig: Signature::one_of(accepted_arities, Volatility::Stable),
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
// Type OIDs the information_schema precision/length helpers branch on. These are
// the fixed built-in OIDs assigned by PostgreSQL (identical in every server).
const OID_INT2: i64 = 21;
const OID_INT4: i64 = 23;
const OID_INT8: i64 = 20;
const OID_NUMERIC: i64 = 1700;
const OID_FLOAT4: i64 = 700;
const OID_FLOAT8: i64 = 701;
const OID_TEXT: i64 = 25;
const OID_BPCHAR: i64 = 1042;
const OID_VARCHAR: i64 = 1043;
const OID_BIT: i64 = 1560;
const OID_VARBIT: i64 = 1562;
const OID_DATE: i64 = 1082;
const OID_TIME: i64 = 1083;
const OID_TIMESTAMP: i64 = 1114;
const OID_TIMESTAMPTZ: i64 = 1184;
const OID_TIMETZ: i64 = 1266;
const OID_INTERVAL: i64 = 1186;

/// `information_schema._pg_numeric_precision(typid, typmod)`: the number of
/// significant digits a numeric column of this type can hold, or NULL for
/// non-numeric types. Mirrors PostgreSQL's helper: fixed widths for the integer
/// and float types, and the precision packed into `typmod` for `numeric`.
fn pg_numeric_precision(typid: Option<i64>, typmod: Option<i64>) -> Option<i32> {
    match typid? {
        OID_INT2 => Some(16),
        OID_INT4 => Some(32),
        OID_INT8 => Some(64),
        OID_FLOAT4 => Some(24),
        OID_FLOAT8 => Some(53),
        OID_NUMERIC => match typmod? {
            -1 => None,
            m => Some((((m - 4) >> 16) & 65535) as i32),
        },
        _ => None,
    }
}

/// `information_schema._pg_numeric_precision_radix(typid, typmod)`: the base in
/// which the precision is expressed - 2 for binary integer/float types, 10 for
/// `numeric`, NULL otherwise.
fn pg_numeric_precision_radix(typid: Option<i64>, _typmod: Option<i64>) -> Option<i32> {
    match typid? {
        OID_INT2 | OID_INT4 | OID_INT8 | OID_FLOAT4 | OID_FLOAT8 => Some(2),
        OID_NUMERIC => Some(10),
        _ => None,
    }
}

/// `information_schema._pg_numeric_scale(typid, typmod)`: digits after the decimal
/// point - 0 for the integer types, the scale packed into `typmod` for `numeric`,
/// NULL otherwise (the float types have no defined scale).
fn pg_numeric_scale(typid: Option<i64>, typmod: Option<i64>) -> Option<i32> {
    match typid? {
        OID_INT2 | OID_INT4 | OID_INT8 => Some(0),
        OID_NUMERIC => match typmod? {
            -1 => None,
            m => Some(((m - 4) & 65535) as i32),
        },
        _ => None,
    }
}

/// `information_schema._pg_datetime_precision(typid, typmod)`: fractional-seconds
/// precision - 0 for `date`, the `typmod` (defaulting to 6) for the time/timestamp
/// types, and the low 16 bits of `typmod` for `interval`, NULL otherwise.
fn pg_datetime_precision(typid: Option<i64>, typmod: Option<i64>) -> Option<i32> {
    match typid? {
        OID_DATE => Some(0),
        OID_TIME | OID_TIMESTAMP | OID_TIMESTAMPTZ | OID_TIMETZ => {
            let m = typmod.unwrap_or(-1);
            Some(if m < 0 { 6 } else { m as i32 })
        }
        OID_INTERVAL => {
            let m = typmod.unwrap_or(-1);
            Some(if m < 0 || (m & 65535) == 65535 {
                6
            } else {
                (m & 65535) as i32
            })
        }
        _ => None,
    }
}

/// `information_schema._pg_char_max_length(typid, typmod)`: declared maximum
/// character length - `typmod - 4` for `char`/`varchar` (NULL when unbounded),
/// `typmod` for the bit-string types, NULL otherwise.
fn pg_char_max_length(typid: Option<i64>, typmod: Option<i64>) -> Option<i32> {
    match typid? {
        OID_BPCHAR | OID_VARCHAR => match typmod? {
            -1 => None,
            m => Some((m - 4) as i32),
        },
        OID_BIT | OID_VARBIT => typmod.map(|m| m as i32),
        _ => None,
    }
}

/// Maximum bytes per character for the catalog's server encoding. This catalog is
/// UTF-8 (`pg_encoding_max_length('UTF8') = 4`), which is what the octet-length
/// helper multiplies the character length by.
const ENCODING_MAX_LENGTH: i32 = 4;

/// `information_schema._pg_char_octet_length(typid, typmod)`: maximum length in
/// bytes - `1 GiB` for an unbounded `text`/`char`/`varchar`, otherwise the
/// character length times the server encoding's max bytes-per-character, NULL for
/// non-character types.
fn pg_char_octet_length(typid: Option<i64>, typmod: Option<i64>) -> Option<i32> {
    match typid? {
        OID_TEXT | OID_BPCHAR | OID_VARCHAR => match typmod.unwrap_or(-1) {
            -1 => Some(1 << 30),
            _ => pg_char_max_length(typid, typmod).map(|l| l * ENCODING_MAX_LENGTH),
        },
        _ => None,
    }
}

/// Read an Arrow array as one nullable i64 per row, casting from whatever integer
/// (or numeric-text) type the planner supplied. Rows that cannot be read as an
/// integer become `None`.
fn array_as_opt_i64(array: &ArrayRef) -> Vec<Option<i64>> {
    use arrow::datatypes::DataType;
    match arrow::compute::cast(array, &DataType::Int64) {
        Ok(casted) => {
            let a = casted.as_any().downcast_ref::<Int64Array>().unwrap();
            (0..a.len())
                .map(|i| if a.is_null(i) { None } else { Some(a.value(i)) })
                .collect()
        }
        Err(_) => vec![None; array.len()],
    }
}

/// Register a 2-argument `(typid, typmod) -> int4` information_schema helper whose
/// result is a pure function of the two integer arguments. Both arguments arrive
/// as catalog integers (a type OID and a typmod) and are read NULL-safely, so the
/// formula can branch on the type OID and decode the typmod.
fn register_type_fact_int_fn(
    ctx: &SessionContext,
    qualified: &'static str,
    func: fn(Option<i64>, Option<i64>) -> Option<i32>,
) -> Result<()> {
    use arrow::array::{ArrayRef, Int32Builder};
    use arrow::datatypes::DataType;

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct TypeFactFn {
        qualified: String,
        func: fn(Option<i64>, Option<i64>) -> Option<i32>,
        sig: Signature,
    }

    impl ScalarUDFImpl for TypeFactFn {
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
            let typid = arrays
                .first()
                .map(array_as_opt_i64)
                .unwrap_or(vec![None; len]);
            let typmod = arrays
                .get(1)
                .map(array_as_opt_i64)
                .unwrap_or(vec![None; len]);
            let mut out = Int32Builder::with_capacity(len);
            for i in 0..len {
                match (self.func)(typid[i], typmod[i]) {
                    Some(v) => out.append_value(v),
                    None => out.append_null(),
                }
            }
            Ok(ColumnarValue::Array(Arc::new(out.finish()) as ArrayRef))
        }
    }

    let udf_impl = TypeFactFn {
        qualified: qualified.to_string(),
        func,
        sig: Signature::one_of(vec![TypeSignature::Any(2)], Volatility::Stable),
    };
    let bare = qualified.rsplit('.').next().unwrap_or(qualified);
    let udf = ScalarUDF::new_from_impl(udf_impl).with_aliases([bare]);
    ctx.register_udf(udf);
    Ok(())
}

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
                        if let Some(strs) =
                            opts.as_any().downcast_ref::<arrow::array::StringArray>()
                        {
                            let struct_builder = builder.values();
                            for j in 0..strs.len() {
                                if strs.is_null(j) {
                                    continue;
                                }
                                let s = strs.value(j);
                                let (name, value) = s.split_once('=').unwrap_or((s, ""));
                                struct_builder
                                    .field_builder::<StringBuilder>(0)
                                    .unwrap()
                                    .append_value(name);
                                struct_builder
                                    .field_builder::<StringBuilder>(1)
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
/// `rewrite_srf_to_unnest` pass. `x` is Int64 (the element value - a column
/// number or type oid) and `n` is int4 (its 1-based position); the helper
/// `element_as_i64` accepts the int arrays (`conkey`, `proargtypes`) the views
/// pass as well as legacy text arrays.
pub fn register_pg_expandarray(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{
        Array, ArrayRef, Int32Builder, Int64Builder, ListArray, ListBuilder, StructBuilder,
    };
    use arrow::datatypes::{DataType, Field, Fields};
    use datafusion::logical_expr::{
        ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
        Volatility,
    };
    use std::sync::Arc;

    fn item_fields() -> Fields {
        // `x` is the array element (a column number or type oid) and `n` its
        // 1-based position. `x` is Int64 so it compares directly against the int
        // columns the views join it to (e.g. `pg_attribute.attnum = (ss.x).x`).
        vec![
            Field::new("x", DataType::Int64, true),
            Field::new("n", DataType::Int32, true),
        ]
        .into()
    }

    /// The element at index `j` of `elems` as an i64, handling the int arrays
    /// (`conkey` Int16/Int32, `proargtypes` Int64) and legacy text arrays.
    fn element_as_i64(elems: &dyn Array, j: usize) -> Option<i64> {
        use arrow::array::{Int16Array, Int32Array, Int64Array, StringArray};
        if elems.is_null(j) {
            return None;
        }
        if let Some(a) = elems.as_any().downcast_ref::<Int64Array>() {
            Some(a.value(j))
        } else if let Some(a) = elems.as_any().downcast_ref::<Int32Array>() {
            Some(a.value(j) as i64)
        } else if let Some(a) = elems.as_any().downcast_ref::<Int16Array>() {
            Some(a.value(j) as i64)
        } else if let Some(a) = elems.as_any().downcast_ref::<StringArray>() {
            a.value(j).parse::<i64>().ok()
        } else {
            None
        }
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
                vec![Box::new(Int64Builder::new()), Box::new(Int32Builder::new())],
            ));
            let len = arrays.first().map(|a| a.len()).unwrap_or(0);
            for i in 0..len {
                let elems = input.filter(|a| !a.is_null(i)).map(|a| a.value(i));
                match elems {
                    None => builder.append_null(),
                    Some(elems) => {
                        let struct_builder = builder.values();
                        for j in 0..elems.len() {
                            let x = struct_builder.field_builder::<Int64Builder>(0).unwrap();
                            match element_as_i64(elems.as_ref(), j) {
                                Some(v) => x.append_value(v),
                                None => x.append_null(),
                            }
                            struct_builder
                                .field_builder::<Int32Builder>(1)
                                .unwrap()
                                .append_value((j + 1) as i32);
                            struct_builder.append(true);
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
/// returning no rows - which is accurate for an emulated catalog with no ACLs.
/// Modeled as a scalar function returning an empty `List<Struct{...}>` so the
/// inline `(aclexplode(x)).grantee` form unnests to zero rows.
pub fn register_aclexplode(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{
        ArrayRef, BooleanBuilder, Int32Builder, ListBuilder, StringBuilder, StructBuilder,
    };
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
            let names = arrays[0]
                .as_any()
                .downcast_ref::<arrow::array::StringArray>();
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
/// `pg_get_userbyid` over a column (e.g. `pg_tables.tableowner`) resolves the
/// distinct OIDs together rather than running one `pg_authid` query per row, so
/// the cost is one catalog query regardless of row count. OIDs absent from
/// `pg_authid` are simply missing from the returned map (callers substitute a
/// placeholder); an empty input short-circuits without querying.
fn fetch_users_by_oids(ctx: Arc<SessionContext>, oids: &[i64]) -> Result<HashMap<i64, String>> {
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
/// provided OID or "unknown (OID=...)" when no match is found.
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
                    let mut b =
                        StringBuilder::with_capacity(string_array.len(), 32 * string_array.len());
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
            self.collected_values
                .push(ScalarValue::try_from_array(&values[0], i)?);
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
                        self.collected_values
                            .push(ScalarValue::try_from_array(&inner, j)?);
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
        // very rough - 24 bytes per value
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
        "pg_get_array",                 // name
        vec![element_dt],               // input types
        Arc::new(list_dt.clone()),      // return type
        Volatility::Immutable,          // volatility
        Arc::new(make_array_collector), // accumulator factory
        Arc::new(vec![list_dt]),        // state type
    );

    ctx.register_udaf(udaf.clone());
    ctx.register_udaf(udaf.with_aliases(["pg_catalog.pg_get_array"]));
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

    // one closure - we don't care about the argument, just return 1
    let fun = |_args: &[ColumnarValue]| -> Result<ColumnarValue> {
        Ok(ColumnarValue::Scalar(ScalarValue::Int64(Some(1))))
    };

    // accept BIGINT *or* TEXT
    for dt in [DataType::Int64, DataType::Utf8] {
        let udf = create_udf(
            "pg_catalog.age", // <- exact name Postgres uses
            vec![dt],
            DataType::Int64, // always returns BIGINT
            Volatility::Stable,
            Arc::new(fun),
        );
        ctx.register_udf(udf);
    }
    Ok(())
}

/// pg_catalog.pg_is_in_recovery() -> BOOL
///
/// We don't do physical recovery, so just return `false`.
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

/// pg_catalog.txid_current()  ->  BIGINT
///
/// We don't run a real MVCC engine, so we fake a transaction counter that
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

/// pg_catalog.quote_ident(text) -> text
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

/// pg_catalog.translate(text, text, text) -> text
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

/// Register `pg_catalog.pg_get_viewdef(oid [, pretty])`.
///
/// The function resolves each view OID to its identity from the live catalog and
/// asks the process-wide [`ViewDefinitionResolver`] (set by
/// [`set_view_definition_resolver`]) for the definition text, returning NULL when
/// no resolver is set, the OID names no view, or the resolver declines. It is
/// registered during session construction so the live `pg_views` view - whose
/// stored plan binds the UDF at creation time - calls this resolver-backed
/// implementation; the embedding application can install or change the resolver at
/// any later point because the UDF reads the resolver slot at call time.
pub fn register_pg_get_viewdef(ctx: &SessionContext) -> Result<()> {
    let udf = ScalarUDF::new_from_impl(PgGetViewDef::new(Arc::new(ctx.clone())))
        .with_aliases(["pg_get_viewdef"]);
    ctx.register_udf(udf);
    Ok(())
}

/// pg_catalog.pg_get_function_arguments(oid) -> text
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

/// Render the canonical `CREATE INDEX` statement for a plain (non-expression)
/// index, matching PostgreSQL's `pg_get_indexdef` output.
///
/// `columns` are the indexed column names in index-key order. The table is
/// always schema-qualified, e.g.
/// `CREATE UNIQUE INDEX foo_pkey ON public.foo USING btree (a, b)`. Every
/// identifier (index, schema, table, columns) is quoted when it is not a plain
/// lowercase identifier, so mixed-case or special names keep their meaning.
fn render_create_index_statement(
    is_unique: bool,
    index_name: &str,
    schema_name: &str,
    table_name: &str,
    access_method: &str,
    columns: &[String],
) -> String {
    let unique = if is_unique { "UNIQUE " } else { "" };
    let columns = columns
        .iter()
        .map(|c| quote_identifier_if_needed(c))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE {unique}INDEX {} ON {}.{} USING {access_method} ({columns})",
        quote_identifier_if_needed(index_name),
        quote_identifier_if_needed(schema_name),
        quote_identifier_if_needed(table_name),
    )
}

/// Quote a SQL identifier the way PostgreSQL's `quote_ident` does: leave a plain
/// identifier (a lowercase letter or `_`, followed by lowercase letters, digits, or
/// `_`) unquoted, and double-quote anything else, doubling any embedded `"`. This
/// keeps already-safe catalog names (e.g. `pg_proc`) verbatim while protecting
/// mixed-case or special user names.
fn quote_identifier_if_needed(ident: &str) -> String {
    let is_plain = !ident.is_empty()
        && ident
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && ident
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if is_plain {
        ident.to_string()
    } else {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }
}

/// The structural parts of one index, read from the catalog, that a plain
/// `CREATE INDEX` statement is templated from. `key_attnums` are the indexed
/// columns' attribute numbers in key order; a `0` marks an expression column,
/// which makes the index functional/partial (its text comes from the
/// integration-installed definition resolver, not rendered here).
struct PlainIndexParts {
    index_oid: i64,
    table_oid: i64,
    is_unique: bool,
    key_attnums: Vec<i64>,
    index_name: String,
    table_name: String,
    schema_name: String,
    access_method: String,
}

/// Resolve a set of index OIDs to their `CREATE INDEX` text with a fixed, small
/// number of UDF-free catalog queries (one for the index/table/access-method
/// parts, one for the column names). Indexes whose key includes an expression
/// (attnum `0`), and any whose parts cannot be fully resolved, are omitted from
/// the map so the caller renders SQL NULL for them.
fn fetch_index_definitions(ctx: Arc<SessionContext>, oids: &[i64]) -> Result<HashMap<i64, String>> {
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
        // One row per index: name, owning table + schema, access method,
        // uniqueness and the key-column attnums (indkey is an int2vector list).
        let parts_sql = format!(
            "SELECT x.indexrelid, x.indrelid, x.indisunique, x.indkey, \
                    i.relname AS indexname, c.relname AS tablename, \
                    n.nspname AS schemaname, am.amname, x.indexprs, x.indpred \
             FROM pg_catalog.pg_index x \
             JOIN pg_catalog.pg_class i ON i.oid = x.indexrelid \
             JOIN pg_catalog.pg_class c ON c.oid = x.indrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_am am ON am.oid = i.relam \
             WHERE x.indexrelid IN ({in_list})"
        );
        let part_batches = ctx.sql(&parts_sql).await?.collect().await?;

        let mut parsed: Vec<PlainIndexParts> = Vec::new();
        // Identity of every requested index that exists, captured up front so the
        // resolver fallback below can describe functional/partial indexes the
        // structural render skips.
        let mut identities: HashMap<i64, IndexIdentity> = HashMap::new();
        for batch in &part_batches {
            for row in 0..batch.num_rows() {
                let index_oid = match column_oid_at(batch, "indexrelid", row)? {
                    Some(v) => v,
                    None => continue,
                };
                let index_name = column_string_at(batch, "indexname", row)?;
                let table_name = column_string_at(batch, "tablename", row)?;
                let schema_name = column_string_at(batch, "schemaname", row)?;
                if let (Some(name), Some(table), Some(schema)) =
                    (&index_name, &table_name, &schema_name)
                {
                    identities.insert(
                        index_oid,
                        IndexIdentity {
                            oid: index_oid,
                            schema: schema.clone(),
                            table: table.clone(),
                            name: name.clone(),
                        },
                    );
                }

                // A functional index (indexprs) or partial index (indpred) carries a
                // node-tree expression we do not deparse; the resolver fallback below
                // supplies its text, so skip structural render here.
                if column_is_non_null(batch, "indexprs", row)?
                    || column_is_non_null(batch, "indpred", row)?
                {
                    continue;
                }
                let table_oid = match column_oid_at(batch, "indrelid", row)? {
                    Some(v) => v,
                    None => continue,
                };
                let key_attnums = match index_key_attnums_at(batch, "indkey", row)? {
                    Some(v) => v,
                    None => continue,
                };
                let access_method = column_string_at(batch, "amname", row)?;
                let (Some(index_name), Some(table_name), Some(schema_name), Some(access_method)) =
                    (index_name, table_name, schema_name, access_method)
                else {
                    continue;
                };
                parsed.push(PlainIndexParts {
                    index_oid,
                    table_oid,
                    is_unique: column_bool_at(batch, "indisunique", row)?.unwrap_or(false),
                    key_attnums,
                    index_name,
                    table_name,
                    schema_name,
                    access_method,
                });
            }
        }

        // Map each (table oid, attnum) to its column name in one query over the
        // distinct owning tables.
        let mut table_oids: Vec<i64> = parsed.iter().map(|p| p.table_oid).collect();
        table_oids.sort_unstable();
        table_oids.dedup();
        let names = fetch_attribute_names(ctx.clone(), &table_oids).await?;

        for index in parsed {
            // An expression key column (attnum 0) means a functional index; the
            // resolver fallback below supplies its text, so skip structural render.
            if index.key_attnums.iter().any(|&attnum| attnum == 0) {
                continue;
            }
            let mut columns: Vec<String> = Vec::with_capacity(index.key_attnums.len());
            let mut resolvable = true;
            for attnum in &index.key_attnums {
                match names.get(&(index.table_oid, *attnum)) {
                    Some(name) => columns.push(name.clone()),
                    None => {
                        resolvable = false;
                        break;
                    }
                }
            }
            if !resolvable {
                continue;
            }
            out.insert(
                index.index_oid,
                render_create_index_statement(
                    index.is_unique,
                    &index.index_name,
                    &index.schema_name,
                    &index.table_name,
                    &index.access_method,
                    &columns,
                ),
            );
        }

        // Functional/partial indexes (and any the structural render could not
        // describe) get their text from the integration-supplied resolver, keyed by
        // the index's identity. Absent a resolver, or when it declines, the index
        // stays NULL - the default used whenever no definition resolver is installed.
        if let Some(resolver) = INDEX_DEFINITION_RESOLVER.get() {
            for (oid, identity) in &identities {
                if !out.contains_key(oid) {
                    if let Some(text) = resolver(identity) {
                        out.insert(*oid, text);
                    }
                }
            }
        }

        Ok::<_, DataFusionError>(out)
    })
}

/// Resolve `(attrelid, attnum) -> attname` for the given relations with a single
/// catalog query. Dropped and system (attnum <= 0) columns are excluded, since
/// an index key only references real, ordinary columns.
async fn fetch_attribute_names(
    ctx: Arc<SessionContext>,
    table_oids: &[i64],
) -> Result<HashMap<(i64, i64), String>> {
    let mut out: HashMap<(i64, i64), String> = HashMap::new();
    if table_oids.is_empty() {
        return Ok(out);
    }
    let in_list = table_oids
        .iter()
        .map(|o| o.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT attrelid, attnum, attname FROM pg_catalog.pg_attribute \
         WHERE attrelid IN ({in_list}) AND attnum > 0 AND attisdropped = false"
    );
    let batches = ctx.sql(&sql).await?.collect().await?;
    for batch in &batches {
        for row in 0..batch.num_rows() {
            let (Some(attrelid), Some(attnum), Some(attname)) = (
                column_oid_at(batch, "attrelid", row)?,
                column_oid_at(batch, "attnum", row)?,
                column_string_at(batch, "attname", row)?,
            ) else {
                continue;
            };
            out.insert((attrelid, attnum), attname);
        }
    }
    Ok(out)
}

/// Borrow the array for `column` out of `batch`, erroring when the catalog query
/// result has no such column.
fn column_array<'a>(batch: &'a RecordBatch, column: &str) -> Result<&'a ArrayRef> {
    batch.column_by_name(column).ok_or_else(|| {
        DataFusionError::Execution(format!("catalog query result missing column {column}"))
    })
}

/// Read an integer/OID cell from `batch[column][row]`, widening any signed or
/// unsigned 32/64-bit integer to `i64`. Returns `None` for NULL.
fn column_oid_at(batch: &RecordBatch, column: &str, row: usize) -> Result<Option<i64>> {
    oid_at(column_array(batch, column)?, row)
}

/// Read a UTF-8 cell from `batch[column][row]`. Returns `None` for NULL or a
/// non-string column. Handles dictionary-encoded string columns (which DataFusion
/// may produce for low-cardinality names like `relname`/`amname`) by unwrapping the
/// dictionary value - otherwise a valid index would silently render as NULL.
fn column_string_at(batch: &RecordBatch, column: &str, row: usize) -> Result<Option<String>> {
    fn unwrap_string(scalar: ScalarValue) -> Option<String> {
        match scalar {
            ScalarValue::Utf8(v) | ScalarValue::LargeUtf8(v) | ScalarValue::Utf8View(v) => v,
            ScalarValue::Dictionary(_, inner) => unwrap_string(*inner),
            _ => None,
        }
    }
    Ok(unwrap_string(ScalarValue::try_from_array(
        column_array(batch, column)?,
        row,
    )?))
}

/// Whether `batch[column][row]` holds a non-NULL value, for columns whose type we
/// do not otherwise decode (e.g. the `pg_node_tree` `indexprs`/`indpred`, which we
/// only need to test for presence).
fn column_is_non_null(batch: &RecordBatch, column: &str, row: usize) -> Result<bool> {
    Ok(!ScalarValue::try_from_array(column_array(batch, column)?, row)?.is_null())
}

/// Read a boolean cell from `batch[column][row]`. Returns `None` for NULL or a
/// non-boolean column.
fn column_bool_at(batch: &RecordBatch, column: &str, row: usize) -> Result<Option<bool>> {
    Ok(
        match ScalarValue::try_from_array(column_array(batch, column)?, row)? {
            ScalarValue::Boolean(v) => v,
            _ => None,
        },
    )
}

/// Read an `int2vector`-shaped list cell (e.g. `pg_index.indkey`) from
/// `batch[column][row]` as the contained attribute numbers. Returns `None` for a
/// NULL cell or a non-list column.
fn index_key_attnums_at(batch: &RecordBatch, column: &str, row: usize) -> Result<Option<Vec<i64>>> {
    let array = column_array(batch, column)?;
    let list = match array.as_any().downcast_ref::<arrow::array::ListArray>() {
        Some(list) => list,
        None => return Ok(None),
    };
    if list.is_null(row) {
        return Ok(None);
    }
    let element = list.value(row);
    let mut attnums = Vec::with_capacity(element.len());
    for i in 0..element.len() {
        attnums.push(oid_at(&element, i)?.unwrap_or(0));
    }
    Ok(Some(attnums))
}

/// `pg_catalog.pg_get_indexdef(oid)` reconstructs the `CREATE INDEX` text for a
/// plain index from the live catalog at call time, mirroring
/// [`PgGetUserById`]'s batched catalog-lookup pattern. Functional/partial index
/// expressions are left NULL unless an integration-installed resolver supplies
/// their text.
struct PgGetIndexDef {
    sig: Signature,
    ctx: Arc<SessionContext>,
}

impl PgGetIndexDef {
    /// Build the UDF over a clone of the session context it queries the catalog
    /// through, accepting any signed/unsigned 32/64-bit OID argument.
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
}

impl std::fmt::Debug for PgGetIndexDef {
    /// Format without the (non-Debug) session context.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgGetIndexDef").finish()
    }
}

impl PartialEq for PgGetIndexDef {
    /// Two instances are equal when they share the same session context.
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.ctx, &other.ctx)
    }
}

impl Eq for PgGetIndexDef {}

impl std::hash::Hash for PgGetIndexDef {
    /// Hash by the identity of the shared session context.
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.ctx) as usize).hash(state);
    }
}

impl ScalarUDFImpl for PgGetIndexDef {
    /// The schema-qualified function name.
    fn name(&self) -> &str {
        "pg_catalog.pg_get_indexdef"
    }

    /// The accepted argument signature (a single OID).
    fn signature(&self) -> &Signature {
        &self.sig
    }

    /// `pg_get_indexdef` returns the `CREATE INDEX` text as `text`.
    fn return_type(&self, _arg_types: &[ArrowDataType]) -> Result<ArrowDataType> {
        Ok(ArrowDataType::Utf8)
    }

    /// Decode every row's index OID, resolve the DISTINCT ones with a small,
    /// fixed number of catalog queries, then emit the `CREATE INDEX` text per row
    /// (NULL where the OID is NULL, unknown, or a functional/partial index).
    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let arrays = ColumnarValue::values_to_arrays(&args.args)?;
        let arr = &arrays[0];
        let len = arr.len();

        let mut oids: Vec<Option<i64>> = Vec::with_capacity(len);
        for i in 0..len {
            oids.push(oid_at(arr, i)?);
        }

        let mut distinct: Vec<i64> = oids.iter().flatten().copied().collect();
        distinct.sort_unstable();
        distinct.dedup();
        let defs = fetch_index_definitions(self.ctx.clone(), &distinct)?;

        let mut builder = StringBuilder::with_capacity(len, 64 * len.max(1));
        for oid in oids {
            match oid.and_then(|oid| defs.get(&oid)) {
                Some(def) => builder.append_value(def),
                None => builder.append_null(),
            }
        }
        Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
    }
}

/// Register `pg_get_indexdef(oid)`, which reconstructs the `CREATE INDEX` text
/// for a plain index from live catalog rows. Functional/partial indexes return
/// NULL until their expression text is supplied by an installed definition resolver.
pub fn register_pg_get_indexdef(ctx: &SessionContext) -> Result<()> {
    let udf = ScalarUDF::new_from_impl(PgGetIndexDef::new(Arc::new(ctx.clone())))
        .with_aliases(["pg_get_indexdef"]);
    ctx.register_udf(udf);
    Ok(())
}

/// Identifies the view (or materialized view) whose definition SQL a
/// [`ViewDefinitionResolver`] is asked to produce. `oid` is the view's
/// `pg_class.oid`; `schema` and `name` are its `pg_namespace.nspname` /
/// `pg_class.relname`, resolved from the live catalog so the resolver can key on
/// human-readable identity rather than a synthetic OID.
#[derive(Clone, Debug)]
pub struct ViewIdentity {
    /// The view's `pg_class.oid`.
    pub oid: i64,
    /// The schema (namespace) the view lives in.
    pub schema: String,
    /// The view's relation name.
    pub name: String,
}

/// A callback the embedding application installs to supply a database object's
/// definition text at `pg_get_*def` call time, keyed by an identity of type `I`
/// (e.g. [`ViewIdentity`], [`IndexIdentity`]). It returns the SQL the integration
/// wants clients to see (it owns the text and may build it however it likes) or
/// `None` to leave the definition NULL. pg_catalog never deparses node trees;
/// these callbacks are the sole source of definition text. Defaulting to NULL when
/// no resolver is installed is the contract for every definition resolver.
pub type DefinitionResolver<I> = Arc<dyn Fn(&I) -> Option<String> + Send + Sync>;

/// A process-wide slot holding the optional [`DefinitionResolver`] for one kind of
/// object. One shared implementation backs every "integration supplies definitions"
/// resolver, so views, index expressions, and future kinds get identical
/// install / clear / read-at-call-time semantics with no duplicated machinery.
struct DefinitionResolverSlot<I> {
    slot: std::sync::RwLock<Option<DefinitionResolver<I>>>,
}

impl<I> DefinitionResolverSlot<I> {
    /// An empty slot - no resolver installed, so every definition is NULL.
    fn new() -> Self {
        Self {
            slot: std::sync::RwLock::new(None),
        }
    }

    /// Install `resolver`, replacing any previously installed one.
    fn set(&self, resolver: DefinitionResolver<I>) {
        *self
            .slot
            .write()
            .expect("definition resolver lock poisoned") = Some(resolver);
    }

    /// Remove any installed resolver.
    fn clear(&self) {
        *self
            .slot
            .write()
            .expect("definition resolver lock poisoned") = None;
    }

    /// A clone of the installed resolver, if any, taken WITHOUT holding the lock
    /// while it runs - the resolver is integration code we must not call under our
    /// lock.
    fn get(&self) -> Option<DefinitionResolver<I>> {
        self.slot
            .read()
            .expect("definition resolver lock poisoned")
            .clone()
    }
}

/// Resolver supplying view definition SQL; see [`set_view_definition_resolver`].
pub type ViewDefinitionResolver = DefinitionResolver<ViewIdentity>;

/// The process-wide view-definition resolver, read by `pg_get_viewdef` at call
/// time. The live `pg_views` view binds the UDF when its plan is created, so the
/// resolver-backed UDF is registered once during session construction and reads
/// this slot on every call - installing or changing the resolver later still flows
/// through.
static VIEW_DEFINITION_RESOLVER: Lazy<DefinitionResolverSlot<ViewIdentity>> =
    Lazy::new(DefinitionResolverSlot::new);

/// Install the [`ViewDefinitionResolver`] that `pg_get_viewdef` consults, replacing
/// any previously installed one. The embedding application calls this (typically
/// once at startup) so views, `pg_views.definition`, and
/// `information_schema.views.view_definition` report integration-supplied SQL.
pub fn set_view_definition_resolver(resolver: ViewDefinitionResolver) {
    VIEW_DEFINITION_RESOLVER.set(resolver);
}

/// Remove any installed [`ViewDefinitionResolver`], so `pg_get_viewdef` returns NULL
/// again. Primarily for tests that must not leak a resolver into other tests.
pub fn clear_view_definition_resolver() {
    VIEW_DEFINITION_RESOLVER.clear();
}

/// Identifies the index whose `CREATE INDEX` text an [`IndexDefinitionResolver`] is
/// asked to produce. Used only for functional and partial indexes, which carry a
/// node-tree expression pg_catalog cannot render structurally. `oid` is the index's
/// `pg_class.oid`; `schema` and `table` name the indexed relation; `name` is the
/// index's own relation name.
#[derive(Clone, Debug)]
pub struct IndexIdentity {
    /// The index's `pg_class.oid`.
    pub oid: i64,
    /// The schema (namespace) of the indexed relation.
    pub schema: String,
    /// The indexed relation's name.
    pub table: String,
    /// The index's own relation name.
    pub name: String,
}

/// Resolver supplying the `CREATE INDEX` text for indexes pg_catalog cannot render
/// structurally (functional/partial indexes); see
/// [`set_index_definition_resolver`].
pub type IndexDefinitionResolver = DefinitionResolver<IndexIdentity>;

/// The process-wide index-definition resolver, consulted by `pg_get_indexdef` only
/// for indexes it cannot render from structured catalog data alone (functional and
/// partial indexes). Plain indexes are always rendered structurally and ignore it.
static INDEX_DEFINITION_RESOLVER: Lazy<DefinitionResolverSlot<IndexIdentity>> =
    Lazy::new(DefinitionResolverSlot::new);

/// Install the [`IndexDefinitionResolver`] that `pg_get_indexdef` consults for
/// functional/partial indexes, replacing any previously installed one. Plain
/// indexes are rendered structurally from the catalog and never reach the resolver;
/// a functional/partial index is NULL when no resolver is installed or it declines.
pub fn set_index_definition_resolver(resolver: IndexDefinitionResolver) {
    INDEX_DEFINITION_RESOLVER.set(resolver);
}

/// Remove any installed [`IndexDefinitionResolver`], so functional/partial indexes
/// return NULL again. Primarily for tests that must not leak a resolver.
pub fn clear_index_definition_resolver() {
    INDEX_DEFINITION_RESOLVER.clear();
}

/// A process-wide slot holding an optional integration callback, read at call
/// time. Like [`DefinitionResolverSlot`] but for callbacks that return values
/// other than definition text (a sequence's last value, a row-security flag, ...).
struct CallableSlot<F> {
    slot: std::sync::RwLock<Option<F>>,
}

impl<F: Clone> CallableSlot<F> {
    /// An empty slot - no callback installed, so the function uses its stub default.
    fn new() -> Self {
        Self {
            slot: std::sync::RwLock::new(None),
        }
    }

    /// Install `callback`, replacing any previously installed one.
    fn set(&self, callback: F) {
        *self.slot.write().expect("callable slot lock poisoned") = Some(callback);
    }

    /// Remove any installed callback.
    fn clear(&self) {
        *self.slot.write().expect("callable slot lock poisoned") = None;
    }

    /// A clone of the installed callback, taken without holding the lock while it
    /// runs (the callback is integration code we must not call under our lock).
    fn get(&self) -> Option<F> {
        self.slot
            .read()
            .expect("callable slot lock poisoned")
            .clone()
    }
}

/// Read a scalar function's first OID argument as one `Option<i64>` per row,
/// accepting any integer width (OIDs are 32-bit in this catalog but can arrive
/// widened). Shared by the OID-keyed, resolver-backed functions below.
fn oid_arg_as_i64(args: &[ColumnarValue]) -> Result<Vec<Option<i64>>> {
    use arrow::array::Int64Array;
    use arrow::compute::cast;
    use arrow::datatypes::DataType;
    let arrays = ColumnarValue::values_to_arrays(args)?;
    let first = arrays
        .first()
        .ok_or_else(|| DataFusionError::Execution("missing oid argument".into()))?;
    let int64 = cast(first, &DataType::Int64)?;
    let oids = int64
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| DataFusionError::Execution("oid argument not castable to int64".into()))?;
    Ok((0..oids.len())
        .map(|i| (!oids.is_null(i)).then(|| oids.value(i)))
        .collect())
}

/// Callback giving a sequence's last value by sequence OID, or `None` when the
/// integration cannot supply it; see [`set_pg_sequence_last_value_resolver`].
pub type SequenceLastValueResolver = Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>;

/// The process-wide resolver `pg_sequence_last_value` consults at call time. A
/// static catalog has no running sequences, so with no resolver the function is
/// NULL - the value PostgreSQL also reports for a sequence not yet read.
static SEQUENCE_LAST_VALUE_RESOLVER: Lazy<CallableSlot<SequenceLastValueResolver>> =
    Lazy::new(CallableSlot::new);

/// Install the callback `pg_sequence_last_value(oid)` consults, replacing any
/// previously installed one. An embedding fronting real sequences reports their
/// last values through it; without it the function is NULL.
pub fn set_pg_sequence_last_value_resolver(resolver: SequenceLastValueResolver) {
    SEQUENCE_LAST_VALUE_RESOLVER.set(resolver);
}

/// Remove any installed [`SequenceLastValueResolver`], so `pg_sequence_last_value`
/// returns NULL again. Primarily for tests that must not leak a resolver.
pub fn clear_pg_sequence_last_value_resolver() {
    SEQUENCE_LAST_VALUE_RESOLVER.clear();
}

/// Callback answering whether row-level security is active for a relation OID for
/// the current user; see [`set_row_security_active_resolver`].
pub type RowSecurityActiveResolver = Arc<dyn Fn(i64) -> bool + Send + Sync>;

/// The process-wide resolver `row_security_active` consults at call time. With no
/// resolver the function is false, matching a catalog that enforces no policy.
static ROW_SECURITY_ACTIVE_RESOLVER: Lazy<CallableSlot<RowSecurityActiveResolver>> =
    Lazy::new(CallableSlot::new);

/// Install the callback `row_security_active(oid)` consults, replacing any
/// previously installed one. An embedding enforcing row-level security reports it
/// through this; without it the function is false.
pub fn set_row_security_active_resolver(resolver: RowSecurityActiveResolver) {
    ROW_SECURITY_ACTIVE_RESOLVER.set(resolver);
}

/// Remove any installed [`RowSecurityActiveResolver`], so `row_security_active`
/// returns false again. Primarily for tests that must not leak a resolver.
pub fn clear_row_security_active_resolver() {
    ROW_SECURITY_ACTIVE_RESOLVER.clear();
}

/// Register `pg_sequence_last_value(oid)`, giving each sequence's last value via the
/// installed [`SequenceLastValueResolver`], or NULL when none is installed.
pub fn register_pg_sequence_last_value(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, Int64Array};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{
        ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature, Volatility,
    };

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct PgSequenceLastValue {
        sig: Signature,
    }

    impl ScalarUDFImpl for PgSequenceLastValue {
        /// The fully-qualified function name.
        fn name(&self) -> &str {
            "pg_catalog.pg_sequence_last_value"
        }
        /// One argument of any type (the sequence OID / regclass).
        fn signature(&self) -> &Signature {
            &self.sig
        }
        /// Always `bigint`.
        fn return_type(&self, _t: &[DataType]) -> Result<DataType> {
            Ok(DataType::Int64)
        }
        /// The resolver's value per sequence OID, or NULL with no resolver.
        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            let oids = oid_arg_as_i64(&args.args)?;
            let resolver = SEQUENCE_LAST_VALUE_RESOLVER.get();
            let values: Int64Array = oids
                .iter()
                .map(|oid| match (oid, &resolver) {
                    (Some(oid), Some(resolve)) => resolve(*oid),
                    _ => None,
                })
                .collect();
            Ok(ColumnarValue::Array(Arc::new(values) as ArrayRef))
        }
    }

    let udf = ScalarUDF::new_from_impl(PgSequenceLastValue {
        sig: Signature::one_of(vec![TypeSignature::Any(1)], Volatility::Stable),
    })
    .with_aliases(["pg_sequence_last_value"]);
    ctx.register_udf(udf);
    Ok(())
}

/// Register `row_security_active(oid)`, answering via the installed
/// [`RowSecurityActiveResolver`], or false when none is installed.
pub fn register_row_security_active(ctx: &SessionContext) -> Result<()> {
    use arrow::array::{ArrayRef, BooleanArray};
    use arrow::datatypes::DataType;
    use datafusion::logical_expr::{
        ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature, Volatility,
    };

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct RowSecurityActive {
        sig: Signature,
    }

    impl ScalarUDFImpl for RowSecurityActive {
        /// The fully-qualified function name.
        fn name(&self) -> &str {
            "pg_catalog.row_security_active"
        }
        /// One argument of any type (the relation OID / regclass).
        fn signature(&self) -> &Signature {
            &self.sig
        }
        /// Always boolean.
        fn return_type(&self, _t: &[DataType]) -> Result<DataType> {
            Ok(DataType::Boolean)
        }
        /// The resolver's answer per relation OID, or false with no resolver.
        fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
            let oids = oid_arg_as_i64(&args.args)?;
            let resolver = ROW_SECURITY_ACTIVE_RESOLVER.get();
            let values: BooleanArray = oids
                .iter()
                .map(|oid| match (oid, &resolver) {
                    (Some(oid), Some(resolve)) => Some(resolve(*oid)),
                    _ => Some(false),
                })
                .collect();
            Ok(ColumnarValue::Array(Arc::new(values) as ArrayRef))
        }
    }

    let udf = ScalarUDF::new_from_impl(RowSecurityActive {
        sig: Signature::one_of(vec![TypeSignature::Any(1)], Volatility::Stable),
    })
    .with_aliases(["row_security_active"]);
    ctx.register_udf(udf);
    Ok(())
}

/// Resolve each view / materialized-view OID to its `(schema, name)` identity with
/// a single catalog query, skipping OIDs that name no view. Mirrors
/// [`fetch_index_definitions`]'s batched distinct-OID lookup.
fn fetch_view_identities(
    ctx: Arc<SessionContext>,
    oids: &[i64],
) -> Result<HashMap<i64, ViewIdentity>> {
    let mut out: HashMap<i64, ViewIdentity> = HashMap::new();
    if oids.is_empty() {
        return Ok(out);
    }
    let in_list = oids
        .iter()
        .map(|o| o.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    run_catalog_query(async move {
        let sql = format!(
            "SELECT c.oid, c.relname, n.nspname AS schemaname \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.oid IN ({in_list}) AND c.relkind IN ('v', 'm')"
        );
        let batches = ctx.sql(&sql).await?.collect().await?;
        for batch in &batches {
            for row in 0..batch.num_rows() {
                let (Some(oid), Some(name), Some(schema)) = (
                    column_oid_at(batch, "oid", row)?,
                    column_string_at(batch, "relname", row)?,
                    column_string_at(batch, "schemaname", row)?,
                ) else {
                    continue;
                };
                out.insert(oid, ViewIdentity { oid, schema, name });
            }
        }
        Ok::<_, DataFusionError>(out)
    })
}

/// `pg_catalog.pg_get_viewdef(oid [, pretty])`. It resolves each row's view OID to
/// its identity from the live catalog, then asks the process-wide
/// [`ViewDefinitionResolver`] for the definition text - returning NULL where the
/// OID is NULL, names no view, no resolver is installed, or the resolver declines.
struct PgGetViewDef {
    sig: Signature,
    ctx: Arc<SessionContext>,
}

impl PgGetViewDef {
    /// Build the UDF over a clone of the session context it resolves identities
    /// through. Accepts the one-argument `pg_get_viewdef(oid)` and two-argument
    /// `pg_get_viewdef(oid, pretty)` forms; the OID is read from the first argument
    /// and the pretty flag is ignored (the resolver owns formatting).
    fn new(ctx: Arc<SessionContext>) -> Self {
        Self {
            sig: Signature::one_of(
                vec![TypeSignature::Any(1), TypeSignature::Any(2)],
                Volatility::Stable,
            ),
            ctx,
        }
    }
}

impl std::fmt::Debug for PgGetViewDef {
    /// Format without the (non-Debug) session context.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgGetViewDef").finish()
    }
}

impl PartialEq for PgGetViewDef {
    /// Two instances are equal when they share the same session context.
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.ctx, &other.ctx)
    }
}

impl Eq for PgGetViewDef {}

impl std::hash::Hash for PgGetViewDef {
    /// Hash by the identity of the shared session context.
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.ctx) as usize).hash(state);
    }
}

impl ScalarUDFImpl for PgGetViewDef {
    /// The schema-qualified function name.
    fn name(&self) -> &str {
        "pg_catalog.pg_get_viewdef"
    }

    /// The accepted argument signature (an OID, optionally with a pretty flag).
    fn signature(&self) -> &Signature {
        &self.sig
    }

    /// `pg_get_viewdef` returns the view definition as `text`.
    fn return_type(&self, _arg_types: &[ArrowDataType]) -> Result<ArrowDataType> {
        Ok(ArrowDataType::Utf8)
    }

    /// Decode every row's view OID, resolve the DISTINCT ones to identities with a
    /// single catalog query, then ask the installed resolver for each view's text
    /// (NULL where the OID is NULL, names no view, no resolver is installed, or the
    /// resolver supplies nothing).
    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let arrays = ColumnarValue::values_to_arrays(&args.args)?;
        let arr = &arrays[0];
        let len = arr.len();

        let mut oids: Vec<Option<i64>> = Vec::with_capacity(len);
        for i in 0..len {
            oids.push(oid_at(arr, i)?);
        }

        let resolver = VIEW_DEFINITION_RESOLVER.get();
        let identities = match &resolver {
            // No resolver installed: every definition is NULL, and there is no
            // need to query the catalog for identities.
            None => HashMap::new(),
            Some(_) => {
                let mut distinct: Vec<i64> = oids.iter().flatten().copied().collect();
                distinct.sort_unstable();
                distinct.dedup();
                fetch_view_identities(self.ctx.clone(), &distinct)?
            }
        };

        let mut builder = StringBuilder::with_capacity(len, 64 * len.max(1));
        for oid in oids {
            let def = match (&resolver, oid.and_then(|oid| identities.get(&oid))) {
                (Some(resolve), Some(identity)) => resolve(identity),
                _ => None,
            };
            match def {
                Some(text) => builder.append_value(&text),
                None => builder.append_null(),
            }
        }
        Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
    }
}

/// pg_catalog.pg_get_function_result(oid) -> text
pub fn register_pg_get_function_result(ctx: &SessionContext) -> Result<()> {
    register_null_text_stub(ctx, "pg_catalog.pg_get_function_result", 1)
}

/// pg_catalog.pg_get_function_sqlbody(oid) -> text
pub fn register_pg_get_function_sqlbody(ctx: &SessionContext) -> Result<()> {
    register_null_text_stub(ctx, "pg_catalog.pg_get_function_sqlbody", 1)
}

/// pg_catalog.encode(bytea, text) -> text
///
/// Placeholder implementation returning NULL.
pub fn register_encode(ctx: &SessionContext) -> Result<()> {
    register_null_text_stub(ctx, "pg_catalog.encode", 2)
}

/// pg_catalog.pg_get_triggerdef(oid [, bool]) -> text
///
/// Returns NULL placeholder.
pub fn register_pg_get_triggerdef(ctx: &SessionContext) -> Result<()> {
    register_null_text_stub_accepting_arities(ctx, "pg_catalog.pg_get_triggerdef", &[1, 2])
}

/// pg_catalog.pg_get_ruledef(oid [, bool]) -> text
///
/// Returns NULL placeholder.
pub fn register_pg_get_ruledef(ctx: &SessionContext) -> Result<()> {
    register_null_text_stub_accepting_arities(ctx, "pg_catalog.pg_get_ruledef", &[1, 2])
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

    // Registered as `available_extension_versions`, NOT `pg_available_extension_versions`:
    // the catalog also declares a *view* named `pg_available_extension_versions` whose
    // body calls this function, and DataFusion resolves a table function and a relation
    // of the same name ambiguously (the function shadows the view in FROM clauses). The
    // view body's call is renamed to this internal name by
    // `rewrite_available_extension_versions_source` so the view owns its name.
    ctx.register_udtf(
        "available_extension_versions",
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
    ctx.register_udtf(
        "pg_catalog.pg_get_keywords",
        Arc::new(KeywordsTableFunc { schema }),
    );
    Ok(())
}

/// Register a relation-size stub `<base_name>(oid) -> int8` that returns zero for
/// now, under `pg_catalog.<base_name>` with the bare name as an alias.
fn register_zero_relation_size(ctx: &SessionContext, base_name: &'static str) -> Result<()> {
    use arrow::datatypes::DataType;
    use datafusion::common::ScalarValue;
    use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
    use std::sync::Arc;

    let fun = |_args: &[ColumnarValue]| -> Result<ColumnarValue> {
        Ok(ColumnarValue::Scalar(ScalarValue::Int64(Some(0))))
    };

    let udf = create_udf(
        &format!("pg_catalog.{base_name}"),
        vec![DataType::Int64],
        DataType::Int64,
        Volatility::Stable,
        Arc::new(fun),
    )
    .with_aliases([base_name]);
    ctx.register_udf(udf);
    Ok(())
}

/// Register `pg_relation_size(oid)` returning zero for now.
pub fn register_pg_relation_size(ctx: &SessionContext) -> Result<()> {
    register_zero_relation_size(ctx, "pg_relation_size")
}

/// Register `pg_total_relation_size(oid)` returning zero for now.
pub fn register_pg_total_relation_size(ctx: &SessionContext) -> Result<()> {
    register_zero_relation_size(ctx, "pg_total_relation_size")
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_run_catalog_query_nested_does_not_deadlock() {
        // Three levels of nested catalog queries on a 2-worker runtime - the size
        // of the old bounded CATALOG_QUERY_RT, which deadlocked here because each
        // blocked caller parked a worker with no free worker left to run the inner
        // task. block_in_place hands the worker back, so the runtime grows and this
        // completes. (A hang here is the regression this guards.)
        let value = run_catalog_query(async {
            run_catalog_query(async { run_catalog_query(async { 42 }) })
        });
        assert_eq!(value, 42);
    }

    #[test]
    fn test_render_create_index_statement() {
        // Unique multi-column index, schema-qualified table, matching the
        // real-PostgreSQL pg_get_indexdef text reproduced in pg_indexes.
        assert_eq!(
            render_create_index_statement(
                true,
                "pg_proc_proname_args_nsp_index",
                "pg_catalog",
                "pg_proc",
                "btree",
                &[
                    "proname".to_string(),
                    "proargtypes".to_string(),
                    "pronamespace".to_string(),
                ],
            ),
            "CREATE UNIQUE INDEX pg_proc_proname_args_nsp_index ON pg_catalog.pg_proc \
             USING btree (proname, proargtypes, pronamespace)"
        );
        // Non-unique single-column index omits the UNIQUE keyword.
        assert_eq!(
            render_create_index_statement(
                false,
                "pg_index_indrelid_index",
                "pg_catalog",
                "pg_index",
                "btree",
                &["indrelid".to_string()],
            ),
            "CREATE INDEX pg_index_indrelid_index ON pg_catalog.pg_index USING btree (indrelid)"
        );
        // Mixed-case / special identifiers are double-quoted (and embedded quotes
        // doubled); plain lowercase ones are left bare.
        assert_eq!(
            render_create_index_statement(
                false,
                "MyIdx",
                "public",
                "My Table",
                "btree",
                &["Col\"1".to_string(), "id".to_string()],
            ),
            "CREATE INDEX \"MyIdx\" ON public.\"My Table\" USING btree (\"Col\"\"1\", id)"
        );
    }

    #[test]
    fn test_quote_identifier_if_needed() {
        assert_eq!(quote_identifier_if_needed("pg_proc"), "pg_proc");
        assert_eq!(quote_identifier_if_needed("_x9"), "_x9");
        assert_eq!(quote_identifier_if_needed("Mixed"), "\"Mixed\"");
        assert_eq!(quote_identifier_if_needed("has space"), "\"has space\"");
        assert_eq!(quote_identifier_if_needed("1leading"), "\"1leading\"");
        assert_eq!(quote_identifier_if_needed("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn test_pg_numeric_precision_formula() {
        // Fixed widths for the integer/float types.
        assert_eq!(pg_numeric_precision(Some(OID_INT2), Some(-1)), Some(16));
        assert_eq!(pg_numeric_precision(Some(OID_INT4), Some(-1)), Some(32));
        assert_eq!(pg_numeric_precision(Some(OID_INT8), Some(-1)), Some(64));
        assert_eq!(pg_numeric_precision(Some(OID_FLOAT4), Some(-1)), Some(24));
        assert_eq!(pg_numeric_precision(Some(OID_FLOAT8), Some(-1)), Some(53));
        // numeric(10,2) has typmod ((10 << 16) | 2) + 4 -> precision 10, scale 2.
        let numeric_10_2 = ((10i64 << 16) | 2) + 4;
        assert_eq!(
            pg_numeric_precision(Some(OID_NUMERIC), Some(numeric_10_2)),
            Some(10)
        );
        assert_eq!(
            pg_numeric_scale(Some(OID_NUMERIC), Some(numeric_10_2)),
            Some(2)
        );
        // Unbounded numeric and non-numeric types are NULL.
        assert_eq!(pg_numeric_precision(Some(OID_NUMERIC), Some(-1)), None);
        assert_eq!(pg_numeric_precision(Some(OID_TEXT), Some(-1)), None);
        assert_eq!(pg_numeric_precision(None, Some(-1)), None);
    }

    #[test]
    fn test_pg_numeric_radix_and_scale_formula() {
        assert_eq!(
            pg_numeric_precision_radix(Some(OID_INT4), Some(-1)),
            Some(2)
        );
        assert_eq!(
            pg_numeric_precision_radix(Some(OID_NUMERIC), Some(-1)),
            Some(10)
        );
        assert_eq!(pg_numeric_precision_radix(Some(OID_TEXT), Some(-1)), None);
        assert_eq!(pg_numeric_scale(Some(OID_INT2), Some(-1)), Some(0));
        assert_eq!(pg_numeric_scale(Some(OID_FLOAT4), Some(-1)), None);
    }

    #[test]
    fn test_pg_datetime_precision_formula() {
        assert_eq!(pg_datetime_precision(Some(OID_DATE), Some(-1)), Some(0));
        // time/timestamp default to 6, or the explicit typmod.
        assert_eq!(
            pg_datetime_precision(Some(OID_TIMESTAMPTZ), Some(-1)),
            Some(6)
        );
        assert_eq!(pg_datetime_precision(Some(OID_TIMESTAMP), Some(3)), Some(3));
        assert_eq!(pg_datetime_precision(Some(OID_INTERVAL), Some(-1)), Some(6));
        assert_eq!(pg_datetime_precision(Some(OID_INT4), Some(-1)), None);
    }

    #[test]
    fn test_pg_char_length_formula() {
        // varchar(3): typmod = 3 + 4 -> max length 3, octet length 3 * 4 (UTF-8).
        assert_eq!(pg_char_max_length(Some(OID_VARCHAR), Some(7)), Some(3));
        assert_eq!(pg_char_octet_length(Some(OID_VARCHAR), Some(7)), Some(12));
        // Unbounded text/varchar octet length is 1 GiB.
        assert_eq!(
            pg_char_octet_length(Some(OID_TEXT), Some(-1)),
            Some(1 << 30)
        );
        assert_eq!(
            pg_char_octet_length(Some(OID_VARCHAR), Some(-1)),
            Some(1 << 30)
        );
        // Unbounded varchar has no declared max length; non-char types are NULL.
        assert_eq!(pg_char_max_length(Some(OID_VARCHAR), Some(-1)), None);
        assert_eq!(pg_char_max_length(Some(OID_INT4), Some(-1)), None);
        assert_eq!(pg_char_octet_length(Some(OID_INT4), Some(-1)), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_pg_precision_helpers_via_sql() {
        // The registered UDFs compute precision/length from a column of (typid,
        // typmod) pairs, matching the per-row formulas above.
        let ctx = SessionContext::new();
        register_pg_numeric_helpers(&ctx).unwrap();
        register_pg_char_max_length(&ctx).unwrap();
        register_pg_char_octet_length(&ctx).unwrap();

        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("typid", DataType::Int32, false),
                Field::new("typmod", DataType::Int32, false),
            ])),
            vec![
                Arc::new(Int32Array::from(vec![23, 700, 25, 1043])),
                Arc::new(Int32Array::from(vec![-1, -1, -1, 7])),
            ],
        )
        .unwrap();
        ctx.register_batch("t", batch).unwrap();

        let rows = ctx
            .sql(
                "SELECT information_schema._pg_numeric_precision(typid, typmod) AS prec, \
                        information_schema._pg_char_octet_length(typid, typmod) AS oct \
                 FROM t ORDER BY typid",
            )
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let b = &rows[0];
        let prec = b.column(0).as_any().downcast_ref::<Int32Array>().unwrap();
        let oct = b.column(1).as_any().downcast_ref::<Int32Array>().unwrap();
        // typid order after sort: 23 int4, 25 text, 700 float4, 1043 varchar(3).
        assert_eq!(prec.value(0), 32); // int4 precision
        assert!(oct.is_null(0)); // int4 has no octet length
        assert!(prec.is_null(1)); // text has no numeric precision
        assert_eq!(oct.value(1), 1 << 30); // text octet length 1 GiB
        assert_eq!(prec.value(2), 24); // float4 precision
        assert_eq!(oct.value(3), 12); // varchar(3) octet length 3 * 4
    }

    #[test]
    fn test_format_type_name_resolves_via_typname_lookup() {
        // A representative pg_type oid -> typname slice (oid 19 = name,
        // 1184 = timestamptz, 23 = int4, _int4 = the int4 array).
        let by_oid: HashMap<i64, String> = [
            (19, "name"),
            (23, "int4"),
            (1184, "timestamptz"),
            (1043, "varchar"),
            (1007, "_int4"),
        ]
        .into_iter()
        .map(|(o, n)| (o, n.to_string()))
        .collect();

        // The cases that used to print the bare OID now resolve to a SQL name.
        assert_eq!(format_type_name(19, None, &by_oid), "name");
        assert_eq!(
            format_type_name(1184, None, &by_oid),
            "timestamp with time zone"
        );
        assert_eq!(format_type_name(23, None, &by_oid), "integer");
        // typmod-carrying types apply the modifier; arrays append `[]`.
        assert_eq!(
            format_type_name(1043, Some(14), &by_oid),
            "character varying(10)"
        );
        assert_eq!(format_type_name(1007, None, &by_oid), "integer[]");
        // An OID with no pg_type row falls back to its numeric text.
        assert_eq!(format_type_name(999999, None, &by_oid), "999999");
    }

    #[test]
    fn test_sql_name_for_typname_canonical_and_passthrough() {
        // Built-ins map to their SQL-standard spelling.
        assert_eq!(sql_name_for_typname("int4", None), "integer");
        assert_eq!(sql_name_for_typname("bool", None), "boolean");
        assert_eq!(
            sql_name_for_typname("numeric", Some(4 + ((10 << 16) | 2))),
            "numeric(10,2)"
        );
        // A non-standard type prints its own typname unchanged.
        assert_eq!(sql_name_for_typname("name", None), "name");
        assert_eq!(sql_name_for_typname("citext", None), "citext");
    }

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

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
    async fn test_pg_age_always_one() -> datafusion::error::Result<()> {
        use arrow::array::Int64Array;

        // 1  fresh context
        let ctx = SessionContext::new();

        // 2  register the helper we just added
        register_scalar_pg_age(&ctx)?;

        // 3  run any query that invokes the function
        let batches = ctx
            .sql("SELECT pg_catalog.age(123::BIGINT) AS v;")
            .await?
            .collect()
            .await?;

        // 4  assert we got the constant 1 back
        let arr = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();

        assert_eq!(arr.value(0), 1);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
    async fn available_extension_versions_empty() -> Result<()> {
        let ctx = SessionContext::new();
        register_pg_available_extension_versions(&ctx)?;
        let batches = ctx
            .sql("SELECT * FROM available_extension_versions()")
            .await?
            .collect()
            .await?;
        assert_eq!(batches[0].num_rows(), 0);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
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

    #[tokio::test(flavor = "multi_thread")]
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
