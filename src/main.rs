mod session;
mod replace;
mod clean_duplicate_columns;
use std::env;
use arrow::util::pretty;
use crate::session::{print_execution_log, get_session_context, execute_sql};

#[tokio::main]
async fn main() -> datafusion::error::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        println!("Usage: {} schema.yaml \"SQL_QUERY\"", args[0]);
        std::process::exit(1);
    }

    let schema_path = &args[1];
    let sql = &args[2];
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

    let (ctx, log) = get_session_context(schema_path, default_catalog.clone(), default_schema.clone())?;
    let results = execute_sql(&ctx, sql.as_str()).await?;
    pretty::print_batches(&results)?;
    print_execution_log(log.clone());
    Ok(())
}
