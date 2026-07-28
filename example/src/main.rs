use arrow::array::{ArrayRef, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use arrow::util::pretty::print_batches;
use datafusion::error::Result as DFResult;
use datafusion_pg_catalog::{dispatch_query, get_base_session_context};
use rusqlite::Connection;
use std::sync::{Arc, Mutex};

/// Run `query` against the `SQLite` connection and return the rows as a single
/// Arrow batch, every column typed as text.
///
/// The example's point is routing, not fidelity: values are rendered with
/// their Debug form so any `SQLite` type can be returned without a type map.
///
/// # Errors
///
/// Returns an error if the connection mutex is poisoned, if `SQLite` rejects
/// the statement, or if the collected columns do not form a valid batch.
fn handle_sqlite(
    conn: &Arc<Mutex<Connection>>,
    query: &str,
) -> DFResult<(Vec<RecordBatch>, Arc<Schema>)> {
    let conn = conn
        .lock()
        .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?;
    let mut stmt = conn
        .prepare(query)
        .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?;
    let column_names = stmt
        .column_names()
        .into_iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>();
    let mut rows = stmt
        .query([])
        .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?;
    let mut columns: Vec<Vec<String>> = vec![Vec::new(); column_names.len()];
    while let Some(row) = rows
        .next()
        .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?
    {
        for (index, column) in columns.iter_mut().enumerate() {
            let v: rusqlite::types::Value = row
                .get(index)
                .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?;
            column.push(format!("{v:?}"));
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} '<SQL>'", args[0]);
        return Ok(());
    }
    let sql = args[1].clone();

    let (ctx, _log) =
        get_base_session_context(None, "datafusion".to_string(), "public".to_string(), None)
            .await?;

    let conn = Arc::new(Mutex::new(Connection::open_in_memory()?));
    {
        let conn = conn
            .lock()
            .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?;
        conn.execute("CREATE TABLE users(id INTEGER, name TEXT)", [])
            .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?;
        conn.execute("INSERT INTO users VALUES (1,'Alice'),(2,'Bob')", [])
            .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?;
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
