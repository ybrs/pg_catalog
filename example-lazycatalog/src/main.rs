//! Example: lazy, callback-driven `pg_catalog` backed by a live SQLite schema.
//!
//! This mirrors the sibling `example/` crate (route data queries to an in-memory
//! SQLite database, catalog queries to the `pg_catalog` layer) but additionally
//! wires up the **lazy catalog** mechanism. A [`LazyCatalogSource`] reads the
//! SQLite schema on demand, so every scan of `pg_catalog.pg_class` /
//! `pg_namespace` / `pg_attribute` / `pg_type` / `information_schema.*` reflects
//! the *current* set of SQLite tables — nothing is pre-registered.
//!
//! Run a query by passing it as an argument:
//! ```bash
//! cargo run -- "SELECT relname FROM pg_catalog.pg_class WHERE relname IN ('users','orders')"
//! ```

use std::sync::{Arc, Mutex};
use std::time::Instant;

use arrow::array::{ArrayRef, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use arrow::util::pretty::print_batches;
use datafusion::error::{DataFusionError, Result as DFResult};
use datafusion_pg_catalog::{
    dispatch_query, get_base_session_context_with_lazy_catalog, ColumnSpec, DatabaseDef,
    LazyCatalogOptions, LazyCatalogSource, LazyDatabaseRow, RelationDef, SchemaDef,
};
use rusqlite::Connection;

/// The single logical database this example exposes to `pg_catalog`.
const DB_NAME: &str = "appdb";
/// `pg_database.oid` for [`DB_NAME`] — chosen above the built-in OID range.
const DB_OID: i32 = 16384;
/// The single schema this example exposes.
const SCHEMA_NAME: &str = "public";
/// `pg_namespace.oid` for [`SCHEMA_NAME`] — chosen above the built-in OID range.
const SCHEMA_OID: i32 = 16385;

/// A tiny deterministic string hash (djb2). Used to derive stable OIDs from
/// table names so the SAME oid comes back on every callback — which is what lets
/// `pg_class.oid` and `pg_attribute.attrelid` agree and the joins resolve.
fn djb2(s: &str) -> u32 {
    let mut hash: u32 = 5381;
    for b in s.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(b as u32);
    }
    hash
}

/// Stable `pg_class.oid` for a table name (range 20000..25000, clear of built-ins).
fn relation_oid(name: &str) -> i32 {
    20000 + (djb2(name) % 5000) as i32
}

/// Stable `pg_type.oid` for a table's composite rowtype (range 30000..35000).
fn reltype_oid(name: &str) -> i32 {
    30000 + (djb2(name) % 5000) as i32
}

/// Map a SQLite declared column type to a PostgreSQL `pg_type` OID.
/// Only the common affinities are needed for the demo; everything else is text.
fn sqlite_type_to_oid(decl_type: &str) -> i32 {
    let t = decl_type.to_uppercase();
    if t.contains("INT") {
        23 // int4
    } else {
        25 // text
    }
}

/// A [`LazyCatalogSource`] that reflects the live SQLite schema.
///
/// It holds a handle to the same connection used for data queries, so adding a
/// table in SQLite is immediately visible to the next catalog scan. Every method
/// locks the connection, reads what it needs, releases the lock, and hands the
/// rows back through the callback — all synchronous, exactly as the trait wants.
struct SqliteCatalogSource {
    /// Shared handle to the in-memory SQLite database.
    conn: Arc<Mutex<Connection>>,
}

impl SqliteCatalogSource {
    /// List the user tables currently present in SQLite (excluding internal
    /// `sqlite_*` tables), ordered by name for stable output.
    fn list_tables(&self) -> DFResult<Vec<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| DataFusionError::Execution(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master \
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .map_err(|e| DataFusionError::Execution(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| DataFusionError::Execution(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| DataFusionError::Execution(e.to_string()))?);
        }
        Ok(out)
    }

    /// Read the columns of `table` via `PRAGMA table_info`, returning
    /// `(name, type_oid, nullable)` in ordinal order.
    fn list_columns(&self, table: &str) -> DFResult<Vec<ColumnSpec>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| DataFusionError::Execution(e.to_string()))?;
        // Table names here come from sqlite_master, so a quoted identifier is safe.
        let sql = format!("PRAGMA table_info(\"{}\")", table.replace('"', "\"\""));
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DataFusionError::Execution(e.to_string()))?;
        // PRAGMA table_info columns: cid, name, type, notnull, dflt_value, pk
        let rows = stmt
            .query_map([], |row| {
                let name: String = row.get(1)?;
                let decl_type: String = row.get(2)?;
                let notnull: i64 = row.get(3)?;
                Ok((name, decl_type, notnull))
            })
            .map_err(|e| DataFusionError::Execution(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            let (name, decl_type, notnull) =
                r.map_err(|e| DataFusionError::Execution(e.to_string()))?;
            out.push(ColumnSpec::new(
                name,
                sqlite_type_to_oid(&decl_type),
                notnull == 0,
            ));
        }
        Ok(out)
    }
}

impl LazyCatalogSource for SqliteCatalogSource {
    /// One database: `appdb`.
    fn databases(&self, callback: &mut dyn FnMut(Vec<DatabaseDef>)) -> DFResult<()> {
        let mut db = LazyDatabaseRow::new(DB_NAME, 10);
        db.oid = Some(DB_OID);
        callback(vec![db]);
        Ok(())
    }

    /// One schema (`public`) under `appdb`.
    fn schemas(&self, database: &str, callback: &mut dyn FnMut(Vec<SchemaDef>)) -> DFResult<()> {
        if database == DB_NAME {
            callback(vec![SchemaDef::new(SCHEMA_OID, SCHEMA_NAME)]);
        }
        Ok(())
    }

    /// Every SQLite table in `appdb.public`, with stable name-derived OIDs.
    fn relations(
        &self,
        database: &str,
        schema: &str,
        callback: &mut dyn FnMut(Vec<RelationDef>),
    ) -> DFResult<()> {
        if database != DB_NAME || schema != SCHEMA_NAME {
            return Ok(());
        }
        let relations = self
            .list_tables()?
            .into_iter()
            .map(|name| RelationDef::table(relation_oid(&name), reltype_oid(&name), name))
            .collect();
        callback(relations);
        Ok(())
    }

    /// The columns of a SQLite table, read live via `PRAGMA table_info`.
    fn columns(
        &self,
        database: &str,
        schema: &str,
        relation: &str,
        callback: &mut dyn FnMut(Vec<ColumnSpec>),
    ) -> DFResult<()> {
        if database != DB_NAME || schema != SCHEMA_NAME {
            return Ok(());
        }
        callback(self.list_columns(relation)?);
        Ok(())
    }
}

/// Execute a data query against SQLite and return the rows as Arrow batches.
/// (Values are stringified for simplicity, exactly like the sibling `example/`.)
fn handle_sqlite(
    conn: &Arc<Mutex<Connection>>,
    query: &str,
) -> DFResult<(Vec<RecordBatch>, Arc<Schema>)> {
    let conn = conn
        .lock()
        .map_err(|e| DataFusionError::Execution(e.to_string()))?;
    let mut stmt = conn
        .prepare(query)
        .map_err(|e| DataFusionError::Execution(e.to_string()))?;

    // Non-row-returning statements (CREATE TABLE, INSERT, ...) have no result
    // columns: execute them and report a short status instead of scanning rows.
    if stmt.column_count() == 0 {
        let affected = stmt
            .execute([])
            .map_err(|e| DataFusionError::Execution(e.to_string()))?;
        // Only DML actually affects rows; DDL (CREATE/DROP/ALTER) should just say OK
        // rather than echo SQLite's stale change counter.
        let verb = query
            .trim_start()
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_uppercase();
        let status = match verb.as_str() {
            "INSERT" | "UPDATE" | "DELETE" => format!("OK ({affected} row(s) affected)"),
            _ => "OK".to_string(),
        };
        let schema = Arc::new(Schema::new(vec![Field::new("status", DataType::Utf8, true)]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(StringArray::from(vec![status])) as ArrayRef],
        )?;
        return Ok((vec![batch], schema));
    }

    let column_names = stmt
        .column_names()
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    let mut rows = stmt
        .query([])
        .map_err(|e| DataFusionError::Execution(e.to_string()))?;
    let mut columns: Vec<Vec<String>> = vec![Vec::new(); column_names.len()];
    while let Some(row) = rows
        .next()
        .map_err(|e| DataFusionError::Execution(e.to_string()))?
    {
        for i in 0..column_names.len() {
            let v: rusqlite::types::Value = row
                .get(i)
                .map_err(|e| DataFusionError::Execution(e.to_string()))?;
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

/// Seed the in-memory SQLite database with a couple of tables and rows so the
/// catalog has something to reflect.
fn seed_database(conn: &Arc<Mutex<Connection>>) -> DFResult<()> {
    let conn = conn
        .lock()
        .map_err(|e| DataFusionError::Execution(e.to_string()))?;
    conn.execute_batch(
        "CREATE TABLE users(id INTEGER NOT NULL, name TEXT);
         INSERT INTO users VALUES (1,'Alice'),(2,'Bob');
         CREATE TABLE orders(id INTEGER NOT NULL, user_id INTEGER, status TEXT);
         INSERT INTO orders VALUES (10,1,'open'),(11,2,'shipped');",
    )
    .map_err(|e| DataFusionError::Execution(e.to_string()))?;
    Ok(())
}

/// Run one SQL statement through the catalog/SQLite router and print the result.
///
/// `pg_catalog`/`information_schema` queries are answered lazily by the catalog
/// layer; everything else (including DDL/DML) runs against SQLite via the shared
/// connection.
async fn run_one(
    ctx: &datafusion::execution::context::SessionContext,
    conn: &Arc<Mutex<Connection>>,
    sql: &str,
) -> DFResult<()> {
    let handler = {
        let conn = conn.clone();
        move |_ctx, query: &str, _p, _t| {
            let conn = conn.clone();
            std::future::ready(handle_sqlite(&conn, query))
        }
    };
    let started = Instant::now();
    let (batches, _schema) = dispatch_query(ctx, sql, None, None, handler).await?;
    let elapsed = started.elapsed();

    let total_rows: usize = batches.iter().map(|b| b.num_rows()).sum();
    if total_rows == 0 {
        // An empty result set renders as a bare "++" box, which looks broken.
        // Print a clear marker instead.
        println!("(0 rows)");
    } else {
        print_batches(&batches)?;
    }
    // psql `\timing on` style: report how long the query took, in milliseconds.
    println!("Time: {:.3} ms", elapsed.as_secs_f64() * 1000.0);
    Ok(())
}

/// Interactive loop backed by `rustyline`, so it supports line editing: up/down
/// arrows for history, left/right to move the cursor, Ctrl-A/E, Ctrl-R search,
/// etc. One statement per line. Because the catalog source is re-read on every
/// scan, a `CREATE TABLE` here shows up in `pg_catalog.pg_class` on the very next
/// query.
async fn run_repl(
    ctx: &datafusion::execution::context::SessionContext,
    conn: &Arc<Mutex<Connection>>,
) -> anyhow::Result<()> {
    use rustyline::error::ReadlineError;
    use rustyline::DefaultEditor;

    println!("pg_catalog lazy example — interactive SQL.");
    println!("Line editing: up/down = history, left/right = move cursor, Ctrl-R = search.");
    println!("Ctrl-D or \\q to quit. Try, in order:");
    println!("  SELECT relname FROM pg_catalog.pg_class WHERE relname = 'invoices';");
    println!("  CREATE TABLE invoices(id INTEGER NOT NULL, amount INTEGER);");
    println!("  SELECT relname FROM pg_catalog.pg_class WHERE relname = 'invoices';");
    println!("  SELECT column_name, data_type FROM information_schema.columns WHERE table_name = 'invoices';");

    let mut editor = DefaultEditor::new()?;
    loop {
        match editor.readline("sql> ") {
            Ok(line) => {
                let stmt = line.trim().trim_end_matches(';').trim().to_string();
                if stmt.is_empty() {
                    continue;
                }
                // Record the entry so the up arrow recalls it.
                let _ = editor.add_history_entry(stmt.as_str());
                if stmt == "\\q"
                    || stmt.eq_ignore_ascii_case("quit")
                    || stmt.eq_ignore_ascii_case("exit")
                {
                    break;
                }
                if let Err(e) = run_one(ctx, conn, &stmt).await {
                    eprintln!("error: {e}");
                }
            }
            // Ctrl-C cancels the current line; keep going.
            Err(ReadlineError::Interrupted) => continue,
            // Ctrl-D on an empty line quits.
            Err(ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("error: {e}");
                break;
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // One in-memory SQLite database, shared by the data handler and the catalog source.
    let conn = Arc::new(Mutex::new(
        Connection::open_in_memory().map_err(|e| DataFusionError::Execution(e.to_string()))?,
    ));
    seed_database(&conn)?;

    // Build the session with the lazy catalog wired in BEFORE the catalog views
    // are created, so views such as pg_tables/pg_views resolve against the lazy
    // providers and reflect the live SQLite schema too — not just the base tables.
    let source: Arc<dyn LazyCatalogSource> = Arc::new(SqliteCatalogSource { conn: conn.clone() });
    let load_started = Instant::now();
    let (ctx, _log) = get_base_session_context_with_lazy_catalog(
        None,
        "datafusion".to_string(),
        "public".to_string(),
        None,
        source,
        LazyCatalogOptions::all(),
    )
    .await?;
    println!(
        "Catalog loaded in {:.2}s",
        load_started.elapsed().as_secs_f64()
    );

    // With a SQL argument, run it once and exit; otherwise drop into the REPL.
    if args.len() >= 2 {
        run_one(&ctx, &conn, &args[1]).await?;
    } else {
        run_repl(&ctx, &conn).await?;
    }

    Ok(())
}
