# pg_catalog_rs

**pg_catalog_rs** is a PostgreSQL system catalog compatibility layer for [Apache DataFusion](https://github.com/apache/datafusion).  
It enables PostgreSQL clients (e.g., DBeaver, pgAdmin, JDBC, ODBC, BI platforms) to connect to a DataFusion-backed database by emulating the behavior of PostgreSQL's `pg_catalog` schema.

This makes it possible to support tooling that expects PostgreSQL system metadata, even if the underlying engine is not Postgres.

Note: This is WIP heavily and API can change.

---

## Features
- Emulates core system tables from the `pg_catalog` schema:
  - `pg_class`
  - `pg_attribute`
  - `pg_namespace`
  - `pg_type`
  - `pg_index`
  - `pg_proc`

- Supports PostgreSQL-specific built-in functions:
  - `pg_get_constraintdef(oid)`
  - `current_database()`
  - `has_schema_privilege(...)`

- Allows standard metadata queries used by:
  - DBeaver, DataGrip, and other GUI tools
  - BI tools using JDBC or ODBC
  - Postgres CLI (`psql`)

> **Catalog coverage:** see [`CATALOG-REFERENCE.md`](CATALOG-REFERENCE.md) for the
> full list of catalog tables and views, how each is populated (seed vs. runtime
> registration), and per-view working/partial/broken status.

- Compatible with [`pgwire`](https://crates.io/crates/pgwire`) for Postgres wire protocol handling

- Fully in-memory and extensible via DataFusion APIs

---

## Installation

Add the crate from GitHub to your `Cargo.toml`:

```toml
[dependencies]
datafusion_pg_catalog = { git = "https://github.com/ybrs/pg_catalog" }
```

---

## Example Usage

Create a `SessionContext` preloaded with the catalog tables and register your own schema:
```rust
use std::collections::BTreeMap;
use datafusion_pg_catalog::{
    get_base_session_context, register_user_database, register_schema,
    register_user_tables, register_user_index, ColumnDef,
};

let (ctx, _log) = get_base_session_context(
    Some("pg_catalog_data/pg_schema"),
    "pgtry".to_string(),
    "public".to_string(),
    None, // optional current_schemas getter
).await?;

register_user_database(&ctx, "crm").await?;
register_schema(&ctx, "crm", "public").await?;

let mut cols = BTreeMap::new();
cols.insert(
    "id".to_string(),
    ColumnDef { col_type: "int".to_string(), nullable: false },
);
register_user_tables(&ctx, "crm", "public", "users", vec![cols]).await?;

// Optionally register an index so pg_indexes / pg_get_indexdef can describe it.
// Args: schema, index name, table, key column attnums, is_unique, is_primary.
register_user_index(&ctx, "public", "users_pkey", "users", vec![1], true, true).await?;
```

Then you can run queries like:

```sql
SELECT oid, relname FROM pg_class WHERE relnamespace = 2200;
SELECT attname FROM pg_attribute WHERE attrelid = 12345;
```

Or use DBeaver/psql to introspect the schema.

## Query Routing with `dispatch_query`

When executing SQL, call [`dispatch_query`](src/router.rs) to automatically
route catalog queries to the internal handler while forwarding all other
statements to your application logic.

The handler you pass receives `(ctx, sql, params, param_types)` and returns the
result rows together with their Arrow schema, so `dispatch_query` can hand back a
uniform `(Vec<RecordBatch>, Arc<Schema>)` whether the query was answered by the
catalog or by your handler.

```rust
use std::sync::Arc;
use datafusion_pg_catalog::router::dispatch_query;

let (batches, schema) = dispatch_query(
    &ctx,
    "SELECT * FROM pg_class",
    None, // bind parameter values
    None, // bind parameter types
    |ctx, sql, _params, _param_types| async move {
        let df = ctx.sql(sql).await?;
        let schema = Arc::new(df.schema().as_arrow().clone());
        let batches = df.collect().await?;
        Ok((batches, schema))
    },
).await?;
```

---

## Integration

- Works with the [`pgwire`](https://github.com/sunng87/pgwire) crate for wire protocol emulation
- Can be combined with custom `MemTable` or real storage backends (Parquet, Arrow, etc.)
- Designed to be embedded in hybrid SQL engines or compatibility layers

---

## Embedding the pgwire Server

Launch a PostgreSQL-compatible endpoint using the provided helper:

```rust
use std::sync::Arc;
use datafusion_pg_catalog::{get_base_session_context, start_server};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
        None,
    ).await?;

start_server(Arc::new(ctx), "127.0.0.1:5433", "pgtry", "public", None).await?;
    Ok(())
}
```

---

## Using with an Existing Server

If you already expose your own SQL server you can integrate `pg_catalog_rs` without
starting the bundled pgwire endpoint. Build a `SessionContext` and register your
tables, then call [`dispatch_query`](src/router.rs) from your handler. Catalog
queries will be executed internally and all other statements are forwarded.

```rust
use std::sync::Arc;
use datafusion_pg_catalog::router::dispatch_query;

async fn handle_request(ctx: &SessionContext, sql: &str) -> DFResult<Vec<RecordBatch>> {
    let (batches, _schema) = dispatch_query(
        ctx,
        sql,
        None,
        None,
        |ctx, sql, _params, _param_types| async move {
            let df = ctx.sql(sql).await?;
            let schema = Arc::new(df.schema().as_arrow().clone());
            let batches = df.collect().await?;
            Ok((batches, schema))
        },
    ).await?;
    Ok(batches)
}
```

---

## Runtime functions (statistics & live state)

Many `pg_catalog` views call server-runtime functions a static catalog cannot compute -
per-object statistics (`pg_stat_get_numscans`, ...), live session/lock state
(`pg_stat_get_activity`, `pg_lock_status`, ...), WAL/replication state, and more. They are
all registered, so those views are real, queryable views; with no callback installed each
returns its empty default (SQL `NULL`, or no rows), so the views work out of the box.

Supply values through typed **resolver** callbacks. Each function has its own explicit,
compile-time-checked setter named `set_<function>_resolver`:

```rust
use std::sync::Arc;
use datafusion_pg_catalog::{
    set_pg_stat_get_numscans_resolver,   // scalar:        Fn(oid) -> Option<i64>
    set_pg_lock_status_resolver,         // set-returning: Fn() -> Vec<PgLockStatusRow>
    PgLockStatusRow,
};

set_pg_stat_get_numscans_resolver(Arc::new(|relation_oid: i64| Some(scans_for(relation_oid))));

set_pg_lock_status_resolver(Arc::new(|| vec![
    PgLockStatusRow { locktype: Some("relation".into()), pid: Some(42), granted: Some(true),
                      ..Default::default() },
]));
```

See **[docs/runtime-functions.md](docs/runtime-functions.md)** for the full guide (the two
resolver shapes, the generated row structs, session identity, and caveats), and
**[docs/runtime-functions-reference.md](docs/runtime-functions-reference.md)** for the
copy-pasteable reference: every setter's exact signature and every row struct's full field
list, in Rust.

---

## Limitations

- No persistence - catalog is in-memory only
- Partial function support (more can be added)
- Schema reflection based on user-defined tables must be manually tracked
- No write-back support to catalog (read-only)

---

## Roadmap
- [ ] Hook into `CREATE TABLE` to auto-populate metadata
- [x] `pg_index` registration (eager `register_user_index` + lazy `LazyCatalogSource::indexes()`)
- [ ] Add remaining catalog tables for user objects (`pg_constraint`, `pg_attrdef`, ...)
- [ ] Deparse for `pg_get_indexdef` / `pg_get_viewdef` / `pg_get_ruledef`
- [ ] Catalog persistence to disk or external store
- [ ] Enhanced type inference and function overloads

---

## Testing

Functional tests are written in Python and executed with [pytest](https://docs.pytest.org/).
After installing the dependencies from `requirements.txt`, run:

```bash
pytest
```

---

## License

Licensed under either of:

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT license](LICENSE-MIT)

at your option.

---

## Contributing

Pull requests are welcome. Contributions that improve PostgreSQL compatibility or support new clients are especially appreciated.

Before submitting a change please run the Rust unit tests and the Python functional
tests:

```bash
cargo test
pytest
```

---

## Acknowledgments

This project is inspired by:

- PostgreSQL's system catalog architecture
- Apache DataFusion as the execution backend
- [`pgwire`](https://github.com/sunng87/pgwire) for wire protocol support

---
