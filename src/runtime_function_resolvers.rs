//! Integration-supplied implementations of the catalog's runtime functions.
//!
//! Many `pg_catalog` views call server-runtime functions - per-object statistics
//! accessors (`pg_stat_get_numscans(oid)`, ...), live-state table functions
//! (`pg_lock_status()`, `pg_stat_get_activity(oid)`, ...) - that a static catalog
//! cannot compute. This module lets the embedding application supply them.
//!
//! The contract is explicit: each function has its own named, typed setter, e.g.
//! `set_pg_stat_get_numscans_resolver(Arc<dyn Fn(i64) -> Option<i64>>)`. With no
//! resolver installed the function returns NULL (or, for table functions, no rows),
//! so the views are correct - just empty - until an integration plugs values in.
//!
//! The implementation is generated from one declarative line per function (see the
//! `scalar_resolvers!` table below): the generic [`ResolverSlot`] holds the callback,
//! [`DynScalarUdf`] is the single UDF that reads a slot at call time, and the macro
//! emits each function's slot, typed setter/clearer, and registration. Adding a
//! function is one line; there is no per-function engine plumbing to copy.

use std::sync::{Arc, RwLock};

use arrow::array::{
    Array, ArrayRef, BooleanArray, Float64Array, Int32Array, Int64Array, ListBuilder, RecordBatch,
    StringArray, StringBuilder, TimestampMicrosecondArray,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::catalog::TableFunctionImpl;
use datafusion::datasource::{MemTable, TableProvider};
use datafusion::error::Result;
use datafusion::logical_expr::{
    ColumnarValue, Expr, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
    Volatility,
};
use datafusion::prelude::SessionContext;
use once_cell::sync::Lazy;

/// A process-wide slot holding an optional integration callback, read at call time.
///
/// Shared by every resolver-backed function. The callback is cloned out before being
/// invoked so integration code never runs while the lock is held.
pub(crate) struct ResolverSlot<F> {
    slot: RwLock<Option<F>>,
}

impl<F: Clone> ResolverSlot<F> {
    /// An empty slot - no callback installed, so the function uses its default.
    pub(crate) fn new() -> Self {
        Self {
            slot: RwLock::new(None),
        }
    }

    /// Install `callback`, replacing any previously installed one.
    pub(crate) fn set(&self, callback: F) {
        *self.slot.write().expect("resolver slot poisoned") = Some(callback);
    }

    /// Remove any installed callback.
    pub(crate) fn clear(&self) {
        *self.slot.write().expect("resolver slot poisoned") = None;
    }

    /// A clone of the installed callback, if any.
    pub(crate) fn get(&self) -> Option<F> {
        self.slot.read().expect("resolver slot poisoned").clone()
    }
}

/// One scalar UDF whose behaviour is a boxed closure, so a single type backs every
/// resolver-driven scalar function. Identity (for DataFusion's plan dedup) is the
/// qualified name; the closure is not part of it.
pub(crate) struct DynScalarUdf {
    qualified: String,
    aliases: Vec<String>,
    return_type: DataType,
    signature: Signature,
    eval: Box<dyn Fn(ScalarFunctionArgs) -> Result<ColumnarValue> + Send + Sync>,
}

impl DynScalarUdf {
    /// Build a UDF named `qualified` (with bare `alias`) returning `return_type`,
    /// taking `arity` arguments of any type, evaluated by `eval`.
    pub(crate) fn new(
        qualified: &str,
        alias: &str,
        return_type: DataType,
        arity: usize,
        eval: Box<dyn Fn(ScalarFunctionArgs) -> Result<ColumnarValue> + Send + Sync>,
    ) -> Self {
        // A zero-argument call matches `Nullary`, not `Any(0)` - the latter rejects an
        // empty argument list, so the no-argument stat functions would not resolve.
        let type_signature = if arity == 0 {
            TypeSignature::Nullary
        } else {
            TypeSignature::Any(arity)
        };
        Self {
            qualified: qualified.to_string(),
            aliases: vec![alias.to_string()],
            return_type,
            signature: Signature::one_of(vec![type_signature], Volatility::Stable),
            eval,
        }
    }
}

impl std::fmt::Debug for DynScalarUdf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynScalarUdf")
            .field("qualified", &self.qualified)
            .finish()
    }
}

impl PartialEq for DynScalarUdf {
    fn eq(&self, other: &Self) -> bool {
        self.qualified == other.qualified
    }
}
impl Eq for DynScalarUdf {}
impl std::hash::Hash for DynScalarUdf {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.qualified.hash(state);
    }
}

impl ScalarUDFImpl for DynScalarUdf {
    fn name(&self) -> &str {
        &self.qualified
    }
    fn aliases(&self) -> &[String] {
        &self.aliases
    }
    fn signature(&self) -> &Signature {
        &self.signature
    }
    fn return_type(&self, _args: &[DataType]) -> Result<DataType> {
        Ok(self.return_type.clone())
    }
    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        (self.eval)(args)
    }
}

/// The timestamptz type the timestamp-returning stat functions report (UTC).
fn timestamptz() -> DataType {
    DataType::Timestamp(TimeUnit::Microsecond, Some("+00:00".into()))
}

/// Read scalar UDF argument `index` as one `Option<i64>` per row, widening any integer
/// type (OIDs are 32-bit here but may arrive widened). An index past the supplied
/// arguments yields all-NULL.
fn int_arg(args: &ScalarFunctionArgs, index: usize) -> Result<Vec<Option<i64>>> {
    use arrow::array::Int64Array as I64;
    use arrow::compute::cast;
    let arrays = ColumnarValue::values_to_arrays(&args.args)?;
    let Some(col) = arrays.get(index) else {
        return Ok(vec![None; args.number_rows]);
    };
    let widened = cast(col, &DataType::Int64)?;
    let ints = widened
        .as_any()
        .downcast_ref::<I64>()
        .expect("cast to int64 yields Int64Array");
    Ok((0..ints.len())
        .map(|i| (!ints.is_null(i)).then(|| ints.value(i)))
        .collect())
}

/// Read a scalar UDF's first OID argument as one `Option<i64>` per row.
fn oid_args(args: &ScalarFunctionArgs) -> Result<Vec<Option<i64>>> {
    int_arg(args, 0)
}

/// Declare a batch of scalar runtime functions. Each line is the explicit contract:
/// `name (arg) -> ret`. The macro generates, per function, the typed resolver alias,
/// its slot, `set_<name>_resolver` / `clear_<name>_resolver`, and a registration that
/// installs a [`DynScalarUdf`] reading the slot. `register_all_scalar_resolvers`
/// registers every one.
macro_rules! scalar_resolvers {
    ( $( $fn:ident ( $($arg:ident)? ) -> $ret:ident $( = $default:expr )? ; )* ) => {
        paste::paste! {
            $(
                #[doc = concat!("Resolver supplying `pg_catalog.", stringify!($fn),
                    "`; see [`set_", stringify!($fn), "_resolver`].")]
                pub type [<$fn:camel Resolver>] = scalar_resolver_ty!($($arg)?, $ret);

                static [<$fn:upper _SLOT>]: Lazy<ResolverSlot<[<$fn:camel Resolver>]>> =
                    Lazy::new(ResolverSlot::new);

                #[doc = concat!("Install the callback `", stringify!($fn),
                    "` consults, replacing any previously installed one.")]
                pub fn [<set_ $fn _resolver>](resolver: [<$fn:camel Resolver>]) {
                    [<$fn:upper _SLOT>].set(resolver);
                }

                #[doc = concat!("Remove any installed `", stringify!($fn), "` resolver.")]
                pub fn [<clear_ $fn _resolver>]() {
                    [<$fn:upper _SLOT>].clear();
                }

                fn [<register_ $fn>](ctx: &SessionContext) {
                    let udf = DynScalarUdf::new(
                        concat!("pg_catalog.", stringify!($fn)),
                        stringify!($fn),
                        scalar_datatype!($ret),
                        scalar_arity!($($arg)?),
                        Box::new(scalar_eval!(
                            $($arg)?, $ret, [<$fn:upper _SLOT>], scalar_default!($($default)?)
                        )),
                    );
                    ctx.register_udf(ScalarUDF::new_from_impl(udf));
                }
            )*

            /// Register every macro-generated scalar resolver on `ctx`. The public entry
            /// point [`register_all_scalar_resolvers`] also adds the hand-written ones
            /// whose signatures fall outside this macro's shape.
            fn register_generated_scalar_resolvers(ctx: &SessionContext) {
                $( [<register_ $fn>](ctx); )*
            }
        }
    };
}

// The per-row fallback a function reports when no resolver is installed: `None` (SQL
// NULL) unless a function declares an explicit default (e.g. the visibility predicates
// default to `true`).
macro_rules! scalar_default {
    () => { None };
    ($default:expr) => { Some($default) };
}

// Map a `(arg) -> ret` shape to the resolver callback type.
macro_rules! scalar_resolver_ty {
    (oid, int8) => { Arc<dyn Fn(i64) -> Option<i64> + Send + Sync> };
    (oid, int4) => { Arc<dyn Fn(i64) -> Option<i32> + Send + Sync> };
    (oid, float8) => { Arc<dyn Fn(i64) -> Option<f64> + Send + Sync> };
    (oid, bool) => { Arc<dyn Fn(i64) -> Option<bool> + Send + Sync> };
    (oid, timestamptz) => { Arc<dyn Fn(i64) -> Option<i64> + Send + Sync> };
    (, int8) => { Arc<dyn Fn() -> Option<i64> + Send + Sync> };
    (, float8) => { Arc<dyn Fn() -> Option<f64> + Send + Sync> };
    (, timestamptz) => { Arc<dyn Fn() -> Option<i64> + Send + Sync> };
}

macro_rules! scalar_datatype {
    (int8) => { DataType::Int64 };
    (int4) => { DataType::Int32 };
    (float8) => { DataType::Float64 };
    (bool) => { DataType::Boolean };
    (timestamptz) => { timestamptz() };
}

macro_rules! scalar_arity {
    (oid) => { 1 };
    () => { 0 };
}

// Build the eval closure for a `(arg) -> ret` shape, reading `$slot` at call time and
// mapping the resolver over the rows. A row reports `$default` when no resolver is
// installed and NULL when its OID input is NULL; an installed resolver's answer is
// reported as-is. The `@oid` / `@noarg` inner arms hold the shared array-building; the
// public arms select the array type, with the timestamptz cases adding the UTC zone the
// declared type carries.
macro_rules! scalar_eval {
    (@oid $array:ty, $slot:ident, $default:expr) => {
        move |args: ScalarFunctionArgs| -> Result<ColumnarValue> {
            let oids = oid_args(&args)?;
            let resolver = $slot.get();
            let out: $array = oids
                .iter()
                .map(|oid| match (oid, &resolver) {
                    (Some(oid), Some(resolve)) => resolve(*oid),
                    (Some(_), None) => $default,
                    (None, _) => None,
                })
                .collect();
            Ok(ColumnarValue::Array(Arc::new(out) as ArrayRef))
        }
    };
    (@noarg $array:ty, $slot:ident, $default:expr) => {
        move |args: ScalarFunctionArgs| -> Result<ColumnarValue> {
            let value = match $slot.get() {
                Some(resolve) => resolve(),
                None => $default,
            };
            let out: $array = std::iter::repeat(value).take(args.number_rows).collect();
            Ok(ColumnarValue::Array(Arc::new(out) as ArrayRef))
        }
    };
    (oid, int8, $slot:ident, $default:expr) => { scalar_eval!(@oid Int64Array, $slot, $default) };
    (oid, int4, $slot:ident, $default:expr) => { scalar_eval!(@oid Int32Array, $slot, $default) };
    (oid, float8, $slot:ident, $default:expr) => { scalar_eval!(@oid Float64Array, $slot, $default) };
    (oid, bool, $slot:ident, $default:expr) => { scalar_eval!(@oid BooleanArray, $slot, $default) };
    (oid, timestamptz, $slot:ident, $default:expr) => {
        move |args: ScalarFunctionArgs| -> Result<ColumnarValue> {
            let oids = oid_args(&args)?;
            let resolver = $slot.get();
            let out = oids
                .iter()
                .map(|oid| match (oid, &resolver) {
                    (Some(oid), Some(resolve)) => resolve(*oid),
                    (Some(_), None) => $default,
                    (None, _) => None,
                })
                .collect::<TimestampMicrosecondArray>()
                .with_timezone("+00:00");
            Ok(ColumnarValue::Array(Arc::new(out) as ArrayRef))
        }
    };
    (, int8, $slot:ident, $default:expr) => { scalar_eval!(@noarg Int64Array, $slot, $default) };
    (, float8, $slot:ident, $default:expr) => { scalar_eval!(@noarg Float64Array, $slot, $default) };
    (, timestamptz, $slot:ident, $default:expr) => {
        move |args: ScalarFunctionArgs| -> Result<ColumnarValue> {
            let value = match $slot.get() {
                Some(resolve) => resolve(),
                None => $default,
            };
            let out = std::iter::repeat(value)
                .take(args.number_rows)
                .collect::<TimestampMicrosecondArray>()
                .with_timezone("+00:00");
            Ok(ColumnarValue::Array(Arc::new(out) as ArrayRef))
        }
    };
}

// Every `pg_stat_get_*` accessor a view calls, with its real PostgreSQL signature
// (from the seed pg_proc; see claude-scripts/missing_functions.md). With no resolver
// installed each returns NULL, so the stat views are real views over no statistics
// until an integration supplies them.
//
// The `*_is_visible` predicates answer "is this object reachable on the search path
// without schema-qualification". A static catalog has no live search_path, so they
// default to `true` (everything visible) rather than NULL; an integration can install
// search-path-aware logic. The functions with multi-argument or array signatures
// (`pg_indexam_progress_phasename`, `pg_get_statisticsobjdef_expressions`) fall outside
// this macro's shape and are registered just below it.
scalar_resolvers! {
    pg_table_is_visible (oid) -> bool = true;
    pg_function_is_visible (oid) -> bool = true;
    pg_type_is_visible (oid) -> bool = true;
    pg_stat_get_analyze_count (oid) -> int8;
    pg_stat_get_autoanalyze_count (oid) -> int8;
    pg_stat_get_autovacuum_count (oid) -> int8;
    pg_stat_get_bgwriter_buf_written_clean () -> int8;
    pg_stat_get_bgwriter_maxwritten_clean () -> int8;
    pg_stat_get_bgwriter_stat_reset_time () -> timestamptz;
    pg_stat_get_blocks_fetched (oid) -> int8;
    pg_stat_get_blocks_hit (oid) -> int8;
    pg_stat_get_buf_alloc () -> int8;
    pg_stat_get_checkpointer_buffers_written () -> int8;
    pg_stat_get_checkpointer_num_requested () -> int8;
    pg_stat_get_checkpointer_num_timed () -> int8;
    pg_stat_get_checkpointer_restartpoints_performed () -> int8;
    pg_stat_get_checkpointer_restartpoints_requested () -> int8;
    pg_stat_get_checkpointer_restartpoints_timed () -> int8;
    pg_stat_get_checkpointer_stat_reset_time () -> timestamptz;
    pg_stat_get_checkpointer_sync_time () -> float8;
    pg_stat_get_checkpointer_write_time () -> float8;
    pg_stat_get_db_active_time (oid) -> float8;
    pg_stat_get_db_blk_read_time (oid) -> float8;
    pg_stat_get_db_blk_write_time (oid) -> float8;
    pg_stat_get_db_blocks_fetched (oid) -> int8;
    pg_stat_get_db_blocks_hit (oid) -> int8;
    pg_stat_get_db_checksum_failures (oid) -> int8;
    pg_stat_get_db_checksum_last_failure (oid) -> timestamptz;
    pg_stat_get_db_conflict_all (oid) -> int8;
    pg_stat_get_db_conflict_bufferpin (oid) -> int8;
    pg_stat_get_db_conflict_lock (oid) -> int8;
    pg_stat_get_db_conflict_logicalslot (oid) -> int8;
    pg_stat_get_db_conflict_snapshot (oid) -> int8;
    pg_stat_get_db_conflict_startup_deadlock (oid) -> int8;
    pg_stat_get_db_conflict_tablespace (oid) -> int8;
    pg_stat_get_db_deadlocks (oid) -> int8;
    pg_stat_get_db_idle_in_transaction_time (oid) -> float8;
    pg_stat_get_db_numbackends (oid) -> int4;
    pg_stat_get_db_session_time (oid) -> float8;
    pg_stat_get_db_sessions (oid) -> int8;
    pg_stat_get_db_sessions_abandoned (oid) -> int8;
    pg_stat_get_db_sessions_fatal (oid) -> int8;
    pg_stat_get_db_sessions_killed (oid) -> int8;
    pg_stat_get_db_stat_reset_time (oid) -> timestamptz;
    pg_stat_get_db_temp_bytes (oid) -> int8;
    pg_stat_get_db_temp_files (oid) -> int8;
    pg_stat_get_db_tuples_deleted (oid) -> int8;
    pg_stat_get_db_tuples_fetched (oid) -> int8;
    pg_stat_get_db_tuples_inserted (oid) -> int8;
    pg_stat_get_db_tuples_returned (oid) -> int8;
    pg_stat_get_db_tuples_updated (oid) -> int8;
    pg_stat_get_db_xact_commit (oid) -> int8;
    pg_stat_get_db_xact_rollback (oid) -> int8;
    pg_stat_get_dead_tuples (oid) -> int8;
    pg_stat_get_function_calls (oid) -> int8;
    pg_stat_get_function_self_time (oid) -> float8;
    pg_stat_get_function_total_time (oid) -> float8;
    pg_stat_get_ins_since_vacuum (oid) -> int8;
    pg_stat_get_last_analyze_time (oid) -> timestamptz;
    pg_stat_get_last_autoanalyze_time (oid) -> timestamptz;
    pg_stat_get_last_autovacuum_time (oid) -> timestamptz;
    pg_stat_get_last_vacuum_time (oid) -> timestamptz;
    pg_stat_get_lastscan (oid) -> timestamptz;
    pg_stat_get_live_tuples (oid) -> int8;
    pg_stat_get_mod_since_analyze (oid) -> int8;
    pg_stat_get_numscans (oid) -> int8;
    pg_stat_get_tuples_deleted (oid) -> int8;
    pg_stat_get_tuples_fetched (oid) -> int8;
    pg_stat_get_tuples_hot_updated (oid) -> int8;
    pg_stat_get_tuples_inserted (oid) -> int8;
    pg_stat_get_tuples_newpage_updated (oid) -> int8;
    pg_stat_get_tuples_returned (oid) -> int8;
    pg_stat_get_tuples_updated (oid) -> int8;
    pg_stat_get_vacuum_count (oid) -> int8;
    pg_stat_get_xact_function_calls (oid) -> int8;
    pg_stat_get_xact_function_self_time (oid) -> float8;
    pg_stat_get_xact_function_total_time (oid) -> float8;
    pg_stat_get_xact_numscans (oid) -> int8;
    pg_stat_get_xact_tuples_deleted (oid) -> int8;
    pg_stat_get_xact_tuples_fetched (oid) -> int8;
    pg_stat_get_xact_tuples_hot_updated (oid) -> int8;
    pg_stat_get_xact_tuples_inserted (oid) -> int8;
    pg_stat_get_xact_tuples_newpage_updated (oid) -> int8;
    pg_stat_get_xact_tuples_returned (oid) -> int8;
    pg_stat_get_xact_tuples_updated (oid) -> int8;
}

/// Resolver supplying `pg_indexam_progress_phasename(oid, int8)`; see
/// [`set_pg_indexam_progress_phasename_resolver`]. Given an index access method's OID
/// and a method-specific phase number it returns the human-readable phase name.
pub type PgIndexamProgressPhasenameResolver = Arc<dyn Fn(i64, i64) -> Option<String> + Send + Sync>;

static PG_INDEXAM_PROGRESS_PHASENAME_SLOT: Lazy<ResolverSlot<PgIndexamProgressPhasenameResolver>> =
    Lazy::new(ResolverSlot::new);

/// Install the callback `pg_indexam_progress_phasename` consults, replacing any
/// previously installed one. With none installed the function returns NULL.
pub fn set_pg_indexam_progress_phasename_resolver(resolver: PgIndexamProgressPhasenameResolver) {
    PG_INDEXAM_PROGRESS_PHASENAME_SLOT.set(resolver);
}

/// Remove any installed `pg_indexam_progress_phasename` resolver.
pub fn clear_pg_indexam_progress_phasename_resolver() {
    PG_INDEXAM_PROGRESS_PHASENAME_SLOT.clear();
}

/// Register `pg_indexam_progress_phasename(oid, int8) -> text` on `ctx`. Its two-argument
/// signature falls outside `scalar_resolvers!`, so it is built directly on the shared
/// [`DynScalarUdf`]; the eval reads both integer arguments and maps the resolver over the
/// rows (NULL where absent).
fn register_pg_indexam_progress_phasename(ctx: &SessionContext) {
    let eval = move |args: ScalarFunctionArgs| -> Result<ColumnarValue> {
        let methods = int_arg(&args, 0)?;
        let phases = int_arg(&args, 1)?;
        let resolver = PG_INDEXAM_PROGRESS_PHASENAME_SLOT.get();
        let out: StringArray = methods
            .iter()
            .zip(phases.iter())
            .map(|(method, phase)| match (method, phase, &resolver) {
                (Some(method), Some(phase), Some(resolve)) => resolve(*method, *phase),
                _ => None,
            })
            .collect();
        Ok(ColumnarValue::Array(Arc::new(out) as ArrayRef))
    };
    let udf = DynScalarUdf::new(
        "pg_catalog.pg_indexam_progress_phasename",
        "pg_indexam_progress_phasename",
        DataType::Utf8,
        2,
        Box::new(eval),
    );
    ctx.register_udf(ScalarUDF::new_from_impl(udf));
}

/// Resolver supplying `pg_get_statisticsobjdef_expressions(oid)`; see
/// [`set_pg_get_statisticsobjdef_expressions_resolver`]. Given an extended-statistics
/// object's OID it returns that object's expression definitions as a text array.
pub type PgGetStatisticsobjdefExpressionsResolver =
    Arc<dyn Fn(i64) -> Option<Vec<String>> + Send + Sync>;

static PG_GET_STATISTICSOBJDEF_EXPRESSIONS_SLOT: Lazy<
    ResolverSlot<PgGetStatisticsobjdefExpressionsResolver>,
> = Lazy::new(ResolverSlot::new);

/// Install the callback `pg_get_statisticsobjdef_expressions` consults, replacing any
/// previously installed one. With none installed the function returns NULL.
pub fn set_pg_get_statisticsobjdef_expressions_resolver(
    resolver: PgGetStatisticsobjdefExpressionsResolver,
) {
    PG_GET_STATISTICSOBJDEF_EXPRESSIONS_SLOT.set(resolver);
}

/// Remove any installed `pg_get_statisticsobjdef_expressions` resolver.
pub fn clear_pg_get_statisticsobjdef_expressions_resolver() {
    PG_GET_STATISTICSOBJDEF_EXPRESSIONS_SLOT.clear();
}

/// Register `pg_get_statisticsobjdef_expressions(oid) -> _text` on `ctx`. Its text-array
/// return type falls outside `scalar_resolvers!`, so it is built directly on the shared
/// [`DynScalarUdf`]; the eval builds one list per row (NULL where the resolver is absent
/// or returns NULL).
fn register_pg_get_statisticsobjdef_expressions(ctx: &SessionContext) {
    let eval = move |args: ScalarFunctionArgs| -> Result<ColumnarValue> {
        let oids = int_arg(&args, 0)?;
        let resolver = PG_GET_STATISTICSOBJDEF_EXPRESSIONS_SLOT.get();
        let mut builder = ListBuilder::new(StringBuilder::new());
        for oid in &oids {
            match (oid, &resolver) {
                (Some(oid), Some(resolve)) => match resolve(*oid) {
                    Some(exprs) => {
                        for expr in exprs {
                            builder.values().append_value(expr);
                        }
                        builder.append(true);
                    }
                    None => builder.append(false),
                },
                _ => builder.append(false),
            }
        }
        Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
    };
    let udf = DynScalarUdf::new(
        "pg_catalog.pg_get_statisticsobjdef_expressions",
        "pg_get_statisticsobjdef_expressions",
        DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
        1,
        Box::new(eval),
    );
    ctx.register_udf(ScalarUDF::new_from_impl(udf));
}

/// Register every scalar runtime-function resolver on `ctx`: the macro-generated stat
/// accessors and visibility predicates, plus the two hand-written functions whose
/// signatures (multi-argument, array-returning) fall outside the macro's shape.
pub fn register_all_scalar_resolvers(ctx: &SessionContext) {
    register_generated_scalar_resolvers(ctx);
    register_pg_indexam_progress_phasename(ctx);
    register_pg_get_statisticsobjdef_expressions(ctx);
}

// --- Set-returning (table) functions ---------------------------------------------
//
// Each is a real PostgreSQL function returning a row set whose schema is fixed (see
// claude-scripts/missing_functions.md). An integration installs a callback returning
// typed rows; with none installed the function yields no rows, so the view that calls
// it is a real, empty view. As with the scalar side, one generic provider plus a
// macro generates every function's typed row struct, setter, schema, and registration.

/// One table function whose rows come from an installed resolver (empty when none),
/// so a single type backs every set-returning catalog function. The schema is fixed
/// at registration; `build` reads the slot and materializes the rows on each call.
pub(crate) struct DynTableUdf {
    schema: SchemaRef,
    build: Arc<dyn Fn() -> RecordBatch + Send + Sync>,
}

impl std::fmt::Debug for DynTableUdf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynTableUdf").field("schema", &self.schema).finish()
    }
}

impl TableFunctionImpl for DynTableUdf {
    fn call(&self, _exprs: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        let batch = (self.build)();
        Ok(Arc::new(MemTable::try_new(self.schema.clone(), vec![vec![batch]])?))
    }
}

// Map a column kind to its Rust field type, Arrow data type, and array-building
// expression. The set of kinds is deliberately small; richer PostgreSQL types are
// represented as text in the row struct (the function is empty by default, so this
// only has to let the calling view plan).
macro_rules! col_rust_ty {
    (text) => { String };
    (bool) => { bool };
    (int4) => { i32 };
    (int8) => { i64 };
    (float8) => { f64 };
    (timestamptz) => { i64 };
}

macro_rules! col_datatype {
    (text) => { DataType::Utf8 };
    (bool) => { DataType::Boolean };
    (int4) => { DataType::Int32 };
    (int8) => { DataType::Int64 };
    (float8) => { DataType::Float64 };
    (timestamptz) => { timestamptz() };
}

macro_rules! col_array {
    (text, $rows:ident, $col:ident) => {
        Arc::new($rows.iter().map(|r| r.$col.clone()).collect::<StringArray>()) as ArrayRef
    };
    (bool, $rows:ident, $col:ident) => {
        Arc::new($rows.iter().map(|r| r.$col).collect::<BooleanArray>()) as ArrayRef
    };
    (int4, $rows:ident, $col:ident) => {
        Arc::new($rows.iter().map(|r| r.$col).collect::<Int32Array>()) as ArrayRef
    };
    (int8, $rows:ident, $col:ident) => {
        Arc::new($rows.iter().map(|r| r.$col).collect::<Int64Array>()) as ArrayRef
    };
    (float8, $rows:ident, $col:ident) => {
        Arc::new($rows.iter().map(|r| r.$col).collect::<Float64Array>()) as ArrayRef
    };
    (timestamptz, $rows:ident, $col:ident) => {
        Arc::new(
            $rows
                .iter()
                .map(|r| r.$col)
                .collect::<TimestampMicrosecondArray>()
                .with_timezone("+00:00"),
        ) as ArrayRef
    };
}

/// Declare a batch of set-returning runtime functions. Each line is the explicit
/// contract: `name -> { col: kind, ... }`. The macro generates, per function, the
/// typed row struct (`<Name>Row` with `Option` fields), the resolver alias, its slot,
/// `set_<name>_resolver` / `clear_<name>_resolver`, the fixed Arrow schema, and the
/// table-function registration. `register_all_table_resolvers` registers every one.
macro_rules! table_resolvers {
    ( $( $fn:ident -> { $( $col:ident : $kind:ident ),* $(,)? } ; )* ) => {
        paste::paste! {
            $(
                #[doc = concat!("One row of `", stringify!($fn), "`.")]
                #[derive(Clone, Debug, Default)]
                pub struct [<$fn:camel Row>] {
                    $( #[doc = concat!("`", stringify!($col), "` column.")]
                       pub $col: Option<col_rust_ty!($kind)>, )*
                }

                #[doc = concat!("Resolver supplying `", stringify!($fn),
                    "` rows; see [`set_", stringify!($fn), "_resolver`].")]
                pub type [<$fn:camel Resolver>] =
                    Arc<dyn Fn() -> Vec<[<$fn:camel Row>]> + Send + Sync>;

                static [<$fn:upper _SLOT>]: Lazy<ResolverSlot<[<$fn:camel Resolver>]>> =
                    Lazy::new(ResolverSlot::new);

                #[doc = concat!("Install the callback `", stringify!($fn),
                    "` consults, replacing any previously installed one.")]
                pub fn [<set_ $fn _resolver>](resolver: [<$fn:camel Resolver>]) {
                    [<$fn:upper _SLOT>].set(resolver);
                }

                #[doc = concat!("Remove any installed `", stringify!($fn), "` resolver.")]
                pub fn [<clear_ $fn _resolver>]() {
                    [<$fn:upper _SLOT>].clear();
                }

                fn [<register_ $fn>](ctx: &SessionContext) {
                    let schema: SchemaRef = Arc::new(Schema::new(vec![
                        $( Field::new(stringify!($col), col_datatype!($kind), true), )*
                    ]));
                    let build_schema = schema.clone();
                    let build: Arc<dyn Fn() -> RecordBatch + Send + Sync> = Arc::new(move || {
                        let rows = [<$fn:upper _SLOT>].get().map(|resolve| resolve()).unwrap_or_default();
                        let columns: Vec<ArrayRef> = vec![
                            $( col_array!($kind, rows, $col), )*
                        ];
                        RecordBatch::try_new(build_schema.clone(), columns)
                            .expect("generated row schema matches its columns")
                    });
                    ctx.register_udtf(stringify!($fn), Arc::new(DynTableUdf { schema, build }));
                }
            )*

            /// Register every set-returning runtime-function resolver on `ctx`.
            pub fn register_all_table_resolvers(ctx: &SessionContext) {
                $( [<register_ $fn>](ctx); )*
            }
        }
    };
}

table_resolvers! {
    pg_cursor -> { name: text, statement: text, is_holdable: bool, is_binary: bool, is_scrollable: bool, creation_time: timestamptz };
    pg_get_backend_memory_contexts -> { name: text, ident: text, parent: text, level: int4, total_bytes: int8, total_nblocks: int8, free_bytes: int8, free_chunks: int8, used_bytes: int8 };
    pg_get_publication_tables -> { pubid: int8, relid: int8, attrs: text, qual: text };
    pg_get_replication_slots -> { slot_name: text, plugin: text, slot_type: text, datoid: int8, temporary: bool, active: bool, active_pid: int4, xmin: int8, catalog_xmin: int8, restart_lsn: text, confirmed_flush_lsn: text, wal_status: text, safe_wal_size: int8, two_phase: bool, inactive_since: timestamptz, conflicting: bool, invalidation_reason: text, failover: bool, synced: bool };
    pg_get_shmem_allocations -> { name: text, off: int8, size: int8, allocated_size: int8 };
    pg_get_wait_events -> { r#type: text, name: text, description: text };
    pg_lock_status -> { locktype: text, database: int8, relation: int8, page: int4, tuple: int4, virtualxid: text, transactionid: int8, classid: int8, objid: int8, objsubid: int4, virtualtransaction: text, pid: int4, mode: text, granted: bool, fastpath: bool, waitstart: timestamptz };
    pg_mcv_list_items -> { index: int4, values: text, nulls: text, frequency: float8, base_frequency: float8 };
    pg_prepared_statement -> { name: text, statement: text, prepare_time: timestamptz, parameter_types: text, result_types: text, from_sql: bool, generic_plans: int8, custom_plans: int8 };
    pg_prepared_xact -> { transaction: int8, gid: text, prepared: timestamptz, ownerid: int8, dbid: int8 };
    pg_show_all_file_settings -> { sourcefile: text, sourceline: int4, seqno: int4, name: text, setting: text, applied: bool, error: text };
    pg_show_replication_origin_status -> { local_id: int8, external_id: text, remote_lsn: text, local_lsn: text };
    pg_stat_get_activity -> { datid: int8, pid: int4, usesysid: int8, application_name: text, state: text, query: text, wait_event_type: text, wait_event: text, xact_start: timestamptz, query_start: timestamptz, backend_start: timestamptz, state_change: timestamptz, client_addr: text, client_hostname: text, client_port: int4, backend_xid: int8, backend_xmin: int8, backend_type: text, ssl: bool, sslversion: text, sslcipher: text, sslbits: int4, ssl_client_dn: text, ssl_client_serial: text, ssl_issuer_dn: text, gss_auth: bool, gss_princ: text, gss_enc: bool, gss_delegation: bool, leader_pid: int4, query_id: int8 };
    pg_stat_get_io -> { backend_type: text, object: text, context: text, reads: int8, read_time: float8, writes: int8, write_time: float8, writebacks: int8, writeback_time: float8, extends: int8, extend_time: float8, op_bytes: int8, hits: int8, evictions: int8, reuses: int8, fsyncs: int8, fsync_time: float8, stats_reset: timestamptz };
    pg_stat_get_progress_info -> { pid: int4, datid: int8, relid: int8, param1: int8, param2: int8, param3: int8, param4: int8, param5: int8, param6: int8, param7: int8, param8: int8, param9: int8, param10: int8, param11: int8, param12: int8, param13: int8, param14: int8, param15: int8, param16: int8, param17: int8, param18: int8, param19: int8, param20: int8 };
    pg_stat_get_recovery_prefetch -> { stats_reset: timestamptz, prefetch: int8, hit: int8, skip_init: int8, skip_new: int8, skip_fpw: int8, skip_rep: int8, wal_distance: int4, block_distance: int4, io_depth: int4 };
    pg_stat_get_slru -> { name: text, blks_zeroed: int8, blks_hit: int8, blks_read: int8, blks_written: int8, blks_exists: int8, flushes: int8, truncates: int8, stats_reset: timestamptz };
    pg_stat_get_subscription -> { subid: int8, relid: int8, pid: int4, leader_pid: int4, received_lsn: text, last_msg_send_time: timestamptz, last_msg_receipt_time: timestamptz, latest_end_lsn: text, latest_end_time: timestamptz, worker_type: text };
    pg_stat_get_wal_senders -> { pid: int4, state: text, sent_lsn: text, write_lsn: text, flush_lsn: text, replay_lsn: text, write_lag: text, flush_lag: text, replay_lag: text, sync_priority: int4, sync_state: text, reply_time: timestamptz };
    // These return a single composite record rather than SETOF, but their views call
    // them in the FROM clause with column aliases (e.g. `FROM pg_stat_get_archiver()
    // s(archived_count, ...)`), so the same row-set provider serves them: a resolver
    // returns the one record's columns as a single row, or none when not installed.
    pg_stat_get_archiver -> { archived_count: int8, last_archived_wal: text, last_archived_time: timestamptz, failed_count: int8, last_failed_wal: text, last_failed_time: timestamptz, stats_reset: timestamptz };
    pg_stat_get_wal -> { wal_records: int8, wal_fpi: int8, wal_bytes: text, wal_buffers_full: int8, wal_write: int8, wal_sync: int8, wal_write_time: float8, wal_sync_time: float8, stats_reset: timestamptz };
    pg_stat_get_wal_receiver -> { pid: int4, status: text, receive_start_lsn: text, receive_start_tli: int4, written_lsn: text, flushed_lsn: text, received_tli: int4, last_msg_send_time: timestamptz, last_msg_receipt_time: timestamptz, latest_end_lsn: text, latest_end_time: timestamptz, slot_name: text, sender_host: text, sender_port: int4, conninfo: text };
    pg_stat_get_replication_slot -> { slot_name: text, spill_txns: int8, spill_count: int8, spill_bytes: int8, stream_txns: int8, stream_count: int8, stream_bytes: int8, total_txns: int8, total_bytes: int8, stats_reset: timestamptz };
    pg_stat_get_subscription_stats -> { subid: int8, apply_error_count: int8, sync_error_count: int8, stats_reset: timestamptz };
}
