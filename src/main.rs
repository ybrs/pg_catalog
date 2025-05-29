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
mod replace_any_group_by;

use clap::{Parser, Subcommand};
use std::sync::Arc;
// use arrow::util::pretty;
use crate::server::start_server;
use crate::session::{get_base_session_context, get_base_session_context_from_binary, parse_schema};

#[derive(Parser)]
#[command(name = "pg_catalog_rs")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Serve {
        schema_dir: String,
        #[arg(long)]
        default_catalog: Option<String>,
        #[arg(long)]
        default_schema: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<String>,
        #[arg(long)]
        capture: Option<String>,
        #[arg(long)]
        binary_file: Option<String>,
    },
    Compile {
        yaml_dir: String,
        binary_file: String,
    },
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Serve { schema_dir, default_catalog, default_schema, host, port, capture, binary_file } => {
            let default_catalog = default_catalog.unwrap_or_else(|| "datafusion".to_string());
            let default_schema = default_schema.unwrap_or_else(|| "public".to_string());
            let host = host.unwrap_or_else(|| "127.0.0.1".to_string());
            let port = port.unwrap_or_else(|| "5433".to_string());
            let address = format!("{}:{}", host, port);

            let (ctx, _log) = if let Some(bin) = binary_file {
                get_base_session_context_from_binary(&bin, default_catalog.clone(), default_schema.clone()).await?
            } else {
                get_base_session_context(&schema_dir, default_catalog.clone(), default_schema.clone()).await?
            };

            start_server(
                Arc::new(ctx),
                &address,
                &default_catalog,
                &default_schema,
                capture.map(|p| p.into()),
            ).await?;
        }
        Commands::Compile { yaml_dir, binary_file } => {
            let schemas = parse_schema(&yaml_dir);
            crate::binary::write_binary(std::path::Path::new(&binary_file), &schemas)?;
        }
    }

    Ok(())
}


#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Err(e) = run().await {
        eprintln!("server crashed: {:?}", e);
    }
    Ok(())
}
