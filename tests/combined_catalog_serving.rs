//! A declared base catalog and a live source served together, end to end.
//!
//! The arrangement `CombinedCatalogSource` exists for: a host declares its
//! fixed base databases up front (`auth` here, wrapped in a
//! `DeclaredCatalogSource`) and supplies a live source for what appears at
//! runtime (`adhoc` here - user-created databases, mounts). The unit tests
//! beside the source pin what the union EMITS; this file pins what a client
//! SEES through full contexts - one per database, exactly as riffq builds
//! them, both over the same combined source.

use std::collections::BTreeMap;
use std::sync::Arc;

use datafusion::error::Result as DFResult;
use datafusion::prelude::SessionContext;
use datafusion_pg_catalog::catalog_oid_registry::InMemoryCatalogOidRegistry;
use datafusion_pg_catalog::combined_catalog_source::CombinedCatalogSource;
use datafusion_pg_catalog::declared_catalog_source::{DeclaredCatalogSource, PUBLIC_SCHEMA_OID};
use datafusion_pg_catalog::lazy_catalog::{
    ColumnSpec, DatabaseDef, LazyCatalogOptions, LazyCatalogSource, RelationDef, SchemaDef,
};
use datafusion_pg_catalog::pg_catalog_helpers::ColumnDef;
use datafusion_pg_catalog::session::{execute_sql, get_base_session_context_with_lazy_catalog};

/// The live half: one runtime-mounted database, plus one table it contributes
/// INTO the declared base database's public schema.
struct AdhocMountSource;

impl LazyCatalogSource for AdhocMountSource {
    fn databases(&self, callback: &mut dyn FnMut(Vec<DatabaseDef>)) -> DFResult<()> {
        callback(vec![DatabaseDef::new(70001, "adhoc", 10)]);
        Ok(())
    }
    fn schemas(&self, database: &str, callback: &mut dyn FnMut(Vec<SchemaDef>)) -> DFResult<()> {
        match database {
            "adhoc" => callback(vec![SchemaDef::new(70100, "main")]),
            // Also claims a public under the BASE database; the declared
            // public must win the dedup and adopt this source's table.
            "auth" => callback(vec![SchemaDef::new(70101, "public")]),
            _ => {}
        }
        Ok(())
    }
    fn relations(
        &self,
        database: &str,
        schema: &str,
        callback: &mut dyn FnMut(Vec<RelationDef>),
    ) -> DFResult<()> {
        match (database, schema) {
            ("adhoc", "main") => callback(vec![RelationDef::table(70200, 70201, "events")]),
            ("auth", "public") => callback(vec![RelationDef::table(70300, 70301, "sessions")]),
            _ => {}
        }
        Ok(())
    }
    fn columns(
        &self,
        database: &str,
        _schema: &str,
        relation: &str,
        callback: &mut dyn FnMut(Vec<ColumnSpec>),
    ) -> DFResult<()> {
        match (database, relation) {
            ("adhoc", "events") => callback(vec![ColumnSpec::new("ts", 25, true)]),
            ("auth", "sessions") => callback(vec![ColumnSpec::new("token", 25, false)]),
            _ => {}
        }
        Ok(())
    }
}

/// One declared column in the wire format hosts use.
fn column(name: &str, col_type: &str) -> BTreeMap<String, ColumnDef> {
    let mut map = BTreeMap::new();
    map.insert(
        name.to_string(),
        ColumnDef {
            col_type: col_type.to_string(),
            nullable: false,
            has_default: false,
        },
    );
    map
}

/// The combined source: declared `auth` base first (it wins collisions), the
/// live mounts behind it.
fn declared_base_with_live_mounts() -> Arc<CombinedCatalogSource> {
    let declared = DeclaredCatalogSource::from_declarations(
        &["auth".to_string()],
        &[("auth".to_string(), "public".to_string())],
        &[(
            "auth".to_string(),
            "public".to_string(),
            "users".to_string(),
            vec![column("id", "int")],
        )],
        &InMemoryCatalogOidRegistry::new(),
    )
    .expect("a well-formed declaration set");
    Arc::new(CombinedCatalogSource::new(vec![
        Arc::new(declared),
        Arc::new(AdhocMountSource),
    ]))
}

/// A context serving one database over the combined source, the way riffq
/// builds one per connected database.
async fn context_serving(database: &str) -> DFResult<SessionContext> {
    let (ctx, _log) = get_base_session_context_with_lazy_catalog(
        None,
        database.to_string(),
        "public".to_string(),
        declared_base_with_live_mounts(),
        LazyCatalogOptions::all(),
        database.to_string(),
    )
    .await?;
    Ok(ctx)
}

/// Every value of a one-column text query, in row order.
async fn text_column(ctx: &SessionContext, sql: &str) -> DFResult<Vec<String>> {
    let (batches, _schema) = execute_sql(ctx, sql, None, None).await?;
    let mut out = Vec::new();
    for batch in &batches {
        let cast = arrow::compute::cast(batch.column(0), &arrow::datatypes::DataType::Utf8)?;
        let array = arrow::array::as_string_array(&cast);
        for row in 0..batch.num_rows() {
            out.push(array.value(row).to_string());
        }
    }
    Ok(out)
}

/// Both databases are listed from either context - what `\l` reads.
#[tokio::test(flavor = "multi_thread")]
async fn both_the_declared_and_the_live_database_are_listed() -> DFResult<()> {
    for database in ["auth", "adhoc"] {
        let ctx = context_serving(database).await?;
        assert_eq!(
            text_column(
                &ctx,
                "SELECT datname FROM pg_catalog.pg_database \
                 WHERE datname IN ('auth', 'adhoc') ORDER BY datname"
            )
            .await?,
            vec!["adhoc", "auth"],
            "from the {database} context"
        );
    }
    Ok(())
}

/// The live database serves its own tables; the base database's context does
/// not see them, nor the other way round - per-database isolation holds
/// through the union.
#[tokio::test(flavor = "multi_thread")]
async fn each_database_serves_its_own_objects() -> DFResult<()> {
    let adhoc = context_serving("adhoc").await?;
    assert_eq!(
        text_column(
            &adhoc,
            "SELECT tablename FROM pg_catalog.pg_tables WHERE schemaname = 'main'"
        )
        .await?,
        vec!["events"]
    );
    assert_eq!(
        text_column(
            &adhoc,
            "SELECT tablename FROM pg_catalog.pg_tables WHERE tablename = 'users'"
        )
        .await?,
        Vec::<String>::new(),
        "the base database's tables belong to its own context"
    );
    Ok(())
}

/// A database both sources contribute to merges, under the canonical public.
///
/// The declared base carries `users`; the live source mounts `sessions` into
/// the same `auth.public`. One schema row - the declared 2200 - owns both,
/// which is the whole point of combining rather than choosing.
#[tokio::test(flavor = "multi_thread")]
async fn live_tables_merge_into_the_declared_databases_public() -> DFResult<()> {
    let ctx = context_serving("auth").await?;

    assert_eq!(
        text_column(
            &ctx,
            "SELECT oid FROM pg_catalog.pg_namespace WHERE nspname = 'public'"
        )
        .await?,
        vec![PUBLIC_SCHEMA_OID.to_string()],
        "one public row, the canonical one"
    );
    assert_eq!(
        text_column(
            &ctx,
            &format!(
                "SELECT relname FROM pg_catalog.pg_class \
                 WHERE relnamespace = {PUBLIC_SCHEMA_OID} \
                 AND relname IN ('users', 'sessions') ORDER BY relname"
            )
        )
        .await?,
        vec!["sessions", "users"],
        "both sources' tables under that one schema"
    );

    // And each relation carries its own source's columns.
    assert_eq!(
        text_column(
            &ctx,
            "SELECT a.attname FROM pg_catalog.pg_attribute a \
             JOIN pg_catalog.pg_class c ON a.attrelid = c.oid \
             WHERE c.relname = 'sessions' AND a.attnum > 0"
        )
        .await?,
        vec!["token"]
    );
    assert_eq!(
        text_column(
            &ctx,
            "SELECT a.attname FROM pg_catalog.pg_attribute a \
             JOIN pg_catalog.pg_class c ON a.attrelid = c.oid \
             WHERE c.relname = 'users' AND a.attnum > 0"
        )
        .await?,
        vec!["id"]
    );
    Ok(())
}
