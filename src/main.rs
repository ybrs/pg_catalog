//! Entry point for the `pg_catalog` compatibility server.
//!
//! Parses CLI arguments, builds a `SessionContext` and starts the pgwire server,
//! providing a simple way to run the `DataFusion`-backed `PostgreSQL` emulator.
use datafusion_pg_catalog::pg_catalog_helpers::ColumnDef;
use datafusion_pg_catalog::{pg_catalog_helpers, register_user_database};
use std::collections::BTreeMap;
use std::env;
use std::sync::Arc;
// use arrow::util::pretty;
use datafusion_pg_catalog::server::start_server;
use datafusion_pg_catalog::session::get_base_session_context;

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

    let (ctx, _log) = get_base_session_context(
        Some(schema_path),
        default_catalog.clone(),
        default_schema.clone(),
    )
    .await?;

    register_user_database(&ctx, "pgtry").await?;
    let public_oid = pg_catalog_helpers::register_schema(&ctx, "pgtry", "public").await?;
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
    pg_catalog_helpers::register_user_tables(&ctx, "pgtry", public_oid, "users", vec![c1, c2])
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

/// Start the async runtime and run the server, logging a crash instead of propagating it.
///
/// A failure is logged rather than returned so the process exits 0 after printing the
/// cause, which keeps the message visible instead of letting the runtime print a bare
/// `Error: ...` debug dump.
///
/// # Errors
///
/// Never returns an error: [`run`]'s failure is logged and swallowed. The `Result`
/// return type is kept so `?` stays available inside `main`.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    if let Err(e) = run().await {
        log::error!("server crashed: {e:?}");
    }
    Ok(())
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
