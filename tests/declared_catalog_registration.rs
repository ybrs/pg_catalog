//! Declared registrations served end to end through the one catalog mechanism.
//!
//! The INSERT-based eager registration path is gone; a host that declares its
//! objects up front now has them wrapped in a `DeclaredCatalogSource` and
//! served through the same lazy merge path a live source uses. The unit tests
//! beside the source pin what it EMITS; this file pins what a client then SEES,
//! which is the contract the deleted eager path's own tests used to hold:
//! declared tables appear in `pg_class`, `pg_attribute`, `pg_tables` and both
//! `information_schema` views with the right types, a declared `public` is the
//! canonical one, and a host that declares something twice keeps working.

use std::collections::BTreeMap;
use std::sync::Arc;

use datafusion::error::Result as DFResult;
use datafusion::prelude::SessionContext;
use datafusion_pg_catalog::catalog_oid_registry::InMemoryCatalogOidRegistry;
use datafusion_pg_catalog::declared_catalog_source::{DeclaredCatalogSource, PUBLIC_SCHEMA_OID};
use datafusion_pg_catalog::lazy_catalog::LazyCatalogOptions;
use datafusion_pg_catalog::pg_catalog_helpers::ColumnDef;
use datafusion_pg_catalog::session::{execute_sql, get_base_session_context_with_lazy_catalog};

/// One declared column in the wire format hosts use.
fn column(name: &str, col_type: &str, nullable: bool) -> BTreeMap<String, ColumnDef> {
    let mut map = BTreeMap::new();
    map.insert(
        name.to_string(),
        ColumnDef {
            col_type: col_type.to_string(),
            nullable,
            has_default: false,
        },
    );
    map
}

/// A context serving `appdb`, declared the way a riffq host declares it:
/// one database, the `public` schema, and a two-column `users` table.
async fn context_over_declared_appdb() -> DFResult<SessionContext> {
    let declared = DeclaredCatalogSource::from_declarations(
        &["appdb".to_string()],
        &[("appdb".to_string(), "public".to_string())],
        &[(
            "appdb".to_string(),
            "public".to_string(),
            "users".to_string(),
            vec![column("id", "int", false), column("name", "text", true)],
        )],
        &InMemoryCatalogOidRegistry::new(),
    )
    .expect("a well-formed declaration set");
    let (ctx, _log) = get_base_session_context_with_lazy_catalog(
        None,
        "appdb".to_string(),
        "public".to_string(),
        Arc::new(declared),
        LazyCatalogOptions::all(),
        "appdb".to_string(),
    )
    .await?;
    Ok(ctx)
}

/// Every value of a one-column text query, in row order.
async fn text_column(ctx: &SessionContext, sql: &str) -> DFResult<Vec<String>> {
    let (batches, _schema) = execute_sql(ctx, sql, None, None).await?;
    let mut out = Vec::new();
    for batch in &batches {
        let column = arrow::compute::cast(batch.column(0), &arrow::datatypes::DataType::Utf8)?;
        let array = arrow::array::as_string_array(&column);
        for row in 0..batch.num_rows() {
            out.push(array.value(row).to_string());
        }
    }
    Ok(out)
}

/// A declared table is visible everywhere a client looks for it.
///
/// `pg_tables` and the two `information_schema` views are all SQL views over
/// the base rows, so this asserts the whole derivation chain, not one table.
#[tokio::test(flavor = "multi_thread")]
async fn a_declared_table_is_served_from_every_catalog_surface() -> DFResult<()> {
    let ctx = context_over_declared_appdb().await?;

    assert_eq!(
        text_column(
            &ctx,
            "SELECT relname FROM pg_catalog.pg_class WHERE relname = 'users'"
        )
        .await?,
        vec!["users"]
    );
    assert_eq!(
        text_column(
            &ctx,
            "SELECT tablename FROM pg_catalog.pg_tables WHERE schemaname = 'public' \
             AND tablename = 'users'"
        )
        .await?,
        vec!["users"]
    );
    assert_eq!(
        text_column(
            &ctx,
            "SELECT table_type FROM information_schema.tables WHERE table_name = 'users'"
        )
        .await?,
        vec!["BASE TABLE"]
    );
    Ok(())
}

/// The declared columns come back with their order, types and nullability.
///
/// The type strings were resolved through `map_type_to_oid` on the way in, so
/// this is the round trip a client's introspection actually performs.
#[tokio::test(flavor = "multi_thread")]
async fn declared_columns_keep_their_order_types_and_nullability() -> DFResult<()> {
    let ctx = context_over_declared_appdb().await?;

    let described = text_column(
        &ctx,
        "SELECT column_name || ':' || data_type || ':' || is_nullable \
         FROM information_schema.columns WHERE table_name = 'users' \
         ORDER BY ordinal_position",
    )
    .await?;
    assert_eq!(described, vec!["id:integer:NO", "name:text:YES"]);

    let attributes = text_column(
        &ctx,
        "SELECT a.attname || ':' || a.attnum FROM pg_catalog.pg_attribute a \
         JOIN pg_catalog.pg_class c ON a.attrelid = c.oid \
         WHERE c.relname = 'users' ORDER BY a.attnum",
    )
    .await?;
    assert_eq!(attributes, vec!["id:1", "name:2"]);
    Ok(())
}

/// The declared `public` is the canonical one, end to end.
///
/// OID 2200, exactly one row, and present in `information_schema.schemata` -
/// which inner-joins the owner to `pg_authid`, so this also proves the owner
/// the declared row carries is a role that exists. A generated OID or a
/// made-up owner each made a schema silently unusable in a different way.
#[tokio::test(flavor = "multi_thread")]
async fn the_declared_public_schema_is_the_canonical_one() -> DFResult<()> {
    let ctx = context_over_declared_appdb().await?;

    assert_eq!(
        text_column(
            &ctx,
            "SELECT oid FROM pg_catalog.pg_namespace WHERE nspname = 'public'"
        )
        .await?,
        vec![PUBLIC_SCHEMA_OID.to_string()],
        "exactly one public row, at the canonical oid"
    );
    assert_eq!(
        text_column(
            &ctx,
            "SELECT schema_name FROM information_schema.schemata WHERE schema_name = 'public'"
        )
        .await?,
        vec!["public"],
        "the owner join must hold, or the schema vanishes from schemata"
    );
    // And the canonical oid actually owns the table - the failure mode that
    // motivated pinning 2200 was a public that existed but owned nothing.
    assert_eq!(
        text_column(
            &ctx,
            &format!(
                "SELECT relname FROM pg_catalog.pg_class WHERE relnamespace = {PUBLIC_SCHEMA_OID} \
                 AND relname = 'users'"
            )
        )
        .await?,
        vec!["users"]
    );
    Ok(())
}

/// A duplicate declaration is refused with an "already exists" error.
///
/// The eager path silently skipped duplicates; that tolerance is retired.
/// `PostgreSQL` refuses to CREATE an existing name, and so does the declared
/// source - at declaration time, before any context exists, so the mistake
/// is reported once and loudly rather than shaping the catalog quietly.
#[tokio::test(flavor = "multi_thread")]
async fn duplicate_declarations_are_refused() -> DFResult<()> {
    let error = DeclaredCatalogSource::from_declarations(
        &["appdb".to_string(), "appdb".to_string()],
        &[],
        &[],
        &InMemoryCatalogOidRegistry::new(),
    )
    .expect_err("the duplicate database must be refused");
    assert!(
        error
            .to_string()
            .contains("database \"appdb\" already exists"),
        "{error}"
    );
    Ok(())
}

/// The declared database's metadata is what the retired eager path wrote.
///
/// The ACL is load-bearing: riffq's wire tests decode it as text[], so it is
/// asserted through SQL exactly as a client reads it.
#[tokio::test(flavor = "multi_thread")]
async fn a_declared_database_reports_the_contractual_metadata() -> DFResult<()> {
    let ctx = context_over_declared_appdb().await?;
    assert_eq!(
        text_column(
            &ctx,
            "SELECT pg_catalog.array_to_string(datacl, ',') FROM pg_catalog.pg_database \
             WHERE datname = 'appdb'"
        )
        .await?,
        vec!["=Tc/dbuser,dbuser=CTc/dbuser"]
    );
    assert_eq!(
        text_column(
            &ctx,
            "SELECT encoding || ':' || datcollate || ':' || datallowconn \
             FROM pg_catalog.pg_database WHERE datname = 'appdb'"
        )
        .await?,
        vec!["6:C:true"]
    );
    Ok(())
}
