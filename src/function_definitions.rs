//! `pg_get_functiondef(oid)`: the `CREATE OR REPLACE FUNCTION` statement for a
//! function the catalog serves.
//!
//! WHY THIS EXISTS. psql's `\sf` (and `\ef`) resolve a function name to its
//! OID and then send `SELECT pg_catalog.pg_get_functiondef(<oid>)`; the
//! function did not exist, so both commands failed outright.
//!
//! THE DEFINITIONS ARE REAL, NOT INVENTED. Task 262 worried that a
//! wrong-but-plausible function body would be worse than a missing one - and
//! it would be, but none is needed: the snapshot's `pg_proc` rows carry
//! `prosrc` and every flag column captured from a real `PostgreSQL` server,
//! and for an internal or C function the body `PostgreSQL` itself prints IS
//! the `prosrc` symbol (`AS $function$now$function$`). Rendering from the
//! snapshot therefore reproduces the real server's output - byte for byte,
//! which is what the tests assert against strings captured live from the
//! bundled `PostgreSQL` 17.4.
//!
//! THE ONE HONEST GAP: 56 of the snapshot's 3330 functions are new-style SQL
//! functions (`RETURN` bodies stored as `prosqlbody` node trees, which the
//! snapshot extraction does not deparse), so their `prosrc` is empty and no
//! faithful body exists to print. Those answer NULL - a visibly missing
//! definition - rather than a fabricated one.
//!
//! EDGES, each measured against the live server rather than assumed:
//! an OID that is no function answers NULL; an aggregate's OID answers the
//! error `"name" is an aggregate function`; a window function renders with
//! `WINDOW` leading its flags; a function with no flags at all omits the
//! flags line entirely (`set_config`). The flag order is the measured one -
//! volatility, `PARALLEL`, `STRICT`, `LEAKPROOF`, `SUPPORT`, `COST`, `ROWS` -
//! and `COST` appears only when it differs from the language's default
//! (1 for internal and C, 100 otherwise), `ROWS` only for a set-returning
//! function off the 1000-row default.
//!
//! Definitions are rendered once at registration, the same snapshot trade
//! `format_type` and `pg_describe_object` make: the catalog is static once
//! built, so pre-rendering trades a megabyte of strings for never touching
//! the catalog at call time.

use std::collections::HashMap;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, StringBuilder};
use arrow::datatypes::DataType as ArrowDataType;
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::SessionContext;
use datafusion::logical_expr::{
    ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
    Volatility,
};

/// What `pg_get_functiondef` answers for one OID.
#[derive(Debug)]
enum DefinitionOutcome {
    /// The rendered `CREATE OR REPLACE FUNCTION` statement.
    Definition(String),
    /// The OID belongs to an aggregate, which `PostgreSQL` refuses by name.
    AggregateRefusal(String),
    /// The function is real but its body was not captured (a new-style SQL
    /// function whose `prosqlbody` node tree the snapshot does not deparse),
    /// so the answer is NULL rather than a fabricated definition.
    BodyNotCaptured,
}

/// One `pg_proc` row, joined to its schema and language names, as the
/// renderer consumes it.
///
/// The bools mirror `pg_proc`'s own boolean columns (`proisstrict`,
/// `proleakproof`, `prosecdef`, `proretset`) one-to-one; folding them into
/// enums would only obscure which catalog column each one reads.
#[allow(clippy::struct_excessive_bools)]
struct FunctionRow {
    name: String,
    schema: String,
    language: String,
    /// `pg_proc.prokind`: `f` plain, `a` aggregate, `w` window, `p` procedure.
    kind: String,
    /// `pg_proc.provolatile`: `i`, `s`, or `v`.
    volatility: String,
    /// `pg_proc.proparallel`: `s`, `r`, or `u`.
    parallel: String,
    is_strict: bool,
    is_leakproof: bool,
    security_definer: bool,
    returns_set: bool,
    rettype_oid: i64,
    /// `proargtypes`: the input argument type OIDs.
    argument_type_oids: Vec<i64>,
    /// `proallargtypes`: every argument's type OID when OUT/INOUT/VARIADIC
    /// modes exist; empty otherwise.
    all_argument_type_oids: Vec<i64>,
    /// `proargmodes`: one of `i`/`o`/`b`/`v`/`t` per entry of
    /// `all_argument_type_oids`; empty when every argument is plain IN.
    argument_modes: Vec<String>,
    /// `proargnames`: one name per argument, empty strings for unnamed ones.
    argument_names: Vec<String>,
    cost: f64,
    result_rows: f64,
    /// `prosupport` as the snapshot stores it: the support function's name,
    /// or `-` for none.
    support: String,
    /// `prosrc`; empty for the new-style SQL functions whose bodies the
    /// snapshot does not carry.
    source: String,
}

impl FunctionRow {
    /// Render this row the way `PostgreSQL`'s `pg_get_functiondef` prints it.
    ///
    /// `type_names` maps type OIDs to their `format_type` renderings, shared
    /// with the `format_type` UDF so argument and return types can never be
    /// spelled differently here than there.
    fn render(&self, type_names: &HashMap<i64, String>) -> DefinitionOutcome {
        if self.kind == "a" {
            return DefinitionOutcome::AggregateRefusal(self.name.clone());
        }
        if self.source.is_empty() {
            return DefinitionOutcome::BodyNotCaptured;
        }

        let type_name = |oid: i64| {
            type_names
                .get(&oid)
                .cloned()
                .unwrap_or_else(|| oid.to_string())
        };

        let mut returns = String::new();
        if self.returns_set {
            returns.push_str("SETOF ");
        }
        returns.push_str(&type_name(self.rettype_oid));

        let mut statement = format!(
            "CREATE OR REPLACE FUNCTION {}.{}({})\n RETURNS {returns}\n LANGUAGE {}\n",
            self.schema,
            self.name,
            self.rendered_argument_list(type_names),
            self.language,
        );
        let flags = self.rendered_flags();
        if !flags.is_empty() {
            statement.push(' ');
            statement.push_str(&flags);
            statement.push('\n');
        }
        statement.push_str("AS $function$");
        statement.push_str(&self.source);
        statement.push_str("$function$\n");
        DefinitionOutcome::Definition(statement)
    }

    /// The parenthesised argument list: mode keyword, name, and type per
    /// argument, exactly as `PostgreSQL` prints them (`acl aclitem[], OUT
    /// grantor oid, ...`; `VARIADIC "any"`).
    fn rendered_argument_list(&self, type_names: &HashMap<i64, String>) -> String {
        let type_name = |oid: i64| {
            type_names
                .get(&oid)
                .cloned()
                .unwrap_or_else(|| oid.to_string())
        };

        // With OUT/INOUT/VARIADIC modes present the full list lives in
        // proallargtypes; otherwise proargtypes holds the (all-IN) inputs.
        let type_oids: &[i64] = if self.all_argument_type_oids.is_empty() {
            &self.argument_type_oids
        } else {
            &self.all_argument_type_oids
        };

        let mut rendered = Vec::with_capacity(type_oids.len());
        for (index, type_oid) in type_oids.iter().enumerate() {
            let mut argument = String::new();
            match self.argument_modes.get(index).map(String::as_str) {
                Some("o") => argument.push_str("OUT "),
                Some("b") => argument.push_str("INOUT "),
                Some("v") => argument.push_str("VARIADIC "),
                // "t" belongs to RETURNS TABLE, which no snapshot function
                // uses; plain IN takes no keyword.
                _ => {}
            }
            if let Some(name) = self.argument_names.get(index) {
                if !name.is_empty() {
                    argument.push_str(name);
                    argument.push(' ');
                }
            }
            argument.push_str(&type_name(*type_oid));
            rendered.push(argument);
        }
        rendered.join(", ")
    }

    /// The flags line, in the measured order, empty when every flag is at its
    /// default - in which case the whole line is omitted.
    fn rendered_flags(&self) -> String {
        let mut flags: Vec<String> = Vec::new();
        if self.kind == "w" {
            flags.push("WINDOW".to_string());
        }
        match self.volatility.as_str() {
            "i" => flags.push("IMMUTABLE".to_string()),
            "s" => flags.push("STABLE".to_string()),
            // "v" is the default and is not printed.
            _ => {}
        }
        match self.parallel.as_str() {
            "s" => flags.push("PARALLEL SAFE".to_string()),
            "r" => flags.push("PARALLEL RESTRICTED".to_string()),
            // "u" is the default and is not printed.
            _ => {}
        }
        if self.is_strict {
            flags.push("STRICT".to_string());
        }
        if self.is_leakproof {
            flags.push("LEAKPROOF".to_string());
        }
        if self.security_definer {
            flags.push("SECURITY DEFINER".to_string());
        }
        if self.support != "-" && !self.support.is_empty() {
            flags.push(format!("SUPPORT {}", self.support));
        }
        let default_cost = if self.language == "internal" || self.language == "c" {
            1.0
        } else {
            100.0
        };
        if (self.cost - default_cost).abs() > f64::EPSILON {
            flags.push(format!("COST {}", trimmed_number(self.cost)));
        }
        if self.returns_set && (self.result_rows - 1000.0).abs() > f64::EPSILON {
            flags.push(format!("ROWS {}", trimmed_number(self.result_rows)));
        }
        flags.join(" ")
    }
}

/// `value` printed without a fractional part when it has none, as
/// `PostgreSQL` prints `COST 100` and `ROWS 10`.
fn trimmed_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

/// The list column at `column_index`, row `row`, as strings - for
/// `proargmodes` and `proargnames`. A NULL list is an empty vector.
fn string_list_at(
    batch: &arrow::record_batch::RecordBatch,
    column_index: usize,
    row: usize,
) -> Vec<String> {
    use arrow::array::{ListArray, StringArray};
    let Some(lists) = batch
        .column(column_index)
        .as_any()
        .downcast_ref::<ListArray>()
    else {
        return Vec::new();
    };
    if lists.is_null(row) {
        return Vec::new();
    }
    let values = lists.value(row);
    let Some(strings) = values.as_any().downcast_ref::<StringArray>() else {
        return Vec::new();
    };
    (0..strings.len())
        .map(|i| {
            if strings.is_null(i) {
                String::new()
            } else {
                strings.value(i).to_string()
            }
        })
        .collect()
}

/// The list column at `column_index`, row `row`, as i64s - for
/// `proargtypes` and `proallargtypes`. A NULL list is an empty vector.
fn oid_list_at(
    batch: &arrow::record_batch::RecordBatch,
    column_index: usize,
    row: usize,
) -> Vec<i64> {
    use arrow::array::{Int64Array, ListArray};
    let Some(lists) = batch
        .column(column_index)
        .as_any()
        .downcast_ref::<ListArray>()
    else {
        return Vec::new();
    };
    if lists.is_null(row) {
        return Vec::new();
    }
    let values = lists.value(row);
    let Some(oids) = values.as_any().downcast_ref::<Int64Array>() else {
        return Vec::new();
    };
    (0..oids.len()).map(|i| oids.value(i)).collect()
}

/// Render every snapshot function's definition, keyed by OID.
async fn load_rendered_definitions(
    ctx: &SessionContext,
    type_names: &HashMap<i64, String>,
) -> Result<HashMap<i64, DefinitionOutcome>> {
    use arrow::array::{BooleanArray, Float64Array, Int64Array, StringArray};

    let batches = ctx
        .sql(
            "SELECT CAST(p.oid AS BIGINT), p.proname, n.nspname, l.lanname, p.prokind, \
                    p.provolatile, p.proparallel, p.proisstrict, p.proleakproof, p.prosecdef, \
                    p.proretset, CAST(p.prorettype AS BIGINT), p.proargtypes, \
                    p.proallargtypes, p.proargmodes, p.proargnames, \
                    CAST(p.procost AS DOUBLE) AS procost, CAST(p.prorows AS DOUBLE) AS prorows, \
                    p.prosupport, p.prosrc \
             FROM pg_catalog.pg_proc p \
             JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
             JOIN pg_catalog.pg_language l ON l.oid = p.prolang",
        )
        .await?
        .collect()
        .await?;

    /// One downcast with a named error, so a snapshot schema drift is
    /// reported by column rather than as a bare panic.
    macro_rules! column {
        ($batch:expr, $index:expr, $type:ty, $name:literal) => {
            $batch
                .column($index)
                .as_any()
                .downcast_ref::<$type>()
                .ok_or_else(|| {
                    DataFusionError::Internal(concat!("pg_proc ", $name, " column type").into())
                })?
        };
    }

    let mut rendered = HashMap::new();
    for batch in &batches {
        let oids = column!(batch, 0, Int64Array, "oid");
        let names = column!(batch, 1, StringArray, "proname");
        let schemas = column!(batch, 2, StringArray, "nspname");
        let languages = column!(batch, 3, StringArray, "lanname");
        let kinds = column!(batch, 4, StringArray, "prokind");
        let volatilities = column!(batch, 5, StringArray, "provolatile");
        let parallels = column!(batch, 6, StringArray, "proparallel");
        let stricts = column!(batch, 7, BooleanArray, "proisstrict");
        let leakproofs = column!(batch, 8, BooleanArray, "proleakproof");
        let secdefs = column!(batch, 9, BooleanArray, "prosecdef");
        let retsets = column!(batch, 10, BooleanArray, "proretset");
        let rettypes = column!(batch, 11, Int64Array, "prorettype");
        let costs = column!(batch, 16, Float64Array, "procost");
        let row_estimates = column!(batch, 17, Float64Array, "prorows");
        let supports = column!(batch, 18, StringArray, "prosupport");
        let sources = column!(batch, 19, StringArray, "prosrc");

        let text_or_empty = |array: &StringArray, row: usize| {
            if array.is_null(row) {
                String::new()
            } else {
                array.value(row).to_string()
            }
        };

        for row in 0..batch.num_rows() {
            if oids.is_null(row) || names.is_null(row) {
                continue;
            }
            let function = FunctionRow {
                name: text_or_empty(names, row),
                schema: text_or_empty(schemas, row),
                language: text_or_empty(languages, row),
                kind: text_or_empty(kinds, row),
                volatility: text_or_empty(volatilities, row),
                parallel: text_or_empty(parallels, row),
                is_strict: !stricts.is_null(row) && stricts.value(row),
                is_leakproof: !leakproofs.is_null(row) && leakproofs.value(row),
                security_definer: !secdefs.is_null(row) && secdefs.value(row),
                returns_set: !retsets.is_null(row) && retsets.value(row),
                rettype_oid: if rettypes.is_null(row) {
                    0
                } else {
                    rettypes.value(row)
                },
                argument_type_oids: oid_list_at(batch, 12, row),
                all_argument_type_oids: oid_list_at(batch, 13, row),
                argument_modes: string_list_at(batch, 14, row),
                argument_names: string_list_at(batch, 15, row),
                cost: if costs.is_null(row) {
                    0.0
                } else {
                    costs.value(row)
                },
                result_rows: if row_estimates.is_null(row) {
                    0.0
                } else {
                    row_estimates.value(row)
                },
                support: text_or_empty(supports, row),
                source: text_or_empty(sources, row),
            };
            rendered.insert(oids.value(row), function.render(type_names));
        }
    }
    Ok(rendered)
}

/// The `pg_get_functiondef` UDF over the pre-rendered definitions.
#[derive(Debug)]
struct PgGetFunctiondef {
    /// One integer argument, either width - catalog columns arrive as
    /// `Int32`, literals and psql's resolved OIDs as `Int64`.
    signature: Signature,
    definitions: Arc<HashMap<i64, DefinitionOutcome>>,
}

impl PartialEq for PgGetFunctiondef {
    /// Equality by identity of the rendered map, which is what `DataFusion`'s
    /// UDF registry compares registrations by.
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.definitions, &other.definitions)
    }
}
impl Eq for PgGetFunctiondef {}
impl std::hash::Hash for PgGetFunctiondef {
    /// Hash by map identity, matching the equality above.
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.definitions).hash(state);
    }
}

impl ScalarUDFImpl for PgGetFunctiondef {
    /// The unqualified name; the `pg_catalog.` spelling is an alias.
    fn name(&self) -> &'static str {
        "pg_get_functiondef"
    }
    /// One integer OID argument.
    fn signature(&self) -> &Signature {
        &self.signature
    }
    /// Always text (a definition), NULL where none exists.
    fn return_type(&self, _args: &[ArrowDataType]) -> Result<ArrowDataType> {
        Ok(ArrowDataType::Utf8)
    }
    /// Look each OID up; an aggregate's OID is a refused, named error exactly
    /// as `PostgreSQL` refuses it.
    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        use arrow::array::Int64Array;
        let arrays = ColumnarValue::values_to_arrays(&args.args)?;
        let widened = arrow::compute::cast(&arrays[0], &ArrowDataType::Int64)?;
        let oids = widened
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| DataFusionError::Internal("oid not castable to Int64".into()))?;

        let mut definitions = StringBuilder::with_capacity(oids.len(), 128 * oids.len());
        for row in 0..oids.len() {
            if oids.is_null(row) {
                definitions.append_null();
                continue;
            }
            match self.definitions.get(&oids.value(row)) {
                Some(DefinitionOutcome::Definition(statement)) => {
                    definitions.append_value(statement);
                }
                Some(DefinitionOutcome::AggregateRefusal(name)) => {
                    return Err(DataFusionError::Execution(format!(
                        "\"{name}\" is an aggregate function"
                    )));
                }
                // A known function whose body the snapshot does not carry, or
                // an OID that is no function at all: NULL either way, which is
                // also PostgreSQL's answer for the latter.
                Some(DefinitionOutcome::BodyNotCaptured) | None => definitions.append_null(),
            }
        }
        Ok(ColumnarValue::Array(
            Arc::new(definitions.finish()) as ArrayRef
        ))
    }
}

/// Register `pg_get_functiondef` (and its `pg_catalog.` alias) over the
/// definitions rendered from the catalog `ctx` serves.
///
/// `type_names` maps type OIDs to their `format_type` renderings, built by
/// the caller through the same `format_type_name` the `format_type` UDF uses.
///
/// # Errors
///
/// Returns an error when the `pg_proc` snapshot query cannot be planned or
/// executed, or a column comes back as an unexpected Arrow type.
pub async fn register_scalar_pg_get_functiondef<S: std::hash::BuildHasher>(
    ctx: &SessionContext,
    type_names: HashMap<i64, String, S>,
) -> Result<()> {
    let type_names: HashMap<i64, String> = type_names.into_iter().collect();
    let definitions = Arc::new(load_rendered_definitions(ctx, &type_names).await?);
    let udf = ScalarUDF::new_from_impl(PgGetFunctiondef {
        signature: Signature::one_of(
            vec![
                TypeSignature::Exact(vec![ArrowDataType::Int32]),
                TypeSignature::Exact(vec![ArrowDataType::Int64]),
            ],
            // Stable, not Immutable: the map is fixed, but folding at planning
            // time would bake answers into plans built before registration
            // completes.
            Volatility::Stable,
        ),
        definitions,
    })
    .with_aliases(["pg_catalog.pg_get_functiondef"]);
    ctx.register_udf(udf);
    Ok(())
}
