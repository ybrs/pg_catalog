# Example: lazy `pg_catalog` over a live SQLite schema

This `pg_catalog_lazy_example` crate shows the **lazy, callback-driven catalog**.
Instead of pre-registering every table, it implements a `LazyCatalogSource` that
reads the SQLite schema on demand and registers it once with
`register_lazy_catalog`. Every catalog scan then reflects the *current* SQLite
tables, merged with the built-in PostgreSQL system rows.

It uses the in-tree `pg_catalog` crate (`path = ".."`), so it exercises your
local changes — not a published build.

## What it sets up

- An in-memory SQLite database `appdb` seeded with two tables:
  - `users(id INTEGER NOT NULL, name TEXT)`
  - `orders(id INTEGER NOT NULL, user_id INTEGER, status TEXT)`
- A `SqliteCatalogSource` that maps SQLite → catalog:
  - databases → `pg_database` (`appdb`)
  - schemas → `pg_namespace` (`public`)
  - tables → `pg_class` (+ rowtype in `pg_type`)
  - columns (`PRAGMA table_info`) → `pg_attribute` + `information_schema.columns`
- OIDs are derived deterministically from table names, so cross-table joins
  (`pg_attribute.attrelid = pg_class.oid`) resolve.

## Building

```bash
cargo build
```

## Running it manually

Pass a SQL query as the argument. Queries that touch `pg_catalog` /
`information_schema` are answered by the catalog layer (lazily, from SQLite);
everything else runs against SQLite directly.

```bash
# Data query — runs against SQLite
cargo run -- "SELECT id, name FROM users"

# The user tables show up in pg_class, lazily, alongside the built-ins
cargo run -- "SELECT relname FROM pg_catalog.pg_class WHERE relname IN ('users','orders')"

# Join across pg_class ⋈ pg_namespace ⋈ pg_attribute — the columns of 'users'
cargo run -- "SELECT n.nspname, c.relname, a.attname \
  FROM pg_catalog.pg_class c \
  JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
  JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
  WHERE c.relname = 'users' ORDER BY a.attnum"

# information_schema reflects the live columns of 'orders'
cargo run -- "SELECT table_name, column_name, data_type, is_nullable \
  FROM information_schema.columns \
  WHERE table_name = 'orders' ORDER BY ordinal_position"

# Built-ins still survive the merge (int4 is oid 23)
cargo run -- "SELECT typname, oid FROM pg_catalog.pg_type WHERE typname = 'int4'"

# The lazy database appears in pg_database next to postgres/template0/template1
cargo run -- "SELECT datname FROM pg_catalog.pg_database ORDER BY datname"
```

## Interactive mode (watch freshness live)

Run with **no argument** to drop into a SQL REPL. One statement per line; catalog
queries are answered lazily, DDL/DML runs against SQLite. Create a table and
watch it appear in `pg_class` on the very next query — nothing is re-registered:

```bash
cargo run
```

```
sql> SELECT relname FROM pg_catalog.pg_class WHERE relname = 'invoices';
++          -- not there yet
sql> CREATE TABLE invoices(id INTEGER NOT NULL, amount INTEGER, memo TEXT);
sql> SELECT relname FROM pg_catalog.pg_class WHERE relname = 'invoices';
+----------+
| relname  |
| invoices |   -- now present, lazily
+----------+
sql> SELECT column_name, data_type FROM information_schema.columns WHERE table_name = 'invoices';
sql> \q
```

You can also pipe a script in: `printf '...\n...\n' | cargo run`.

## Freshness

Because the source is re-invoked on every scan and nothing is cached, the catalog
always reflects the current SQLite schema — as the REPL session above shows.
