use std::sync::{Arc, Mutex};
use arrow::record_batch::RecordBatch;
use arrow::util::pretty::print_batches;
use arrow::array::{StringArray, ArrayRef};
use arrow::datatypes::{DataType, Field, Schema};
use datafusion_pg_catalog::{dispatch_query, get_base_session_context};
use rusqlite::Connection;
use datafusion::error::Result as DFResult;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} '<SQL>'", args[0]);
        return Ok(());
    }
    let sql = args[1].clone();

    let (ctx, _log) = get_base_session_context(None, "datafusion".to_string(), "public".to_string(), None).await?;

    let conn = Arc::new(Mutex::new(Connection::open_in_memory()?));
    {
        let conn = conn.lock().map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?;
        conn.execute("CREATE TABLE users(id INTEGER, name TEXT)", [])
            .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?;
        conn.execute("INSERT INTO users VALUES (1,'Alice'),(2,'Bob')", [])
            .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?;
    }

    fn handle_sqlite(conn: &Arc<Mutex<Connection>>, query: &str) -> DFResult<(Vec<RecordBatch>, Arc<Schema>)> {
        let conn = conn
            .lock()
            .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?;
        let mut stmt = conn
            .prepare(query)
            .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?;
        let column_names = stmt.column_names().into_iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut rows = stmt
            .query([])
            .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?;
        let mut columns: Vec<Vec<String>> = vec![Vec::new(); column_names.len()];
        while let Some(row) = rows
            .next()
            .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?
        {
            for i in 0..column_names.len() {
                let v: rusqlite::types::Value = row
                    .get(i)
                    .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?;
                columns[i].push(format!("{:?}", v));
            }
        }
        let fields: Vec<Field> = column_names
            .iter()
            .map(|n| Field::new(n, DataType::Utf8, true))
            .collect();
        let arrays: Vec<ArrayRef> = columns
            .into_iter()
            .map(|c| Arc::new(StringArray::from(c)) as ArrayRef)
            .collect();
        let schema = Arc::new(Schema::new(fields));
        let batch = RecordBatch::try_new(schema.clone(), arrays)?;
        Ok((vec![batch], schema))
    }

    let handler = {
        let conn = conn.clone();
        move |_ctx, query: &str, _p, _t| {
            let conn = conn.clone();
            std::future::ready(handle_sqlite(&conn, query))
        }
    };

    let (batches, _schema) = dispatch_query(&ctx, &sql, None, None, handler).await?;
    print_batches(&batches)?;

    Ok(())
}
