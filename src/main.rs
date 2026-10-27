//! Entry point for the `pg_catalog` compatibility server.
//!
//! Parses CLI arguments, builds a `SessionContext` and starts the pgwire server,
//! providing a simple way to run the `DataFusion`-backed `PostgreSQL` emulator.
use datafusion_pg_catalog::catalog_oid_registry::{
    CatalogOidRegistry, InMemoryCatalogOidRegistry, RedbCatalogOidRegistry,
};
use datafusion_pg_catalog::declared_catalog_source::DeclaredCatalogSource;
use datafusion_pg_catalog::lazy_catalog::LazyCatalogOptions;
use datafusion_pg_catalog::pg_catalog_helpers::ColumnDef;
use datafusion_pg_catalog::session::get_base_session_context_with_lazy_catalog;
use std::collections::BTreeMap;
use std::env;
use std::sync::Arc;
// use arrow::util::pretty;
use datafusion_pg_catalog::server::start_server;

/// Build the demo server from the command line and serve it until the process ends.
///
/// Reads the schema directory from the first argument and the optional
/// `--default-catalog` / `--default-schema` / `--host` / `--port` / `--capture`
/// flags, seeds a small `pgtry` database so a client has something to query, and
/// hands the context to [`start_server`].
///
/// # Errors
///
/// Returns an error if the schema directory cannot be loaded into a session context,
/// if seeding the demo database, schema or tables fails, or if the server cannot bind
/// and serve the requested address.
async fn run() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        log::info!(
            "Usage: {} schema_directory --default-catalog public --default-schema postgres",
            args[0]
        );
        std::process::exit(1);
    }

    let schema_path = &args[1];

    let default_catalog = args
        .iter()
        .position(|x| x == "--default-catalog")
        .and_then(|i| args.get(i + 1))
        .unwrap_or(&"datafusion".to_string())
        .clone();

    let default_schema = args
        .iter()
        .position(|x| x == "--default-schema")
        .and_then(|i| args.get(i + 1))
        .unwrap_or(&"public".to_string())
        .clone();

    let host = args
        .iter()
        .position(|x| x == "--host")
        .and_then(|i| args.get(i + 1))
        .unwrap_or(&"127.0.0.1".to_string())
        .clone();

    let port = args
        .iter()
        .position(|x| x == "--port")
        .and_then(|i| args.get(i + 1))
        .unwrap_or(&"5433".to_string())
        .clone();

    let address = format!("{host}:{port}");

    let capture_file = args
        .iter()
        .position(|x| x == "--capture")
        .and_then(|i| args.get(i + 1))
        .cloned();

    // Where declared objects' oids are recorded so they survive restarts.
    // Without it the demo falls back to an in-memory registry, which is
    // deprecated: oids then restart from scratch with the process.
    let oid_registry_path = args
        .iter()
        .position(|x| x == "--oid-registry")
        .and_then(|i| args.get(i + 1))
        .cloned();

    let declared = demo_catalog(oid_registry_path)?;

    let (ctx, _log) = get_base_session_context_with_lazy_catalog(
        Some(schema_path),
        default_catalog.clone(),
        default_schema.clone(),
        Arc::new(declared),
        LazyCatalogOptions::all(),
        "pgtry".to_string(),
    )
    .await?;

    start_server(
        Arc::new(ctx),
        &address,
        &default_catalog,
        &default_schema,
        capture_file.map(std::convert::Into::into),
    )
    .await?;

    Ok(())
}

/// The demo catalog: one `pgtry` database holding one `public.users` table,
/// declared the same way a riffq host declares its objects and served through
/// the same lazy merge path. This binary is the retired eager API's old second
/// consumer; both now drive one mechanism.
///
/// `oid_registry_path` selects where the declared objects' oids are recorded;
/// `None` falls back to the deprecated in-memory registry, with a warning.
///
/// # Errors
///
/// Returns an error when the registry file cannot be opened or the declaration
/// set is rejected.
fn demo_catalog(oid_registry_path: Option<String>) -> anyhow::Result<DeclaredCatalogSource> {
    let mut c1 = BTreeMap::new();
    c1.insert(
        "id".to_string(),
        ColumnDef {
            col_type: "int".to_string(),
            nullable: true,
            has_default: false,
        },
    );
    let mut c2 = BTreeMap::new();
    c2.insert(
        "name".to_string(),
        ColumnDef {
            col_type: "text".to_string(),
            nullable: true,
            has_default: false,
        },
    );
    let registry: Box<dyn CatalogOidRegistry> = if let Some(path) = oid_registry_path {
        Box::new(RedbCatalogOidRegistry::open(std::path::Path::new(&path))?)
    } else {
        log::warn!(
            "no --oid-registry given: using the in-memory oid registry, so oids are \
             not stable across restarts. This fallback is deprecated; pass \
             --oid-registry <path>."
        );
        Box::new(InMemoryCatalogOidRegistry::new())
    };
    Ok(DeclaredCatalogSource::from_declarations(
        &["pgtry".to_string()],
        &[("pgtry".to_string(), "public".to_string())],
        &[(
            "pgtry".to_string(),
            "public".to_string(),
            "users".to_string(),
            vec![c1, c2],
        )],
        registry.as_ref(),
    )?)
}

/// Start the async runtime and run the server, reporting a crash as a non-zero exit.
///
/// The failure is logged for a readable message AND the process exits non-zero,
/// because those are two different audiences. Logging alone is what this did
/// before, and it meant a supervisor - systemd, a container runtime, a shell
/// `&&` - saw a clean shutdown from a server that had crashed, and would not
/// restart it.
///
/// [`std::process::exit`] rather than returning the error, so the operator still
/// gets the logged message rather than the runtime's bare `Error: ...` debug
/// dump on top of it.
#[tokio::main]
async fn main() {
    env_logger::init();

    if let Err(e) = run().await {
        log::error!("server crashed: {e:?}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use arrow::datatypes::Schema;
    use datafusion::execution::context::SessionContext;
    use datafusion_pg_catalog::router::dispatch_query;
    use std::sync::Arc;

    /// The binary can reach `dispatch_query` through the library crate: a non-catalog
    /// query is routed to the supplied handler.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_dispatch_in_main() -> anyhow::Result<()> {
        let ctx = SessionContext::new();
        dispatch_query(&ctx, "SELECT 1", None, None, |_c, _q, _p, _t| async {
            Ok((Vec::new(), Arc::new(Schema::empty())))
        })
        .await?;
        Ok(())
    }
}
