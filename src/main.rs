// Entry point for the pg_catalog compatibility server.
// Parses CLI arguments, builds a SessionContext and starts the pgwire server.
// Provides a simple way to run the DataFusion-backed PostgreSQL emulator.
use datafusion_pg_catalog::{pg_catalog_helpers, register_user_database};
use std::collections::BTreeMap;
use std::env;
use std::sync::Arc;
// use arrow::util::pretty;
use datafusion_pg_catalog::server::start_server;
use datafusion_pg_catalog::session::get_base_session_context;

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

    let address = format!("{}:{}", host, port);

    let capture_file = args
        .iter()
        .position(|x| x == "--capture")
        .and_then(|i| args.get(i + 1))
        .cloned();

    let (ctx, _log) = get_base_session_context(
        Some(schema_path),
        default_catalog.clone(),
        default_schema.clone(),
        None,
    )
    .await?;

    register_user_database(&ctx, "pgtry").await?;
    pg_catalog_helpers::register_schema(&ctx, "pgtry", "public").await?;
    use pg_catalog_helpers::ColumnDef;
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
    pg_catalog_helpers::register_user_tables(&ctx, "pgtry", "public", "users", vec![c1, c2])
        .await?;

    start_server(
        Arc::new(ctx),
        &address,
        &default_catalog,
        &default_schema,
        capture_file.map(|p| p.into()),
    )
    .await?;

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    if let Err(e) = run().await {
        log::error!("server crashed: {:?}", e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use arrow::datatypes::Schema;
    use datafusion::execution::context::SessionContext;
    use datafusion_pg_catalog::router::dispatch_query;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_dispatch_in_main() -> anyhow::Result<()> {
        let ctx = SessionContext::new();
        dispatch_query(&ctx, "SELECT 1", None, None, |_c, _q, _p, _t| async {
            Ok((Vec::new(), Arc::new(Schema::empty())))
        })
        .await?;
        Ok(())
    }
}
