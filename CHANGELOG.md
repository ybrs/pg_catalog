# Changelog

This crate is unpublished (`publish = false`, still `version = "0.1.0"`) and
has no versioned releases, so this file tracks notable batches merged to
`main` by date instead of by version number.

## 2026-10-27 (`main` at `0d4b691`)

Session identity, session settings, new catalog sources, and dependency
upgrades (24 commits over `c21898c`).

### Session identity reads the connection

`current_user`, `session_user`, `current_role` (and their `user` /
`current_catalog` / `current_schema` / `current_schemas` siblings, in every
accepted spelling) now read the connection's `ClientOpts` at call time,
including from inside catalog view bodies planned before the connection
existed. The host records the role with `set_session_user` on the
connection's own context. New modules: `session_identity_spellings`,
`function_definitions`; new UDF: `current_setting(name[, missing_ok])`,
answered from live session state with a `pg_settings` snapshot fallback.

### Session `SET` is applied by the executor

`classify_set_statement` + `SetStatementDisposition` (re-exported at the
crate root) route `SET` on a stored variable to the session itself, so the
behavior no longer depends on whether the embedding server classifies first:

- Multi-value form works: `SET search_path TO a, b` stores the list.
- Bound parameters work: `SET application_name = $1` applies the bound value.
- `SET <variable> = DEFAULT` restores the connection default.
- `SET LOCAL` answers `Ok` without applying, matching `PostgreSQL` outside a
  transaction.

`ClientOpts::set` no longer accepts `session_user`: setting it through SQL
fails with `unknown key session_user` instead of silently switching roles.
`set_session_user` (the host API) is unchanged.

### `SHOW` coverage on the standalone server

The server answers `timezone`, `session_user`, `session_authorization`,
`server_version` and `transaction_isolation`, plus a multi-row `SHOW ALL`,
and installs the authenticated user before answering builtin `SHOW`s so
`SHOW` and `SELECT` agree on the same connection.

### New catalog sources (breaking, see below)

- `DeclaredCatalogSource::from_declarations`: objects known up front, with
  OIDs from `CatalogOidRegistry` (persistent redb-backed or in-memory;
  `public` keeps canonical 2200). Duplicates are refused `PostgreSQL`-style.
- `CombinedCatalogSource`: merges any number of sources; shared schema names
  merge, database/relation collisions are refused loudly.
- `LazyCatalogSource` path unchanged in shape; both paths share the same
  row builders (`build_pg_class_row`, ...).

All 136 declared views are now served as real views (98 working, 38 partial
by documented design); a view that fails to plan fails startup instead of
being served from frozen fixture rows. Details in `CATALOG-REFERENCE.md`.

### Breaking: eager registration API removed

`register_user_database`, `register_user_tables`, `register_user_view`,
`register_user_index`, `register_user_constraint`, `register_schema`,
`unregister_database`, `unregister_schema`, `unregister_tables`, and the
`NEXT_OID` counter are gone. Migrate to declarations:

- database -> `DatabaseDef`, schema -> `SchemaDef`,
  table/columns -> `RelationDef` + `ColumnSpec`/`ColumnDef`
  (note `ColumnDef.has_default`, default false),
- serve with `get_base_session_context_with_lazy_catalog`
  (or merge sources with `CombinedCatalogSource`),
- OIDs via `CatalogOidRegistry` instead of threading them by hand;
  removal means dropping the declaration and rebuilding.

`src/main.rs::demo_catalog` is the worked example. `get_base_session_context`,
`execute_sql`, `dispatch_query`, `register_lazy_catalog` and `start_server`
signatures are unchanged. Rust registration-function rename:
`register_scalar_pg_encoding_to_char` is now
`register_scalar_encoding_name_functions` (the SQL function
`pg_encoding_to_char(n)` is unchanged and now also accepts `Int64`).

### Dependencies

- `datafusion` 54.0.0 -> 55.2.0 (and `datafusion-functions-aggregate`
  likewise), `arrow` 58.3.0 -> 59.3.2, `pgwire` 0.40 -> 0.42, `zip` 8 -> 9.
- `sqlparser` held at 0.62 and `arrow` held below 60: `datafusion` 55 pins
  both, and newer majors would fork the shared types.
- `zip` 9 made `ZipFile::name` fallible; unreadable entry names fail (or are
  skipped with a log line) like any other invalid entry.
- The bundled examples track the parent's `datafusion`/`arrow` exactly, as
  their manifests already require.
