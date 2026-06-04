# Task: Lazy (callback-driven) catalog definitions

## Goal

Let a `pg_catalog` user supply catalog metadata **lazily** through a callback
instead of eagerly pre-registering every database/schema/table/column at
startup. When a catalog query arrives, `pg_catalog` "pulls" the current set of
user objects from the callback and serves them merged with the built-in system
rows.

Primary motivation: a user whose objects live in an external/changing source
(e.g. an embedded SQL engine, a remote service, a config file — **anything**)
should not have to track changes and re-register. They register one source
object once; every catalog query reflects the live state.

## Non-goals / important constraints

- **`pg_catalog` is backend-agnostic and connection-free.** The public contract
  contains *only* catalog concepts (database/schema/relation/column names,
  OIDs). It must never reference a connection, a driver, or any specific engine.
  What backs a source (SQL engine, network service, file, in-memory `Vec`, or
  nothing) is entirely opaque to `pg_catalog`. DuckDB/teleduck is merely *one*
  possible consumer and its choices live in *its* code, not here.
- Do not remove existing files or functionality (per `AGENTS.md`).
- Every new function/struct/trait/method gets a docstring (per `CLAUDE.md`).
- Never fail silently: callback/conversion errors propagate as
  `DataFusionError` to the client (per `AGENTS.md`).
- New functionality must ship with tests; `cargo test` (and existing `pytest`)
  must pass before the task is considered done.

## Decisions (locked)

1. **API shape:** a Rust **trait** `LazyCatalogSource`, backend-neutral and
   connection-free. Its methods are **plain callbacks**: each takes a `callback`
   closure and the implementor calls it with the objects it found. No async,
   no channels, no tokio — pg_catalog calls the method and captures whatever the
   implementor passes to `callback`.
2. **Merge policy:** **MERGE** callback output with built-in system rows for
   *all* lazy tables, including `pg_database` and `pg_namespace`. (For
   `pg_class`/`pg_type`/`pg_attribute` merge is mandatory anyway — replacing them
   would delete built-in types like `int4`(23)/`text`(25) and the catalog's
   self-description. Built-in row counts confirm this: `pg_class` 415,
   `pg_type` 617, `pg_attribute` 3126.) The existing replace-semantics test is
   updated to assert merge.
3. **Freshness:** **always fresh per scan** — re-invoke the source on every
   catalog scan. No cache, no persistence, nothing stored. Oids come from the
   source on every call; consistent joins are the source's responsibility.
4. **Scope (this round):** **Rust `pg_catalog` core only.** Trait + generic
   provider + row-builder refactor + Tier 1/2 tables +
   Rust tests. Any downstream consumer integrates later, on its own, purely by
   implementing the `LazyCatalogSource` trait — that is not pg_catalog's concern.

## Current state (for reference)

- Static/eager path: `src/pg_catalog_helpers.rs` — `register_user_database`
  (:78), `register_schema` (:143), `register_user_tables` (:167). The last one
  expands one table into rows across `pg_class` (:212), `pg_type` (:230),
  `pg_attribute` (:238), `information_schema.tables` (:266),
  `information_schema.columns` (:302), stitched by OID. OIDs come from the global
  `NEXT_OID` (:19, base 50010). Type mapping helpers: `map_type_to_oid` (:57),
  `normalize_data_type_name` (:66).
- Built-in catalog: loaded from YAML in `src/session.rs` —
  `get_base_session_context` (:929) → `register_catalogs_from_schemas` (:785) →
  `build_table` (:586), each wrapped in `ObservableMemTable`
  (`src/db_table.rs:67`). Built-in OIDs top out ~13135 (< 50010).
- Existing lazy attempt: `src/lazy_pg_catalog_helpers.rs` — `LazyDatabaseProvider`
  (:19) wraps `pg_database` only, hand-builds Arrow column-by-column, **replace**
  semantics. `register_user_database_with_callback` (:257). Test asserting
  replace: `tests/lazy_pg_catalog.rs:56`.
- Query routing: `src/router.rs:328` `dispatch_query` sends catalog queries to
  the internal handler.

## The two hard problems and how the design solves them

**(A) Cross-table OID consistency is the source's job, not ours.** Tools join on
OIDs (`pg_attribute.attrelid = pg_class.oid`, `pg_class.relnamespace =
pg_namespace.oid`, `pg_class.reltype = pg_type.oid`). pg_catalog does not invent,
derive, or remember any oid — the source returns the oid for each object and
pg_catalog writes it through verbatim. If the source returns consistent oids,
joins resolve; if it returns garbage, that's on the source.

**(B) Merge with built-ins.** Each lazy provider serves *built-in rows captured
from the YAML-loaded table* **plus** *user rows from the callback*. The source is
responsible for keeping its oids clear of the built-in range (< 13135) if it
wants to join against built-in rows; pg_catalog does no dedup.

## Architecture

One generic `TableProvider` per catalog table. No central object, no shared
state. Each provider holds a clone of the user's source and its own built-in
rows, and answers `scan()` by asking the source.

```
 pg_database   pg_namespace   pg_class   pg_attribute   pg_type   information_schema.*
      └──────────────┴───── each is a LazyCatalogTableProvider ─────┴───────────┘
   LazyCatalogTableProvider {
       table:    which catalog table,
       schema:   SchemaRef (from the YAML-loaded table),
       builtin:  Vec<RecordBatch> (the built-in rows, captured at registration),
       source:   Arc<dyn LazyCatalogSource>,   // the user's callback object
   }
   scan(projection, filters, limit):
       user_rows = ask `source` for what THIS table needs (walk the hierarchy)
       batch     = rows_to_record_batch(schema, user_rows)
       serve `builtin ++ batch` via MemTable, passing projection/filters/limit
       // nothing cached, nothing remembered

   DataFusion does every join/filter/projection across these providers. We don't.
```

### Public contract (backend-neutral, connection-free)

```rust
/// Abstract source of *user* catalog metadata, backend-agnostic and
/// connection-free. Each method takes a `callback` and calls it with the
/// objects it found. How the implementor produces them (SQL engine, service,
/// file, in-memory, or empty) is opaque to pg_catalog. Built-in system rows are
/// added by the layer, so implementors return ONLY their own objects.
pub trait LazyCatalogSource: Send + Sync {
    /// User databases -> pg_catalog.pg_database.
    fn databases(&self, callback: &mut dyn FnMut(Vec<DatabaseDef>)) -> DFResult<()>;
    /// User schemas in `database` -> pg_catalog.pg_namespace.
    fn schemas(&self, database: &str, callback: &mut dyn FnMut(Vec<SchemaDef>)) -> DFResult<()>;
    /// User relations in `database`.`schema` -> pg_class + pg_type.
    fn relations(&self, database: &str, schema: &str,
                 callback: &mut dyn FnMut(Vec<RelationDef>)) -> DFResult<()>;
    /// Columns of `database`.`schema`.`relation`, ordinal order
    /// -> pg_attribute + information_schema.columns.
    fn columns(&self, database: &str, schema: &str, relation: &str,
               callback: &mut dyn FnMut(Vec<ColumnSpec>)) -> DFResult<()>;
}

// Each method returns DFResult<()> so a source can surface errors (per the
// "never fail silently" rule); the second parameter is still just the callback.
```

pg_catalog just calls the method and captures what the implementor passes to
the `callback` — plain and synchronous, no channels:

```rust
let mut dbs = Vec::new();
self.source.databases(&mut |rows| dbs = rows)?;   // dbs now holds the result
```

### Data model

**The source supplies every oid.** pg_catalog never invents, derives, or
allocates oids — it can't know what the user's world uses. The user is
responsible for making oids stable across calls, unique among their objects, and
(if they want to join against built-ins) clear of the built-in range `< 13135`.

```rust
/// One user database -> pg_catalog.pg_database. `oid` is user-supplied.
/// Optional pg_database fields reuse the set already on LazyDatabaseRow
/// (encoding, datistemplate, ...).
pub struct DatabaseDef { pub oid: i32, pub name: String, /* + optional fields */ }

/// One user schema -> pg_catalog.pg_namespace. `oid` is user-supplied.
pub struct SchemaDef { pub oid: i32, pub name: String, pub owner_oid: Option<i32> }

/// One user relation -> pg_class (+ pg_type rowtype). `oid` is the pg_class oid;
/// `reltype_oid` is the rowtype's pg_type oid. Both user-supplied. `kind`
/// selects pg_class.relkind ('r','v','m',...).
pub struct RelationDef { pub oid: i32, pub reltype_oid: i32, pub name: String, pub kind: RelationKind }
pub enum RelationKind { Table, View, MaterializedView /* ext as needed */ }

/// One column -> pg_attribute (+ information_schema.columns). attrelid comes from
/// the owning RelationDef.oid; attnum from ordinal position. The column's type
/// is given as a pg_type oid the user chooses (e.g. 23 for int4), so pg_catalog
/// does not have to know the user's type system. NOTE: pg_catalog_helpers has a
/// different `ColumnDef`, so this distinct name avoids a crate-root clash.
pub struct ColumnSpec { pub name: String, pub type_oid: i32, pub nullable: bool }
```

`DatabaseDef` reuses the optional metadata already modeled by `LazyDatabaseRow`.

### Oids come from the source (nothing stored)

pg_catalog asks the source for `(oid, name, ...)` whenever it needs them, uses
them as-is, and forgets them. If it needs them again, it asks again. **Nothing
is persisted or cached.** Whether the source returns consistent oids or garbage
is entirely the source's problem, not pg_catalog's.

It writes the returned oids straight into the catalog rows — `pg_class.oid =
RelationDef.oid`, `pg_class.relnamespace = SchemaDef.oid`, `pg_class.reltype =
RelationDef.reltype_oid`, `pg_attribute.attrelid = RelationDef.oid`,
`pg_attribute.atttypid = ColumnSpec.type_oid` — and that's all.

### Row building (per scan, inside the provider)

```rust
type Row = BTreeMap<String, serde_json::Value>;   // same row shape build_table consumes
```

Inside `scan()`, the provider asks the source only for what its own table needs
(`databases()` for `pg_database`; down to `columns()` for `pg_attribute`), takes
the oids straight off the returned objects, and builds that table's rows with the
shared row-builders (below). No `CatalogSnapshot`, no central object — just local
work for this one scan, thrown away when it returns.

### Generic provider (replaces bespoke Arrow code)

```rust
/// One catalog table. On scan it builds user rows by asking `source`, merges them
/// with the captured built-in batches, and serves the union via an in-memory
/// plan, honoring projection/filters/limit. Holds no shared/cached state.
struct LazyCatalogTableProvider {
    table: CatalogTable,                 // which catalog table this is
    schema: SchemaRef,                   // from the YAML-loaded table
    builtin: Vec<RecordBatch>,           // captured at registration (immutable)
    source: Arc<dyn LazyCatalogSource>,  // the user's callback object
}
async fn scan(&self, state, projection, filters, limit) -> DFResult<Arc<dyn ExecutionPlan>> {
    let user_rows  = build_rows_for(self.table, &*self.source);   // asks source, here & now
    let user_batch = rows_to_record_batch(&self.schema, &user_rows)?;   // reused helper
    let mut batches = self.builtin.clone();
    batches.push(user_batch);
    MemTable::try_new(self.schema.clone(), vec![batches])?
        .scan(state, projection, filters, limit).await
}
```

### Registration entry point

```rust
/// Install lazy providers over the catalog + information_schema tables, sourcing
/// user rows from `source`. MUST be called right after get_base_session_context
/// and BEFORE any static register_user_* call, so the captured built-in batches
/// contain only the YAML system rows.
pub fn register_lazy_catalog(
    ctx: &SessionContext,
    source: Arc<dyn LazyCatalogSource>,
    opts: LazyCatalogOptions,
) -> DFResult<()>;
```

For each target table it: looks up the current provider, captures its built-in
batches (scan inner once via `ctx.state()` and `collect`), and registers a
`LazyCatalogTableProvider` in its place.

## Refactors required (DRY — one source of truth for row shapes)

1. **Extract `rows_to_record_batch(schema, rows) -> RecordBatch`** from
   `build_table` (`src/session.rs:612–763`, incl. list/binary handling). Make
   `build_table` call it. The lazy providers and (retrofit) `LazyDatabaseProvider`
   reuse it, deleting ~90 lines of hand-rolled Arrow builders.
2. **Extract pure row-builders** from `register_user_tables`
   (`src/pg_catalog_helpers.rs:167`), each returning `Row`(s):
   `build_pg_class_row`, `build_pg_type_rowtype_row`, `build_pg_attribute_rows`,
   `build_info_tables_row`, `build_info_columns_rows`. Static path formats Rows
   into `INSERT`s (as today); lazy path converts Rows to batches. Reuse existing
   `map_type_to_oid` / `normalize_data_type_name`. (Retrofitting the static path
   onto these builders is recommended to prevent drift but may be staged if it
   risks the existing tests.) The lazy path takes oids from the source objects;
   the static path keeps its existing `NEXT_OID` (the two paths are never used
   together for the same context, per the "no intersection" rule).

## Tables covered, in tiers

- **Tier 1 (MVP):** migrate `pg_database` to the merge mechanism; add
  `pg_namespace`, `pg_class`, `pg_attribute`. Proves the join story
  (`pg_class ⋈ pg_namespace ⋈ pg_attribute`).
- **Tier 2:** `pg_type` (user composite rowtypes, merged with 617 built-ins) +
  `information_schema.tables` / `columns` / `schemata`.
- **Tier 3 (later):** equality-filter pushdown to the source (`relname=`,
  `nspname=`, `datname=`) for very large catalogs; `pg_description`/comments.

## Migration of the existing lazy code

- Fold `pg_database` into the generic mechanism. Retrofit `LazyDatabaseProvider`
  to use `rows_to_record_batch`, or replace it with `LazyCatalogTableProvider`.
- Keep `register_user_database_with_callback` working (don't remove
  functionality). Reimplement it as a thin shim that builds a source exposing
  only `databases()` and delegates to the new internals — now with **merge**
  semantics.
- Update `tests/lazy_pg_catalog.rs`:
  - `test_lazy_register_pg_database_on_scan` still valid (the two callback DBs
    appear; merge keeps that true).
  - `test_lazy_replaces_pg_database_rows` → rewrite as
    `test_lazy_merges_pg_database_rows`: assert built-ins
    (`postgres`/`template0`/`template1`) **and** callback rows
    (`only_lazy_1`/`only_lazy_2`) are all present.

## File-by-file change list

- **NEW** `src/lazy_catalog.rs` (or grow `lazy_pg_catalog_helpers.rs`): trait
  `LazyCatalogSource`, `DatabaseDef`/`SchemaDef`/`RelationDef`/`ColumnSpec`/
  `RelationKind`, `CatalogTable`, `build_rows_for`, `LazyCatalogTableProvider`,
  `LazyCatalogOptions`, `register_lazy_catalog`.
- **MODIFY** `src/session.rs`: extract `rows_to_record_batch`.
- **MODIFY** `src/pg_catalog_helpers.rs`: extract row-builders (shared by the
  static path; the lazy path takes oids from the source).
- **MODIFY** `src/lazy_pg_catalog_helpers.rs`: retrofit onto new mechanism.
- **MODIFY** `src/lib.rs`: export new public items.
- **MODIFY** `tests/lazy_pg_catalog.rs`: flip replace→merge; add integration tests.

## Testing plan (Rust)

Use a fake in-memory `LazyCatalogSource` (proves backend-neutrality — no DB
involved) returning 2 databases × schemas × relations × columns.

1. **Joins:** `SELECT a.attname FROM pg_class c JOIN pg_namespace n ON
   n.oid=c.relnamespace JOIN pg_attribute a ON a.attrelid=c.oid WHERE
   c.relname='users' AND n.nspname='public'` returns the expected columns.
2. **Built-ins survive (merge):** `WHERE typname='int4'` still returns 23; a
   built-in catalog self-row (e.g. `pg_class` where `relname='pg_class'`) is
   still present alongside user relations.
3. **Oid pass-through:** the oid the source returns for `users` appears verbatim
   in `pg_class.oid` and is used as `pg_attribute.attrelid` (so the join in #1
   resolves). pg_catalog does not alter or remember it.
4. **No intersection:** with a source active, a `register_user_tables` call's
   rows do not appear (source is authoritative for user objects).
5. **Projection/filter passthrough:** projected/filtered catalog scans return
   correct columns/rows.
6. **information_schema:** `information_schema.columns` for a user relation has
   the right `data_type`/`is_nullable`/`ordinal_position`.
7. **Error propagation:** a source method returning `Err` surfaces as a query
   error (no silent empty result).

Run `cargo test` (and `pytest` for regressions) before declaring done.

## Follow-up (out of scope this round)

- **Filter pushdown** to the source (`relname=`, `nspname=`, `datname=`) so huge
  catalogs aren't fully enumerated per query.

Downstream consumers are entirely out of scope here: they integrate by
implementing `LazyCatalogSource` in their own code and calling
`register_lazy_catalog`. pg_catalog neither knows nor cares what backs them.
