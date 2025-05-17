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
  - `pg_proc`

- Supports PostgreSQL-specific built-in functions:
  - `pg_get_constraintdef(oid)`
  - `current_database()`
  - `has_schema_privilege(...)`

- Allows standard metadata queries used by:
  - DBeaver, DataGrip, and other GUI tools
  - BI tools using JDBC or ODBC
  - Postgres CLI (`psql`)

- Compatible with [`pgwire`](https://crates.io/crates/pgwire`) for Postgres wire protocol handling

- Fully in-memory and extensible via DataFusion APIs

---

## Example Usage

Register the catalog tables into your existing `SessionContext`:

```rust
use datafusion::execution::context::SessionContext;

let ctx = SessionContext::new();
pg_catalog_rs::register_pg_catalog_tables(&ctx).await?;
```

Then you can run queries like:

```sql
SELECT oid, relname FROM pg_class WHERE relnamespace = 2200;
SELECT attname FROM pg_attribute WHERE attrelid = 12345;
```

Or use DBeaver/psql to introspect the schema.

---

## Integration

- Works with the [`pgwire`](https://github.com/sunng87/pgwire) crate for wire protocol emulation
- Can be combined with custom `MemTable` or real storage backends (Parquet, Arrow, etc.)
- Designed to be embedded in hybrid SQL engines or compatibility layers

---

## Limitations

- ❌ No persistence — catalog is in-memory only
- 🟡 Partial function support (more can be added)
- 🟠 Schema reflection based on user-defined tables must be manually tracked
- ❌ No write-back support to catalog (read-only)

---

## Roadmap
- [ ] Hook into `CREATE TABLE` to auto-populate metadata
- [ ] Add missing catalog tables (`pg_index`, `pg_constraint`, etc.)
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

---

## Acknowledgments

This project is inspired by:

- PostgreSQL's system catalog architecture
- Apache DataFusion as the execution backend
- [`pgwire`](https://github.com/sunng87/pgwire) for wire protocol support

---
