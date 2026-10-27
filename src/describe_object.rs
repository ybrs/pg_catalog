//! `pg_describe_object(classid, objid, objsubid)`: the human-readable name of
//! a catalog object identified by which catalog it lives in plus its own OID.
//!
//! WHY THIS EXISTS. psql's `\dx+` lists an extension's contents with
//!
//! ```sql
//! SELECT pg_catalog.pg_describe_object(classid, objid, 0) AS "Object description"
//! FROM pg_catalog.pg_depend
//! WHERE refclassid = 'pg_catalog.pg_extension'::pg_catalog.regclass
//!   AND refobjid = '...' AND deptype = 'e'
//! ```
//!
//! and this function did not exist, so `\dx+` failed outright. It is a
//! dispatch over `classid` - one rendering per catalog - not a single lookup.
//!
//! WHAT IS COVERED, measured against the bundled `PostgreSQL` 17.4 rather
//! than assumed (each format below is the live server's own output):
//!
//! ```text
//!   classid  catalog        rendering
//!   1259     pg_class       "table pg_class", "view pg_tables", ... by relkind;
//!                           objsubid > 0 -> "column oid of table pg_class"
//!   1255     pg_proc        "function avg(bigint)" - argument types included
//!   1247     pg_type        "type integer"
//!   2615     pg_namespace   "schema public"
//!   2617     pg_operator    "operator =(integer,integer)"
//!   2612     pg_language    "language plpgsql"
//!   3079     pg_extension   "extension plpgsql"
//! ```
//!
//! These are every class the embedded catalog's `pg_depend` extension rows
//! reference, plus the classes psql's describe queries touch. An OID under a
//! covered class that matches nothing answers NULL, and an UNCOVERED class
//! answers the error `unsupported object class: N` - both exactly as
//! `PostgreSQL` behaves, so a gap surfaces as the same failure a real server
//! would give rather than a silent blank.
//!
//! Names are schema-qualified only when the schema is not on the default
//! search path (`pg_catalog`, `public`), which is how the live server renders
//! them - `table pg_class`, not `table pg_catalog.pg_class`.
//!
//! THE LOOKUPS ARE A SNAPSHOT taken when the function is registered, the same
//! trade `format_type` makes and with the same limitation: an object
//! registered after startup renders as if absent. The catalog this serves is
//! static once built, and `pg_depend` - the only table whose rows feed this
//! function in practice - only ever holds snapshot objects.

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

/// The `pg_class.oid` values of the catalogs this function can describe -
/// `PostgreSQL`'s own fixed identifiers for its system tables.
mod catalog_class {
    pub const PG_CLASS: i64 = 1259;
    pub const PG_PROC: i64 = 1255;
    pub const PG_TYPE: i64 = 1247;
    pub const PG_NAMESPACE: i64 = 2615;
    pub const PG_OPERATOR: i64 = 2617;
    pub const PG_LANGUAGE: i64 = 2612;
    pub const PG_EXTENSION: i64 = 3079;
}

/// One relation as the descriptions need it: its label, name, and columns.
#[derive(Debug)]
struct DescribedRelation {
    /// The relkind-specific label: `table`, `view`, `index`, ...
    kind_label: &'static str,
    /// The relation's name, schema-qualified if off the search path.
    display_name: String,
    /// Column names by 1-based `attnum`, for the `objsubid > 0` form.
    columns_by_attnum: HashMap<i64, String>,
}

/// Everything the dispatch renders from, loaded once at registration.
#[derive(Debug)]
struct ObjectNameSnapshot {
    relations: HashMap<i64, DescribedRelation>,
    /// Function name plus rendered argument list, e.g. `avg(bigint)`.
    functions: HashMap<i64, String>,
    /// `format_type`-style type names, e.g. `integer`.
    type_names: HashMap<i64, String>,
    schemas: HashMap<i64, String>,
    /// Operator name plus argument types, e.g. `=(integer,integer)`.
    operators: HashMap<i64, String>,
    languages: HashMap<i64, String>,
    extensions: HashMap<i64, String>,
}

/// The `pg_class.relkind` label `PostgreSQL` uses in object descriptions.
fn relkind_label(relkind: &str) -> &'static str {
    match relkind {
        "i" | "I" => "index",
        "S" => "sequence",
        "t" => "toast table",
        "v" => "view",
        "m" => "materialized view",
        "c" => "composite type",
        "f" => "foreign table",
        // "r", "p", and anything unforeseen: PostgreSQL says "table" for both
        // ordinary and partitioned tables.
        _ => "table",
    }
}

/// `name`, qualified with `schema` only when the schema is off the default
/// search path - `PostgreSQL` omits the qualifier for visible objects.
fn on_search_path_or_qualified(schema: &str, name: &str) -> String {
    if schema == "pg_catalog" || schema == "public" {
        name.to_string()
    } else {
        format!("{schema}.{name}")
    }
}

/// Collect `sql`'s first two columns into an oid -> text map.
///
/// Every lookup this module loads is that shape once the query itself does the
/// rendering, so one collector serves them all. Numeric and text columns are
/// cast in SQL, keeping the Arrow downcasts here total.
async fn oid_to_text_map(ctx: &SessionContext, sql: &str) -> Result<HashMap<i64, String>> {
    use arrow::array::{Int64Array, StringArray};
    let mut map = HashMap::new();
    for batch in ctx.sql(sql).await?.collect().await? {
        let oids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| DataFusionError::Internal("oid column not Int64".into()))?;
        let texts = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| DataFusionError::Internal("text column not Utf8".into()))?;
        for row in 0..batch.num_rows() {
            if !oids.is_null(row) && !texts.is_null(row) {
                map.insert(oids.value(row), texts.value(row).to_string());
            }
        }
    }
    Ok(map)
}

/// Render a function's argument list the way `PostgreSQL` does in object
/// descriptions: `format_type` names, comma-separated - `avg(bigint)`.
///
/// `proargtypes` arrives as the list column's text rendering is unreliable
/// across Arrow list types, so the types are unnested in SQL and stitched
/// per function here.
async fn function_signatures(
    ctx: &SessionContext,
    type_names: &HashMap<i64, String>,
) -> Result<HashMap<i64, String>> {
    use arrow::array::{Int64Array, ListArray, StringArray};

    let mut signatures = HashMap::new();
    let batches = ctx
        .sql(
            "SELECT CAST(p.oid AS BIGINT), p.proname, n.nspname, p.proargtypes \
             FROM pg_catalog.pg_proc p \
             JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace",
        )
        .await?
        .collect()
        .await?;
    for batch in batches {
        let oids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| DataFusionError::Internal("pg_proc.oid not Int64".into()))?;
        let names = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| DataFusionError::Internal("pg_proc.proname not Utf8".into()))?;
        let schemas = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| DataFusionError::Internal("nspname not Utf8".into()))?;
        let argument_lists = batch
            .column(3)
            .as_any()
            .downcast_ref::<ListArray>()
            .ok_or_else(|| DataFusionError::Internal("proargtypes not a List".into()))?;
        for row in 0..batch.num_rows() {
            if oids.is_null(row) || names.is_null(row) {
                continue;
            }
            let mut rendered_arguments = Vec::new();
            if !argument_lists.is_null(row) {
                let elements = argument_lists.value(row);
                let elements = elements
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .ok_or_else(|| {
                        DataFusionError::Internal("proargtypes elements not Int64".into())
                    })?;
                for element in 0..elements.len() {
                    let type_oid = elements.value(element);
                    rendered_arguments.push(
                        type_names
                            .get(&type_oid)
                            .cloned()
                            .unwrap_or_else(|| type_oid.to_string()),
                    );
                }
            }
            let display = on_search_path_or_qualified(schemas.value(row), names.value(row));
            signatures.insert(
                oids.value(row),
                format!("{display}({})", rendered_arguments.join(",")),
            );
        }
    }
    Ok(signatures)
}

/// Load every relation's label, display name and column names.
async fn described_relations(ctx: &SessionContext) -> Result<HashMap<i64, DescribedRelation>> {
    use arrow::array::{Int64Array, StringArray};

    let mut relations: HashMap<i64, DescribedRelation> = HashMap::new();
    let batches = ctx
        .sql(
            "SELECT CAST(c.oid AS BIGINT), c.relname, c.relkind, n.nspname \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace",
        )
        .await?
        .collect()
        .await?;
    for batch in batches {
        let oids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| DataFusionError::Internal("pg_class.oid not Int64".into()))?;
        let names = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| DataFusionError::Internal("relname not Utf8".into()))?;
        let kinds = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| DataFusionError::Internal("relkind not Utf8".into()))?;
        let schemas = batch
            .column(3)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| DataFusionError::Internal("nspname not Utf8".into()))?;
        for row in 0..batch.num_rows() {
            if oids.is_null(row) || names.is_null(row) {
                continue;
            }
            relations.insert(
                oids.value(row),
                DescribedRelation {
                    kind_label: relkind_label(kinds.value(row)),
                    display_name: on_search_path_or_qualified(schemas.value(row), names.value(row)),
                    columns_by_attnum: HashMap::new(),
                },
            );
        }
    }

    // Column names attach to the relations loaded above; attnum <= 0 are the
    // system pseudo-columns, which object addresses never reference.
    let batches = ctx
        .sql(
            "SELECT CAST(attrelid AS BIGINT), CAST(attnum AS BIGINT), attname \
             FROM pg_catalog.pg_attribute WHERE attnum > 0",
        )
        .await?
        .collect()
        .await?;
    for batch in batches {
        let relation_oids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| DataFusionError::Internal("attrelid not Int64".into()))?;
        let attnums = batch
            .column(1)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| DataFusionError::Internal("attnum not Int64".into()))?;
        let names = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| DataFusionError::Internal("attname not Utf8".into()))?;
        for row in 0..batch.num_rows() {
            if relation_oids.is_null(row) || attnums.is_null(row) || names.is_null(row) {
                continue;
            }
            if let Some(relation) = relations.get_mut(&relation_oids.value(row)) {
                relation
                    .columns_by_attnum
                    .insert(attnums.value(row), names.value(row).to_string());
            }
        }
    }
    Ok(relations)
}

impl ObjectNameSnapshot {
    /// Load every lookup from the catalog `ctx` currently serves.
    async fn load(ctx: &SessionContext, type_names: HashMap<i64, String>) -> Result<Self> {
        let functions = function_signatures(ctx, &type_names).await?;
        let relations = described_relations(ctx).await?;
        let schemas = oid_to_text_map(
            ctx,
            "SELECT CAST(oid AS BIGINT), nspname FROM pg_catalog.pg_namespace",
        )
        .await?;
        // Operators render with both argument types; a missing side (a prefix
        // operator) is NONE, as PostgreSQL prints it.
        let operators = oid_to_text_map(
            ctx,
            "SELECT CAST(o.oid AS BIGINT), o.oprname || '(' || \
                 coalesce(tl.typ, 'NONE') || ',' || coalesce(tr.typ, 'NONE') || ')' \
             FROM pg_catalog.pg_operator o \
             LEFT JOIN (SELECT CAST(oid AS BIGINT) AS oid, format_type(CAST(oid AS BIGINT), NULL) AS typ \
                        FROM pg_catalog.pg_type) tl ON tl.oid = CAST(o.oprleft AS BIGINT) \
             LEFT JOIN (SELECT CAST(oid AS BIGINT) AS oid, format_type(CAST(oid AS BIGINT), NULL) AS typ \
                        FROM pg_catalog.pg_type) tr ON tr.oid = CAST(o.oprright AS BIGINT)",
        )
        .await?;
        let languages = oid_to_text_map(
            ctx,
            "SELECT CAST(oid AS BIGINT), lanname FROM pg_catalog.pg_language",
        )
        .await?;
        let extensions = oid_to_text_map(
            ctx,
            "SELECT CAST(oid AS BIGINT), extname FROM pg_catalog.pg_extension",
        )
        .await?;
        Ok(Self {
            relations,
            functions,
            type_names,
            schemas,
            operators,
            languages,
            extensions,
        })
    }

    /// The description of `(classid, objid, objsubid)`.
    ///
    /// `Ok(None)` is a covered class whose OID matches nothing - NULL, as
    /// `PostgreSQL` answers it. An uncovered class is an error naming it,
    /// also as `PostgreSQL` answers it.
    fn describe(&self, classid: i64, objid: i64, objsubid: i64) -> Result<Option<String>> {
        let described = match classid {
            catalog_class::PG_CLASS => self.relations.get(&objid).and_then(|relation| {
                let base = format!("{} {}", relation.kind_label, relation.display_name);
                if objsubid == 0 {
                    Some(base)
                } else {
                    relation
                        .columns_by_attnum
                        .get(&objsubid)
                        .map(|column| format!("column {column} of {base}"))
                }
            }),
            catalog_class::PG_PROC => self
                .functions
                .get(&objid)
                .map(|signature| format!("function {signature}")),
            catalog_class::PG_TYPE => self
                .type_names
                .get(&objid)
                .map(|name| format!("type {name}")),
            catalog_class::PG_NAMESPACE => self
                .schemas
                .get(&objid)
                .map(|name| format!("schema {name}")),
            catalog_class::PG_OPERATOR => self
                .operators
                .get(&objid)
                .map(|rendered| format!("operator {rendered}")),
            catalog_class::PG_LANGUAGE => self
                .languages
                .get(&objid)
                .map(|name| format!("language {name}")),
            catalog_class::PG_EXTENSION => self
                .extensions
                .get(&objid)
                .map(|name| format!("extension {name}")),
            uncovered => {
                return Err(DataFusionError::Execution(format!(
                    "unsupported object class: {uncovered}"
                )))
            }
        };
        Ok(described)
    }
}

/// The `pg_describe_object` UDF over one loaded snapshot.
#[derive(Debug)]
struct PgDescribeObject {
    /// Three integer arguments of any width - catalog columns arrive as
    /// `Int32`, literals as `Int64` - normalized in `invoke_with_args`.
    signature: Signature,
    snapshot: Arc<ObjectNameSnapshot>,
}

impl PartialEq for PgDescribeObject {
    /// Equality by identity of the loaded snapshot, which is what `DataFusion`'s
    /// UDF registry compares registrations by.
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.snapshot, &other.snapshot)
    }
}
impl Eq for PgDescribeObject {}
impl std::hash::Hash for PgDescribeObject {
    /// Hash by snapshot identity, matching the equality above.
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.snapshot).hash(state);
    }
}

impl ScalarUDFImpl for PgDescribeObject {
    /// The unqualified name; the `pg_catalog.` spelling is an alias.
    fn name(&self) -> &'static str {
        "pg_describe_object"
    }
    /// `(classid, objid, objsubid)`, any integer widths.
    fn signature(&self) -> &Signature {
        &self.signature
    }
    /// Always text (a description), NULL for a missing object.
    fn return_type(&self, _args: &[ArrowDataType]) -> Result<ArrowDataType> {
        Ok(ArrowDataType::Utf8)
    }
    /// Dispatch each row over its classid.
    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        use arrow::array::Int64Array;
        let arrays = ColumnarValue::values_to_arrays(&args.args)?;
        let widened: Vec<ArrayRef> = arrays
            .iter()
            .map(|array| arrow::compute::cast(array, &ArrowDataType::Int64))
            .collect::<std::result::Result<_, _>>()?;
        let classids = widened[0]
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| DataFusionError::Internal("classid not castable to Int64".into()))?;
        let objids = widened[1]
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| DataFusionError::Internal("objid not castable to Int64".into()))?;
        let objsubids = widened[2]
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| DataFusionError::Internal("objsubid not castable to Int64".into()))?;

        let mut descriptions = StringBuilder::with_capacity(classids.len(), 32 * classids.len());
        for row in 0..classids.len() {
            if classids.is_null(row) || objids.is_null(row) {
                descriptions.append_null();
                continue;
            }
            let objsubid = if objsubids.is_null(row) {
                0
            } else {
                objsubids.value(row)
            };
            match self
                .snapshot
                .describe(classids.value(row), objids.value(row), objsubid)?
            {
                Some(description) => descriptions.append_value(description),
                None => descriptions.append_null(),
            }
        }
        Ok(ColumnarValue::Array(
            Arc::new(descriptions.finish()) as ArrayRef
        ))
    }
}

/// Register `pg_describe_object` (and its `pg_catalog.` alias) over a snapshot
/// of the catalog `ctx` serves.
///
/// `type_names` maps `pg_type.oid` to the `format_type` rendering of that
/// type, built by the caller through the same `format_type_name` the
/// `format_type` UDF uses - so `type integer` here and `format_type(23)`
/// in a query can never disagree on a spelling.
///
/// # Errors
///
/// Returns an error when any of the snapshot queries cannot be planned or
/// executed, or a column comes back as an unexpected Arrow type.
pub async fn register_scalar_pg_describe_object<S: std::hash::BuildHasher>(
    ctx: &SessionContext,
    type_names: HashMap<i64, String, S>,
) -> Result<()> {
    // Rebuilt on the default hasher: the snapshot stores it long-term, and the
    // generic parameter exists only so callers may pass any hasher in.
    let type_names: HashMap<i64, String> = type_names.into_iter().collect();
    let snapshot = Arc::new(ObjectNameSnapshot::load(ctx, type_names).await?);
    let udf = ScalarUDF::new_from_impl(PgDescribeObject {
        signature: Signature::one_of(
            vec![TypeSignature::Any(3)],
            // Stable, not Immutable: the snapshot is fixed, but folding at
            // planning time would bake answers into view plans built before
            // registration completes.
            Volatility::Stable,
        ),
        snapshot,
    })
    .with_aliases(["pg_catalog.pg_describe_object"]);
    ctx.register_udf(udf);
    Ok(())
}
