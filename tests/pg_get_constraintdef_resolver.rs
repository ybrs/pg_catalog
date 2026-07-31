//! `pg_get_constraintdef`, and the resolver slot that gives it something to say.
//!
//! The function was advertised in `pg_proc` and implemented nowhere, so any
//! client calling it got "function `pg_catalog.pg_get_constraintdef()` does not
//! exist" rather than a constraint. Real clients do call it: pgAdmin's
//! constraint listing writes `substring(pg_get_constraintdef(c.oid), 7)`,
//! psql's `\dD` renders a domain's CHECK with it, and psql 18 calls it from
//! `\d <table>` where 17.4 does not.
//!
//! It is a resolver rather than a deparser, for the same reason
//! `pg_get_viewdef` and `pg_get_indexdef` are: rendering `CHECK ((price > 0))`
//! or `FOREIGN KEY (customer_id) REFERENCES customers(id)` means deparsing a
//! node tree this catalog does not hold. Guessing at that text would be worse
//! than admitting it is unknown, so with no resolver installed the function
//! answers NULL - which every caller above carries through to an empty column
//! instead of failing the query.
//!
//! The resolver slot is process-wide, so the test that installs one walks the
//! whole NULL -> text -> NULL story in a single test rather than splitting it.
//! Split across tests sharing this binary they would race each other.

use std::sync::{Arc, Mutex};

use datafusion::error::Result as DFResult;
use datafusion::prelude::SessionContext;
use datafusion_pg_catalog::session::execute_sql;
use datafusion_pg_catalog::{
    clear_constraint_definition_resolver, set_constraint_definition_resolver,
    ConstraintDefinitionResolver, ConstraintIdentity,
};

mod common;
use common::base_ctx;

/// One constraint from the catalog: its oid and the identity fields a resolver
/// is handed.
struct CatalogConstraint {
    /// `pg_constraint.oid`.
    oid: i64,
    /// `pg_constraint.conname`.
    name: String,
    /// The constrained relation's name.
    table: String,
    /// The constrained relation's schema.
    schema: String,
}

/// Read one constraint out of the live catalog.
///
/// Taken from the catalog rather than written down here so the test does not
/// depend on which constraints the seed snapshot happens to carry.
async fn any_catalog_constraint(ctx: &SessionContext) -> DFResult<CatalogConstraint> {
    let (batches, _schema) = execute_sql(
        ctx,
        "SELECT k.oid, k.conname, c.relname, n.nspname \
         FROM pg_catalog.pg_constraint k \
         JOIN pg_catalog.pg_class c ON c.oid = k.conrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         ORDER BY k.oid LIMIT 1",
        None,
        None,
    )
    .await?;
    let batch = batches
        .iter()
        .find(|batch| batch.num_rows() > 0)
        .expect("the seed catalog carries constraints");
    let oids = arrow::compute::cast(batch.column(0), &arrow::datatypes::DataType::Int64)?;
    let oids = arrow::array::as_primitive_array::<arrow::datatypes::Int64Type>(&oids);
    Ok(CatalogConstraint {
        oid: oids.value(0),
        name: arrow::array::as_string_array(batch.column(1))
            .value(0)
            .to_string(),
        table: arrow::array::as_string_array(batch.column(2))
            .value(0)
            .to_string(),
        schema: arrow::array::as_string_array(batch.column(3))
            .value(0)
            .to_string(),
    })
}

/// The one text value `sql` projects, or None when it is NULL.
async fn single_text(ctx: &SessionContext, sql: &str) -> DFResult<Option<String>> {
    use arrow::array::Array;

    let (batches, _schema) = execute_sql(ctx, sql, None, None).await?;
    for batch in &batches {
        if batch.num_rows() > 0 {
            let column = arrow::compute::cast(batch.column(0), &arrow::datatypes::DataType::Utf8)?;
            if column.is_null(0) {
                return Ok(None);
            }
            return Ok(Some(
                arrow::array::as_string_array(&column).value(0).to_string(),
            ));
        }
    }
    Ok(None)
}

/// Every call shape real clients use resolves.
///
/// pgAdmin writes one argument, psql 18 writes two, and either may be written
/// bare or schema-qualified. Asserting only that they run keeps this test out
/// of the way of the resolver test below, which owns the process-wide slot.
#[tokio::test(flavor = "multi_thread")]
async fn every_call_shape_clients_use_resolves() -> DFResult<()> {
    let ctx = base_ctx().await?;
    for sql in [
        "SELECT pg_catalog.pg_get_constraintdef(1)",
        "SELECT pg_catalog.pg_get_constraintdef(1, true)",
        "SELECT pg_get_constraintdef(1)",
        "SELECT pg_get_constraintdef(1, true)",
        // pgAdmin's shape, over the catalog rather than a literal.
        "SELECT substring(pg_catalog.pg_get_constraintdef(k.oid), 7) \
         FROM pg_catalog.pg_constraint k LIMIT 5",
    ] {
        execute_sql(&ctx, sql, None, None)
            .await
            .unwrap_or_else(|error| panic!("{sql} should resolve: {error}"));
    }
    Ok(())
}

/// A NULL oid answers NULL rather than reaching the catalog.
#[tokio::test(flavor = "multi_thread")]
async fn a_null_oid_answers_null() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let answer = single_text(&ctx, "SELECT pg_catalog.pg_get_constraintdef(NULL::bigint)").await?;
    assert_eq!(answer, None);
    Ok(())
}

/// The whole story: NULL with no resolver, the resolver's text once installed,
/// NULL again once cleared - and the identity the resolver is handed.
///
/// One test rather than three because the slot is process-wide: split up, they
/// would race each other inside this binary.
#[tokio::test(flavor = "multi_thread")]
async fn a_resolver_supplies_the_text_and_is_handed_the_constraint_identity() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let constraint = any_catalog_constraint(&ctx).await?;
    let query = format!("SELECT pg_catalog.pg_get_constraintdef({})", constraint.oid);

    assert_eq!(
        single_text(&ctx, &query).await?,
        None,
        "with no resolver installed the answer is NULL, not an error"
    );

    // What the resolver was asked about, so the identity can be checked rather
    // than assumed from the text coming back.
    let seen: Arc<Mutex<Vec<ConstraintIdentity>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let resolver: ConstraintDefinitionResolver = Arc::new(move |identity: &ConstraintIdentity| {
        recorded
            .lock()
            .expect("resolver lock")
            .push(identity.clone());
        Some(format!("CHECK (({}))", identity.name))
    });
    set_constraint_definition_resolver(resolver);

    let outcome = async {
        assert_eq!(
            single_text(&ctx, &query).await?,
            Some(format!("CHECK (({}))", constraint.name)),
            "the installed resolver's text is what the function returns"
        );

        // Copied out so the lock is not held across the await below.
        let identity = seen
            .lock()
            .expect("resolver lock")
            .first()
            .cloned()
            .expect("the resolver was called");
        assert_eq!(identity.oid, constraint.oid);
        assert_eq!(identity.name, constraint.name);
        assert_eq!(
            identity.table, constraint.table,
            "a constraint is named relative to the relation it constrains, so the \
             resolver has to be told which one"
        );
        assert_eq!(identity.schema, constraint.schema);
        assert!(
            !identity.constraint_type.is_empty(),
            "contype tells a resolver which kind of constraint to render"
        );

        // An oid naming no constraint stays NULL even with a resolver installed.
        assert_eq!(
            single_text(&ctx, "SELECT pg_catalog.pg_get_constraintdef(999999999)").await?,
            None,
            "an oid that names no constraint never reaches the resolver"
        );
        Ok::<(), datafusion::error::DataFusionError>(())
    }
    .await;

    clear_constraint_definition_resolver();
    outcome?;

    assert_eq!(
        single_text(&ctx, &query).await?,
        None,
        "clearing the resolver puts the answer back to NULL"
    );
    Ok(())
}
