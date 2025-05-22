// Entry point for the pg_catalog compatibility server.
// Parses CLI arguments, builds a SessionContext and starts the pgwire server.
// Provides a simple way to run the DataFusion-backed PostgreSQL emulator.

mod session;
mod replace;
mod clean_duplicate_columns;
mod server;
mod user_functions;
mod db_table;
mod logical_plan_rules;
mod scalar_to_cte;

use std::env;
use std::sync::Arc;
use arrow::util::pretty;
use datafusion::prelude::SessionContext;
use crate::server::start_server;
use crate::session::{get_base_session_context, execute_sql};

async fn run() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        println!("Usage: {} schema_directory --default-catalog public --default-schema postgres", args[0]);
        std::process::exit(1);
    }

    let schema_path = &args[1];

    let default_catalog = args.iter()
        .position(|x| x == "--default-catalog")
        .and_then(|i| args.get(i + 1))
        .unwrap_or(&"datafusion".to_string())
        .clone();

    let default_schema = args.iter()
        .position(|x| x == "--default-schema")
        .and_then(|i| args.get(i + 1))
        .unwrap_or(&"public".to_string())
        .clone();

    let host = args.iter()
        .position(|x| x == "--host")
        .and_then(|i| args.get(i + 1))
        .unwrap_or(&"127.0.0.1".to_string())
        .clone();

    let port = args.iter()
        .position(|x| x == "--port")
        .and_then(|i| args.get(i + 1))
        .unwrap_or(&"5433".to_string())
        .clone();

    let address = format!("{}:{}", host, port);


    let (ctx, log) = get_base_session_context(schema_path, default_catalog.clone(), default_schema.clone()).await?;
    // let results = execute_sql(&ctx, sql.as_str()).await?;
    // pretty::print_batches(&results)?;
    // print_execution_log(log.clone());
    
    start_server(Arc::new(ctx), &address, &default_catalog, &default_schema).await?;

    Ok(())
}


#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Err(e) = run().await {
        eprintln!("server crashed: {:?}", e);
    }
    Ok(())
}
