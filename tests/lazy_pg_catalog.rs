use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use arrow::array::Array;
use datafusion::error::{DataFusionError, Result as DFResult};
use datafusion_pg_catalog::{
    clear_index_definition_resolver, clear_view_definition_resolver, get_base_session_context,
    get_base_session_context_with_lazy_catalog, register_database_independent_lazy_catalog,
    register_lazy_catalog, register_user_database_with_callback, set_index_definition_resolver,
    set_view_definition_resolver, CatalogTable, ColumnSpec, ConfigSettingDef, ConstraintDef,
    DatabaseDef, IndexDef, IndexDefinitionResolver, IndexIdentity, LazyCatalogOptions,
    LazyCatalogSource, LazyDatabaseRow, RelationDef, RelationKind, SchemaDef, SettingDef,
    ViewDefinitionResolver, ViewIdentity,
};

/// Collect a single-column `StringArray` result into a `Vec<String>`.
async fn string_column(
    ctx: &datafusion::execution::context::SessionContext,
    sql: &str,
) -> DFResult<Vec<String>> {
    let batches = ctx.sql(sql).await?.collect().await?;
    let mut out = Vec::new();
    for b in &batches {
        let arr = b
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .expect("expected a Utf8 column");
        for i in 0..arr.len() {
            if arr.is_valid(i) {
                out.push(arr.value(i).to_string());
            }
        }
    }
    Ok(out)
}

/// Collect a single-column text result into a `Vec<String>`, casting whatever
/// string representation the engine returns (`Utf8`, `LargeUtf8`, `Utf8View`, a
/// dictionary, ...) to `Utf8` first. Use this for catalog columns whose Arrow
/// string flavor is not guaranteed (e.g. the `"char"` `contype`, or values that
/// pass through `information_schema` domain casts).
async fn text_column(
    ctx: &datafusion::execution::context::SessionContext,
    sql: &str,
) -> DFResult<Vec<String>> {
    let batches = ctx.sql(sql).await?.collect().await?;
    let mut out = Vec::new();
    for b in &batches {
        let utf8 = arrow::compute::cast(b.column(0), &arrow::datatypes::DataType::Utf8)
            .expect("a single-column text result must cast to Utf8");
        let arr = utf8
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .expect("cast result must be a Utf8 StringArray");
        for i in 0..arr.len() {
            if arr.is_valid(i) {
                out.push(arr.value(i).to_string());
            }
        }
    }
    Ok(out)
}

/// Collect a single-column `Int32Array` result into a `Vec<i32>`.
async fn int_column(
    ctx: &datafusion::execution::context::SessionContext,
    sql: &str,
) -> DFResult<Vec<i32>> {
    let batches = ctx.sql(sql).await?.collect().await?;
    let mut out = Vec::new();
    for b in &batches {
        let arr = b
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int32Array>()
            .expect("expected an Int32 column");
        for i in 0..arr.len() {
            if arr.is_valid(i) {
                out.push(arr.value(i));
            }
        }
    }
    Ok(out)
}

/// Build a [`DatabaseDef`] with an explicit OID for tests.
fn db(name: &str, oid: i32) -> DatabaseDef {
    LazyDatabaseRow::new(oid, name, 10)
}

/// A fully in-memory [`LazyCatalogSource`] - no database, engine, or connection
/// is involved, which proves the contract is backend-neutral.
///
/// It models two databases, each with one `public` schema holding one relation
/// with columns. OIDs are fixed and clear of the built-in range so cross-table
/// joins resolve and merges with built-ins are unambiguous.
struct FakeSource;

// OIDs used by the fake source; all well above the built-in ceiling (~13135).
const SCHEMA1_OID: i32 = 50100;
const SCHEMA2_OID: i32 = 50200;
const USERS_OID: i32 = 60100;
const USERS_TYPE_OID: i32 = 60101;
const EVENTS_OID: i32 = 60200;
const EVENTS_TYPE_OID: i32 = 60201;

impl LazyCatalogSource for FakeSource {
    fn databases(&self, callback: &mut dyn FnMut(Vec<DatabaseDef>)) -> DFResult<()> {
        callback(vec![db("lazydb1", 50001), db("lazydb2", 50002)]);
        Ok(())
    }

    fn schemas(&self, database: &str, callback: &mut dyn FnMut(Vec<SchemaDef>)) -> DFResult<()> {
        let schema = match database {
            "lazydb1" => SchemaDef::new(SCHEMA1_OID, "public"),
            "lazydb2" => SchemaDef::new(SCHEMA2_OID, "public"),
            _ => return Ok(()),
        };
        callback(vec![schema]);
        Ok(())
    }

    fn relations(
        &self,
        database: &str,
        schema: &str,
        callback: &mut dyn FnMut(Vec<RelationDef>),
    ) -> DFResult<()> {
        if schema != "public" {
            return Ok(());
        }
        let rel = match database {
            "lazydb1" => RelationDef::table(USERS_OID, USERS_TYPE_OID, "users"),
            "lazydb2" => RelationDef::table(EVENTS_OID, EVENTS_TYPE_OID, "events"),
            _ => return Ok(()),
        };
        callback(vec![rel]);
        Ok(())
    }

    fn columns(
        &self,
        _database: &str,
        _schema: &str,
        relation: &str,
        callback: &mut dyn FnMut(Vec<ColumnSpec>),
    ) -> DFResult<()> {
        let cols = match relation {
            // id int4 NOT NULL, name text NULL
            "users" => vec![
                ColumnSpec::new("id", 23, false),
                ColumnSpec::new("name", 25, true),
            ],
            // ts int8 NULL
            "events" => vec![ColumnSpec::new("ts", 20, true)],
            _ => return Ok(()),
        };
        callback(cols);
        Ok(())
    }
}

/// A source whose `databases()` always errors, used to prove error propagation.
struct FailingSource;

impl LazyCatalogSource for FailingSource {
    fn databases(&self, _callback: &mut dyn FnMut(Vec<DatabaseDef>)) -> DFResult<()> {
        Err(DataFusionError::Execution("boom from source".to_string()))
    }
    fn schemas(&self, _d: &str, _c: &mut dyn FnMut(Vec<SchemaDef>)) -> DFResult<()> {
        Ok(())
    }
    fn relations(&self, _d: &str, _s: &str, _c: &mut dyn FnMut(Vec<RelationDef>)) -> DFResult<()> {
        Ok(())
    }
    fn columns(
        &self,
        _d: &str,
        _s: &str,
        _r: &str,
        _c: &mut dyn FnMut(Vec<ColumnSpec>),
    ) -> DFResult<()> {
        Ok(())
    }
}

/// Build a base session serving `database` and install the fake source over all
/// catalog tables.
///
/// `FakeSource` reports two databases, and a context serves exactly one, so
/// every caller says which. `lazydb1` holds `users`, `lazydb2` holds `events`.
async fn ctx_with_fake_source(
    database: &str,
) -> DFResult<datafusion::execution::context::SessionContext> {
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;
    register_lazy_catalog(
        &ctx,
        Arc::new(FakeSource),
        LazyCatalogOptions::all(),
        database,
    )
    .await?;
    Ok(ctx)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_register_pg_database_on_scan() -> DFResult<()> {
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;

    // Before registering callback, databases should not exist.
    let df0 = ctx
        .sql("SELECT 1 FROM pg_catalog.pg_database WHERE datname IN ('lazy_db1','lazy_db2')")
        .await?;
    assert_eq!(df0.count().await?, 0);

    // Prepare a fetcher that records calls and returns two database names.
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = calls.clone();
    let fetcher = move || {
        calls_clone.fetch_add(1, Ordering::SeqCst);
        vec![
            LazyDatabaseRow::new(27001, "lazy_db1", 27735),
            LazyDatabaseRow::new(27002, "lazy_db2", 27735),
        ]
    };

    register_user_database_with_callback(&ctx, Arc::new(fetcher)).await?;

    // Now issue a query that scans pg_database; this should trigger the callback
    // and cause the databases to be registered just-in-time.
    let df = ctx
        .sql("SELECT datname FROM pg_catalog.pg_database WHERE datname IN ('lazy_db1','lazy_db2') ORDER BY datname")
        .await?;
    let batches = df.collect().await?;

    // Expect rows for both databases.
    let total_rows: usize = batches
        .iter()
        .map(arrow::array::RecordBatch::num_rows)
        .sum();
    assert_eq!(total_rows, 2);
    assert!(calls.load(Ordering::SeqCst) >= 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_merges_pg_database_rows() -> DFResult<()> {
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;

    // Precondition: static dataset contains the built-in databases.
    let pre_rows: usize = ctx
        .sql("SELECT datname FROM pg_catalog.pg_database")
        .await?
        .collect()
        .await?
        .iter()
        .map(arrow::array::RecordBatch::num_rows)
        .sum();
    assert!(
        pre_rows >= 3,
        "expected at least the static databases before registration"
    );

    // Register a callback that returns two custom databases.
    let fetcher = || {
        vec![
            LazyDatabaseRow::new(27003, "only_lazy_1", 27735),
            LazyDatabaseRow::new(27004, "only_lazy_2", 27735),
        ]
    };
    register_user_database_with_callback(&ctx, Arc::new(fetcher)).await?;

    // After registration, results MERGE built-ins with the callback rows.
    let names = string_column(
        &ctx,
        "SELECT datname FROM pg_catalog.pg_database ORDER BY datname",
    )
    .await?;

    // Built-in rows survive ...
    for builtin in ["postgres", "template0", "template1"] {
        assert!(
            names.contains(&builtin.to_string()),
            "built-in database {builtin} should still be present, got {names:?}"
        );
    }
    // ... alongside the callback rows.
    for lazy in ["only_lazy_1", "only_lazy_2"] {
        assert!(
            names.contains(&lazy.to_string()),
            "lazy database {lazy} should be present, got {names:?}"
        );
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_catalog_joins_resolve() -> DFResult<()> {
    let ctx = ctx_with_fake_source("lazydb1").await?;

    // pg_class JOIN pg_namespace JOIN pg_attribute for the user relation 'users'.
    let cols = string_column(
        &ctx,
        "SELECT a.attname FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
         WHERE c.relname = 'users' AND n.nspname = 'public' \
         ORDER BY a.attnum",
    )
    .await?;
    assert_eq!(cols, vec!["id".to_string(), "name".to_string()]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_catalog_builtins_survive() -> DFResult<()> {
    let ctx = ctx_with_fake_source("lazydb1").await?;

    // Built-in pg_type row for int4 (oid 23) survives the merge.
    let oids = int_column(
        &ctx,
        "SELECT oid FROM pg_catalog.pg_type WHERE typname = 'int4'",
    )
    .await?;
    assert!(oids.contains(&23), "expected int4 oid 23, got {oids:?}");

    // The catalog's self-describing pg_class row is still present.
    let self_rows = string_column(
        &ctx,
        "SELECT relname FROM pg_catalog.pg_class WHERE relname = 'pg_class'",
    )
    .await?;
    assert_eq!(self_rows, vec!["pg_class".to_string()]);

    // And the user relation is present alongside the built-ins.
    let users = string_column(
        &ctx,
        "SELECT relname FROM pg_catalog.pg_class WHERE relname = 'users'",
    )
    .await?;
    assert_eq!(users, vec!["users".to_string()]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_catalog_oid_passthrough() -> DFResult<()> {
    let ctx = ctx_with_fake_source("lazydb1").await?;

    // The oid the source returns for 'users' appears verbatim in pg_class.oid ...
    let class_oid = int_column(
        &ctx,
        "SELECT oid FROM pg_catalog.pg_class WHERE relname = 'users'",
    )
    .await?;
    assert_eq!(class_oid, vec![USERS_OID]);

    // ... and is used verbatim as pg_attribute.attrelid.
    let attrelids = int_column(
        &ctx,
        "SELECT DISTINCT attrelid FROM pg_catalog.pg_attribute \
         WHERE attrelid = 60100",
    )
    .await?;
    assert_eq!(attrelids, vec![USERS_OID]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_catalog_information_schema_columns() -> DFResult<()> {
    let ctx = ctx_with_fake_source("lazydb1").await?;

    let batches = ctx
        .sql(
            "SELECT column_name, ordinal_position, data_type, is_nullable \
             FROM information_schema.columns \
             WHERE table_name = 'users' AND table_schema = 'public' \
             ORDER BY ordinal_position",
        )
        .await?
        .collect()
        .await?;

    assert_eq!(
        batches
            .iter()
            .map(arrow::array::RecordBatch::num_rows)
            .sum::<usize>(),
        2
    );
    let b = &batches[0];
    let name = b
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::StringViewArray>()
        .unwrap();
    let pos = b
        .column(1)
        .as_any()
        .downcast_ref::<arrow::array::Int32Array>()
        .unwrap();
    let dt = b
        .column(2)
        .as_any()
        .downcast_ref::<arrow::array::StringViewArray>()
        .unwrap();
    let nullable = b
        .column(3)
        .as_any()
        .downcast_ref::<arrow::array::StringViewArray>()
        .unwrap();

    assert_eq!(name.value(0), "id");
    assert_eq!(pos.value(0), 1);
    assert_eq!(dt.value(0), "integer");
    assert_eq!(nullable.value(0), "NO");

    assert_eq!(name.value(1), "name");
    assert_eq!(pos.value(1), 2);
    assert_eq!(dt.value(1), "text");
    assert_eq!(nullable.value(1), "YES");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_catalog_projection_and_filter() -> DFResult<()> {
    let ctx = ctx_with_fake_source("lazydb1").await?;

    // Filter pushes a relname predicate; projection selects a single column.
    // The IN list names both databases' relations, and only this database's
    // comes back: `events` belongs to lazydb2, which this context does not serve.
    let names = string_column(
        &ctx,
        "SELECT relname FROM pg_catalog.pg_class \
         WHERE relname IN ('users','events') ORDER BY relname",
    )
    .await?;
    assert_eq!(names, vec!["users".to_string()]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_catalog_error_propagates() -> DFResult<()> {
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;
    // Registered as a database-independent source so registration itself does
    // not consult databases(): this test is about a source that fails at SCAN
    // time, which is the case a client is exposed to.
    // test_lazy_catalog_error_surfaces_at_registration covers the other one.
    register_database_independent_lazy_catalog(
        &ctx,
        Arc::new(FailingSource),
        LazyCatalogOptions::with_tables(vec![CatalogTable::PgDatabase]),
    )
    .await?;

    // Scanning pg_database must surface the source error, not silently return rows.
    let result = ctx
        .sql("SELECT datname FROM pg_catalog.pg_database")
        .await?
        .collect()
        .await;
    assert!(
        result.is_err(),
        "expected the source error to propagate to the client"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("boom from source"),
        "expected the source error message, got: {msg}"
    );
    Ok(())
}

/// Count the rows returned by `sql` (expects a single Int64 `count(*)` column).
async fn count_rows(
    ctx: &datafusion::execution::context::SessionContext,
    sql: &str,
) -> DFResult<i64> {
    let batches = ctx.sql(sql).await?.collect().await?;
    let arr = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::Int64Array>()
        .expect("expected an Int64 count column");
    Ok(arr.value(0))
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_tables_view_reflects_lazy_tables() -> DFResult<()> {
    // The `pg_tables` VIEW is `SELECT ... FROM pg_class ... WHERE relkind IN ('r','p')`.
    // Registering the lazy source BEFORE the views are created binds the view's
    // plan to the lazy pg_class, so the source's relations show up through it.
    let (ctx, _log) = get_base_session_context_with_lazy_catalog(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
        Arc::new(FakeSource),
        LazyCatalogOptions::all(),
        "lazydb1".to_string(),
    )
    .await?;

    // Only this context's database contributes: `events` lives in lazydb2 and
    // stays invisible from lazydb1, through the view as well as the base table.
    let names = string_column(
        &ctx,
        "SELECT tablename FROM pg_catalog.pg_tables WHERE tablename IN ('users','events') ORDER BY tablename",
    )
    .await?;
    assert_eq!(names, vec!["users".to_string()]);

    // The owning schema is resolved through the join to pg_namespace.
    let schemas = string_column(
        &ctx,
        "SELECT DISTINCT schemaname FROM pg_catalog.pg_tables WHERE tablename = 'users'",
    )
    .await?;
    assert_eq!(schemas, vec!["public".to_string()]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_tables_view_keeps_builtin_tables() -> DFResult<()> {
    // Merging with built-ins must hold through the view too: a built-in ordinary
    // table (pg_class itself, relkind 'r') is still listed alongside user tables.
    let (ctx, _log) = get_base_session_context_with_lazy_catalog(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
        Arc::new(FakeSource),
        LazyCatalogOptions::all(),
        "lazydb1".to_string(),
    )
    .await?;

    let builtin = count_rows(
        &ctx,
        "SELECT count(*) FROM pg_catalog.pg_tables WHERE tablename = 'pg_class'",
    )
    .await?;
    assert_eq!(
        builtin, 1,
        "built-in pg_class should be listed in pg_tables"
    );

    let user = count_rows(
        &ctx,
        "SELECT count(*) FROM pg_catalog.pg_tables WHERE tablename = 'users'",
    )
    .await?;
    assert_eq!(user, 1, "lazy user table should be listed in pg_tables");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_registered_after_session_rebinds_views() -> DFResult<()> {
    // A view (pg_tables) is planned during session construction and binds to
    // whatever pg_class provider exists THEN. register_lazy_catalog swaps the base
    // table provider for the lazy one and then re-plans the registered views, so a
    // view registered after session construction still reflects the lazy tables.
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;
    register_lazy_catalog(
        &ctx,
        Arc::new(FakeSource),
        LazyCatalogOptions::all(),
        "lazydb1",
    )
    .await?;

    // The base table reflects the lazy rows ...
    let base = count_rows(
        &ctx,
        "SELECT count(*) FROM pg_catalog.pg_class WHERE relname = 'users'",
    )
    .await?;
    assert_eq!(base, 1, "base pg_class should see the lazy table");

    // ... and so does the view, because register_lazy_catalog re-planned it
    // against the lazy provider.
    let via_view = count_rows(
        &ctx,
        "SELECT count(*) FROM pg_catalog.pg_tables WHERE tablename = 'users'",
    )
    .await?;
    assert_eq!(
        via_view, 1,
        "view must see the lazy table after register_lazy_catalog re-plans it"
    );
    Ok(())
}

/// A source defining two relations with the SAME name in the SAME schema, used
/// to prove duplicate user objects are rejected rather than silently merged.
struct DuplicateRelationSource;

impl LazyCatalogSource for DuplicateRelationSource {
    fn databases(&self, callback: &mut dyn FnMut(Vec<DatabaseDef>)) -> DFResult<()> {
        callback(vec![db("dupdb", 70001)]);
        Ok(())
    }
    fn schemas(&self, database: &str, callback: &mut dyn FnMut(Vec<SchemaDef>)) -> DFResult<()> {
        if database == "dupdb" {
            callback(vec![SchemaDef::new(70100, "public")]);
        }
        Ok(())
    }
    fn relations(
        &self,
        database: &str,
        schema: &str,
        callback: &mut dyn FnMut(Vec<RelationDef>),
    ) -> DFResult<()> {
        if database == "dupdb" && schema == "public" {
            callback(vec![
                RelationDef::table(70200, 70201, "duptbl"),
                RelationDef::table(70300, 70301, "duptbl"),
            ]);
        }
        Ok(())
    }
    fn columns(
        &self,
        _d: &str,
        _s: &str,
        _r: &str,
        _c: &mut dyn FnMut(Vec<ColumnSpec>),
    ) -> DFResult<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_catalog_double_registration_is_idempotent() -> DFResult<()> {
    // Registering the same source twice must NOT duplicate rows: the second
    // registration captures the first provider's merged output as its "builtin",
    // but the per-scan dedup drops the user rows baked into it, so the result is
    // identical to a single registration.
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;
    register_lazy_catalog(
        &ctx,
        Arc::new(FakeSource),
        LazyCatalogOptions::all(),
        "lazydb1",
    )
    .await?;
    register_lazy_catalog(
        &ctx,
        Arc::new(FakeSource),
        LazyCatalogOptions::all(),
        "lazydb1",
    )
    .await?;

    // The user relation appears exactly once ...
    let users = count_rows(
        &ctx,
        "SELECT count(*) FROM pg_catalog.pg_class WHERE relname = 'users'",
    )
    .await?;
    assert_eq!(users, 1, "double registration must not duplicate user rows");

    // ... the user database exactly once ...
    let lazydb = count_rows(
        &ctx,
        "SELECT count(*) FROM pg_catalog.pg_database WHERE datname = 'lazydb1'",
    )
    .await?;
    assert_eq!(
        lazydb, 1,
        "double registration must not duplicate databases"
    );

    // ... and a built-in survives exactly once (not dropped, not duplicated).
    let int4 = count_rows(
        &ctx,
        "SELECT count(*) FROM pg_catalog.pg_type WHERE typname = 'int4'",
    )
    .await?;
    assert_eq!(
        int4, 1,
        "built-in row must survive double registration once"
    );
    Ok(())
}

/// A source with one indexed table, to prove the relation flags reach `pg_tables`.
struct IndexedSource;

impl LazyCatalogSource for IndexedSource {
    fn databases(&self, callback: &mut dyn FnMut(Vec<DatabaseDef>)) -> DFResult<()> {
        callback(vec![db("idxdb", 80001)]);
        Ok(())
    }
    fn schemas(&self, database: &str, callback: &mut dyn FnMut(Vec<SchemaDef>)) -> DFResult<()> {
        if database == "idxdb" {
            callback(vec![SchemaDef::new(80100, "public")]);
        }
        Ok(())
    }
    fn relations(
        &self,
        database: &str,
        schema: &str,
        callback: &mut dyn FnMut(Vec<RelationDef>),
    ) -> DFResult<()> {
        if database == "idxdb" && schema == "public" {
            callback(vec![RelationDef {
                oid: 80200,
                reltype_oid: 80201,
                name: "indexed".to_string(),
                kind: RelationKind::Table,
                owner_oid: Some(80010),
                has_index: true,
                has_rules: false,
                has_triggers: false,
                row_security: false,
            }]);
        }
        Ok(())
    }
    fn columns(
        &self,
        _d: &str,
        _s: &str,
        _r: &str,
        callback: &mut dyn FnMut(Vec<ColumnSpec>),
    ) -> DFResult<()> {
        // 'id' (no default) and 'note' (has a default expression).
        callback(vec![
            ColumnSpec::new("id", 23, false),
            ColumnSpec::new("note", 25, true).with_default(),
        ]);
        Ok(())
    }

    fn indexes(
        &self,
        database: &str,
        schema: &str,
        callback: &mut dyn FnMut(Vec<IndexDef>),
    ) -> DFResult<()> {
        if database == "idxdb" && schema == "public" {
            // A unique primary-key index on column 1 ('id') of the 'indexed' table.
            let mut pkey = IndexDef::new(80300, "indexed_pkey", 80200, vec![1]);
            pkey.is_unique = true;
            pkey.is_primary = true;
            // A non-unique secondary index spanning columns 1 ('id') and 2
            // ('note'), so pg_get_indexdef must list multiple key columns in order.
            let spanning = IndexDef::new(80301, "indexed_id_note_idx", 80200, vec![1, 2]);
            // A functional/expression index (key column 0 = an expression), which
            // the structural render cannot describe; its text comes from the
            // installed index-definition resolver.
            let expression = IndexDef::new(80302, "indexed_lower_note_idx", 80200, vec![0]);
            callback(vec![pkey, spanning, expression]);
        }
        Ok(())
    }

    fn constraints(
        &self,
        database: &str,
        schema: &str,
        callback: &mut dyn FnMut(Vec<ConstraintDef>),
    ) -> DFResult<()> {
        if database == "idxdb" && schema == "public" {
            // A primary key on column 1 ('id') of 'indexed', backed by the
            // indexed_pkey unique index (oid 80300). Schema oid is 80100.
            callback(vec![ConstraintDef::primary_key(
                80400,
                "indexed_pkey",
                80100,
                80200,
                vec![1],
                80300,
            )]);
        }
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_pg_index_reflects_source() -> DFResult<()> {
    // An index reported by the source becomes both a pg_index structure row and an
    // index relation in pg_class (relkind 'i'), the two rows pg_indexes /
    // pg_get_indexdef join to describe it.
    let (ctx, _log) = get_base_session_context_with_lazy_catalog(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
        Arc::new(IndexedSource),
        LazyCatalogOptions::all(),
        "idxdb".to_string(),
    )
    .await?;

    // The index has its own pg_class row, relkind 'i', carrying its name.
    let index_rel = string_column(
        &ctx,
        "SELECT relname FROM pg_catalog.pg_class \
         WHERE relname = 'indexed_pkey' AND relkind = 'i'",
    )
    .await?;
    assert_eq!(index_rel, vec!["indexed_pkey".to_string()]);

    // The pg_index structure row points at the table and is unique + primary.
    let indrelid = int_column(
        &ctx,
        "SELECT indrelid FROM pg_catalog.pg_index \
         WHERE indexrelid = 80300 AND indisunique AND indisprimary",
    )
    .await?;
    assert_eq!(
        indrelid,
        vec![80200],
        "pg_index must point at the table oid"
    );

    // Joining pg_index -> the index's pg_class name resolves the table's indexes
    // (the primary key and the spanning secondary index).
    let by_table = string_column(
        &ctx,
        "SELECT i.relname FROM pg_catalog.pg_index x \
         JOIN pg_catalog.pg_class i ON i.oid = x.indexrelid \
         JOIN pg_catalog.pg_class t ON t.oid = x.indrelid \
         WHERE t.relname = 'indexed' ORDER BY i.relname",
    )
    .await?;
    assert_eq!(
        by_table,
        vec![
            "indexed_id_note_idx".to_string(),
            "indexed_lower_note_idx".to_string(),
            "indexed_pkey".to_string()
        ]
    );

    // indkey lists the single indexed column's attnum (1 = 'id').
    let indkey = int_column(
        &ctx,
        "SELECT unnest(indkey) AS k FROM pg_catalog.pg_index WHERE indexrelid = 80300",
    )
    .await?;
    assert_eq!(indkey, vec![1], "indkey must list the indexed attnum");

    // pg_get_indexdef templates the CREATE INDEX text from those structural rows
    // (unique flag, btree access method, schema-qualified table, key column name).
    let indexdef = string_column(&ctx, "SELECT pg_catalog.pg_get_indexdef(80300)").await?;
    assert_eq!(
        indexdef,
        vec!["CREATE UNIQUE INDEX indexed_pkey ON public.indexed USING btree (id)".to_string()],
        "pg_get_indexdef must reconstruct the CREATE INDEX text for a registered index"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_get_indexdef_multicolumn_and_unresolvable_oids() -> DFResult<()> {
    // pg_get_indexdef must list a multi-column index's key columns in indkey
    // order, and must yield NULL (not an error or empty text) when the argument
    // is NULL or names an index oid that no row describes.
    let (ctx, _log) = get_base_session_context_with_lazy_catalog(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
        Arc::new(IndexedSource),
        LazyCatalogOptions::all(),
        "idxdb".to_string(),
    )
    .await?;

    // The spanning index (oid 80301) covers columns 1 ('id') and 2 ('note'); both
    // appear, in order, inside the single CREATE INDEX statement.
    let spanning = string_column(&ctx, "SELECT pg_catalog.pg_get_indexdef(80301)").await?;
    assert_eq!(
        spanning,
        vec![
            "CREATE INDEX indexed_id_note_idx ON public.indexed USING btree (id, note)".to_string()
        ],
        "pg_get_indexdef must list every key column of a multi-column index in order"
    );

    // A NULL argument resolves to NULL.
    let null_arg = int_column(
        &ctx,
        "SELECT (pg_catalog.pg_get_indexdef(CAST(NULL AS BIGINT)) IS NULL)::int",
    )
    .await?;
    assert_eq!(null_arg, vec![1], "pg_get_indexdef(NULL) must be NULL");

    // An oid that describes no index resolves to NULL, not an empty or partial
    // statement.
    let unknown_oid = int_column(
        &ctx,
        "SELECT (pg_catalog.pg_get_indexdef(999999) IS NULL)::int",
    )
    .await?;
    assert_eq!(
        unknown_oid,
        vec![1],
        "pg_get_indexdef of an unknown oid must be NULL"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_pg_constraint_reflects_source() -> DFResult<()> {
    // A constraint reported by the source becomes a pg_constraint row, and the
    // (now live) information_schema constraint views derive from it.
    let (ctx, _log) = get_base_session_context_with_lazy_catalog(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
        Arc::new(IndexedSource),
        LazyCatalogOptions::all(),
        "idxdb".to_string(),
    )
    .await?;

    // The pg_constraint structure row is a primary key over the 'indexed' table.
    let contype = text_column(
        &ctx,
        "SELECT contype FROM pg_catalog.pg_constraint \
         WHERE conname = 'indexed_pkey' AND conrelid = 80200",
    )
    .await?;
    assert_eq!(
        contype,
        vec!["p".to_string()],
        "pg_constraint must hold the PK"
    );

    // The live table_constraints view reflects the registered constraint.
    let constraint_types = text_column(
        &ctx,
        "SELECT constraint_type FROM information_schema.table_constraints \
         WHERE table_name = 'indexed' AND constraint_name = 'indexed_pkey'",
    )
    .await?;
    assert_eq!(
        constraint_types,
        vec!["PRIMARY KEY".to_string()],
        "table_constraints must show the registered primary key"
    );

    // key_column_usage maps the constraint to its column ('id').
    let key_columns = text_column(
        &ctx,
        "SELECT column_name FROM information_schema.key_column_usage \
         WHERE constraint_name = 'indexed_pkey' AND table_name = 'indexed'",
    )
    .await?;
    assert_eq!(
        key_columns,
        vec!["id".to_string()],
        "key_column_usage must map the PK to column 'id'"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_pg_attrdef_reflects_source() -> DFResult<()> {
    // A column flagged with a default becomes atthasdef on pg_attribute plus a
    // backing pg_attrdef row, while a column without one gets neither.
    let (ctx, _log) = get_base_session_context_with_lazy_catalog(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
        Arc::new(IndexedSource),
        LazyCatalogOptions::all(),
        "idxdb".to_string(),
    )
    .await?;

    // 'note' (attnum 2) has a default: atthasdef true and a pg_attrdef row.
    let with_default = ctx
        .sql(
            "SELECT 1 FROM pg_catalog.pg_attribute \
             WHERE attrelid = 80200 AND attname = 'note' AND atthasdef = true",
        )
        .await?
        .count()
        .await?;
    assert_eq!(with_default, 1, "the defaulted column must set atthasdef");

    let attrdef_for_note = ctx
        .sql("SELECT 1 FROM pg_catalog.pg_attrdef WHERE adrelid = 80200 AND adnum = 2")
        .await?
        .count()
        .await?;
    assert_eq!(
        attrdef_for_note, 1,
        "the defaulted column must get a pg_attrdef row"
    );

    // 'id' (attnum 1) has no default, so no pg_attrdef row - the table has exactly
    // the one pg_attrdef row, for 'note'.
    let attrdef_total = ctx
        .sql("SELECT 1 FROM pg_catalog.pg_attrdef WHERE adrelid = 80200")
        .await?
        .count()
        .await?;
    assert_eq!(
        attrdef_total, 1,
        "only the defaulted column gets a pg_attrdef row"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_pg_tables_flags_reflect_source() -> DFResult<()> {
    // has_index from the source surfaces as pg_tables.hasindexes; the other flags
    // are non-NULL false (not blank, as they were before).
    let (ctx, _log) = get_base_session_context_with_lazy_catalog(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
        Arc::new(IndexedSource),
        LazyCatalogOptions::all(),
        "idxdb".to_string(),
    )
    .await?;

    let indexed = string_column(
        &ctx,
        "SELECT tablename FROM pg_catalog.pg_tables \
         WHERE tablename = 'indexed' AND hasindexes AND NOT hastriggers AND NOT rowsecurity",
    )
    .await?;
    assert_eq!(indexed, vec!["indexed".to_string()]);

    // The source-supplied owner OID is written through to pg_class.relowner.
    let owner = int_column(
        &ctx,
        "SELECT relowner FROM pg_catalog.pg_class WHERE relname = 'indexed'",
    )
    .await?;
    assert_eq!(owner, vec![80010]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_catalog_owner_omitted_is_null() -> DFResult<()> {
    // FakeSource builds relations via RelationDef::table (no owner), so a backend
    // without ownership leaves pg_class.relowner NULL (int_column skips NULLs).
    let ctx = ctx_with_fake_source("lazydb1").await?;
    let owner = int_column(
        &ctx,
        "SELECT relowner FROM pg_catalog.pg_class WHERE relname = 'users'",
    )
    .await?;
    assert!(
        owner.is_empty(),
        "relowner must be NULL when the source omits the owner, got {owner:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_catalog_user_database_wins_over_builtin() -> DFResult<()> {
    // A user database whose name collides with a built-in ('postgres') must
    // REPLACE the built-in row, not duplicate it: exactly one row, the user's.
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;

    let fetcher = || vec![LazyDatabaseRow::new(91827, "postgres", 10)];
    register_user_database_with_callback(&ctx, Arc::new(fetcher)).await?;

    let oids = int_column(
        &ctx,
        "SELECT oid FROM pg_catalog.pg_database WHERE datname = 'postgres'",
    )
    .await?;
    assert_eq!(
        oids,
        vec![91827],
        "user-supplied 'postgres' must win over the built-in row, with no duplicate"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_catalog_duplicate_database_errors() -> DFResult<()> {
    // Two user databases with the same name is a source mistake -> error.
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;

    let fetcher = || {
        vec![
            LazyDatabaseRow::new(90001, "dup", 10),
            LazyDatabaseRow::new(90002, "dup", 10),
        ]
    };
    register_user_database_with_callback(&ctx, Arc::new(fetcher)).await?;

    let result = ctx
        .sql("SELECT datname FROM pg_catalog.pg_database")
        .await?
        .collect()
        .await;
    assert!(result.is_err(), "two databases named 'dup' must error");
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("duplicate") && msg.contains("pg_database"),
        "expected a duplicate-pg_database error, got: {msg}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_catalog_duplicate_relation_errors() -> DFResult<()> {
    // Two user relations of the same name in the same schema -> error.
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;
    register_lazy_catalog(
        &ctx,
        Arc::new(DuplicateRelationSource),
        LazyCatalogOptions::all(),
        "dupdb",
    )
    .await?;

    let result = ctx
        .sql("SELECT relname FROM pg_catalog.pg_class")
        .await?
        .collect()
        .await;
    assert!(result.is_err(), "two relations named 'duptbl' must error");
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("duplicate") && msg.contains("pg_class"),
        "expected a duplicate-pg_class error, got: {msg}"
    );
    Ok(())
}

/// A source that contributes only `pg_config` settings (nothing else), used to
/// prove the `config()` callback overrides and extends the built-in defaults.
struct ConfigSource;

impl LazyCatalogSource for ConfigSource {
    fn databases(&self, _callback: &mut dyn FnMut(Vec<DatabaseDef>)) -> DFResult<()> {
        Ok(())
    }
    fn schemas(&self, _database: &str, _callback: &mut dyn FnMut(Vec<SchemaDef>)) -> DFResult<()> {
        Ok(())
    }
    fn relations(
        &self,
        _database: &str,
        _schema: &str,
        _callback: &mut dyn FnMut(Vec<RelationDef>),
    ) -> DFResult<()> {
        Ok(())
    }
    fn columns(
        &self,
        _database: &str,
        _schema: &str,
        _relation: &str,
        _callback: &mut dyn FnMut(Vec<ColumnSpec>),
    ) -> DFResult<()> {
        Ok(())
    }
    fn config(&self, callback: &mut dyn FnMut(Vec<ConfigSettingDef>)) -> DFResult<()> {
        callback(vec![
            // Replaces the built-in VERSION (same name = override).
            ConfigSettingDef {
                name: "VERSION".into(),
                setting: "riffq 1.0".into(),
            },
            // A brand-new setting not present in the built-ins.
            ConfigSettingDef {
                name: "EMBEDDER".into(),
                setting: "riffq".into(),
            },
        ]);
        Ok(())
    }
    fn settings(&self, callback: &mut dyn FnMut(Vec<SettingDef>)) -> DFResult<()> {
        callback(vec![
            // Override a session-mutable parameter's live value.
            SettingDef {
                name: "search_path".into(),
                setting: "tenant42, public".into(),
            },
        ]);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_config_callback_overrides_and_extends() -> DFResult<()> {
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;
    register_database_independent_lazy_catalog(
        &ctx,
        Arc::new(ConfigSource),
        LazyCatalogOptions::with_tables(vec![CatalogTable::PgConfig, CatalogTable::PgSettings]),
    )
    .await?;

    // The override replaces the built-in VERSION row (not duplicated).
    let version = string_column(
        &ctx,
        "SELECT setting FROM pg_catalog.pg_config WHERE name = 'VERSION'",
    )
    .await?;
    assert_eq!(
        version,
        vec!["riffq 1.0".to_string()],
        "VERSION overridden once"
    );

    // The new setting is added.
    let embedder = string_column(
        &ctx,
        "SELECT setting FROM pg_catalog.pg_config WHERE name = 'EMBEDDER'",
    )
    .await?;
    assert_eq!(embedder, vec!["riffq".to_string()]);

    // A built-in the callback didn't touch (BINDIR) is still present.
    let bindir = string_column(
        &ctx,
        "SELECT setting FROM pg_catalog.pg_config WHERE name = 'BINDIR'",
    )
    .await?;
    assert_eq!(bindir.len(), 1, "untouched built-in BINDIR preserved");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_settings_callback_overrides_value() -> DFResult<()> {
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;
    register_database_independent_lazy_catalog(
        &ctx,
        Arc::new(ConfigSource),
        LazyCatalogOptions::with_tables(vec![CatalogTable::PgConfig, CatalogTable::PgSettings]),
    )
    .await?;

    // The callback's live value for search_path replaces the snapshot row.
    let search_path = string_column(
        &ctx,
        "SELECT setting FROM pg_catalog.pg_settings WHERE name = 'search_path'",
    )
    .await?;
    assert_eq!(search_path, vec!["tenant42, public".to_string()]);

    // A parameter the callback didn't supply keeps its built-in snapshot value.
    let max_conn = string_column(
        &ctx,
        "SELECT setting FROM pg_catalog.pg_settings WHERE name = 'max_connections'",
    )
    .await?;
    assert_eq!(
        max_conn.len(),
        1,
        "untouched built-in max_connections preserved"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_settings_defaults_without_callback() -> DFResult<()> {
    // With no lazy source, pg_settings serves its built-in snapshot as a table.
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;
    let rows = ctx
        .sql("SELECT count(*) FROM pg_catalog.pg_settings")
        .await?
        .collect()
        .await?;
    let n = rows[0]
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::Int64Array>()
        .unwrap()
        .value(0);
    assert!(n > 300, "expected the full settings snapshot, got {n}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_config_defaults_without_callback() -> DFResult<()> {
    // With no lazy source, pg_config serves its built-in defaults as a table.
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;
    let version = string_column(
        &ctx,
        "SELECT setting FROM pg_catalog.pg_config WHERE name = 'VERSION'",
    )
    .await?;
    assert_eq!(version, vec!["PostgreSQL 17.4".to_string()]);
    Ok(())
}

/// A lazy source exposing one view, `active_users` (oid 80700), in `viewdb.public`.
/// It carries no definition text itself - the text is supplied separately by a
/// [`ViewDefinitionResolver`], mirroring the Phase 3 "integration supplies
/// definitions" contract.
struct ViewSource;

impl LazyCatalogSource for ViewSource {
    fn databases(&self, callback: &mut dyn FnMut(Vec<DatabaseDef>)) -> DFResult<()> {
        callback(vec![db("viewdb", 80501)]);
        Ok(())
    }
    fn schemas(&self, database: &str, callback: &mut dyn FnMut(Vec<SchemaDef>)) -> DFResult<()> {
        if database == "viewdb" {
            callback(vec![SchemaDef::new(80600, "public")]);
        }
        Ok(())
    }
    fn relations(
        &self,
        database: &str,
        schema: &str,
        callback: &mut dyn FnMut(Vec<RelationDef>),
    ) -> DFResult<()> {
        if database == "viewdb" && schema == "public" {
            callback(vec![RelationDef {
                oid: 80700,
                reltype_oid: 80701,
                name: "active_users".to_string(),
                kind: RelationKind::View,
                owner_oid: Some(80010),
                has_index: false,
                has_rules: false,
                has_triggers: false,
                row_security: false,
            }]);
        }
        Ok(())
    }
    fn columns(
        &self,
        _d: &str,
        _s: &str,
        _r: &str,
        callback: &mut dyn FnMut(Vec<ColumnSpec>),
    ) -> DFResult<()> {
        callback(vec![ColumnSpec::new("id", 23, false)]);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_get_viewdef_uses_registered_resolver() -> DFResult<()> {
    // A registered view becomes a relkind 'v' pg_class row; pg_get_viewdef returns
    // whatever the integration-supplied resolver produces for that view, and NULL
    // when the resolver declines or the oid names no view.
    let (ctx, _log) = get_base_session_context_with_lazy_catalog(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
        Arc::new(ViewSource),
        LazyCatalogOptions::all(),
        "viewdb".to_string(),
    )
    .await?;

    // The integration supplies definition text keyed on the view's identity. The
    // resolver is process-wide, so clear it at the end to avoid leaking into other
    // tests sharing this binary.
    let resolver: ViewDefinitionResolver = Arc::new(|view: &ViewIdentity| {
        if view.schema == "public" && view.name == "active_users" {
            Some("SELECT id FROM users WHERE active".to_string())
        } else {
            None
        }
    });
    set_view_definition_resolver(resolver);

    let outcome = async {
        // pg_get_viewdef of the view's oid returns the supplied text.
        let definition = string_column(&ctx, "SELECT pg_catalog.pg_get_viewdef(80700)").await?;
        assert_eq!(
            definition,
            vec!["SELECT id FROM users WHERE active".to_string()],
            "pg_get_viewdef must return the resolver-supplied text"
        );

        // An oid that names no view resolves to NULL.
        let unknown = int_column(
            &ctx,
            "SELECT (pg_catalog.pg_get_viewdef(999999) IS NULL)::int",
        )
        .await?;
        assert_eq!(
            unknown,
            vec![1],
            "pg_get_viewdef of an unknown oid must be NULL"
        );

        // pg_views.definition, which calls pg_get_viewdef(c.oid), reflects the same
        // supplied text for the live view row.
        let via_pg_views = string_column(
            &ctx,
            "SELECT definition FROM pg_catalog.pg_views WHERE viewname = 'active_users'",
        )
        .await?;
        assert_eq!(
            via_pg_views,
            vec!["SELECT id FROM users WHERE active".to_string()],
            "pg_views.definition must reflect the resolver text for the registered view"
        );
        Ok::<(), DataFusionError>(())
    }
    .await;

    clear_view_definition_resolver();
    outcome
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_get_indexdef_uses_index_resolver_for_expression_index() -> DFResult<()> {
    // A functional/expression index cannot be rendered structurally; its
    // CREATE INDEX text comes from the installed index-definition resolver, while
    // plain indexes keep rendering structurally and ignore the resolver.
    let (ctx, _log) = get_base_session_context_with_lazy_catalog(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
        Arc::new(IndexedSource),
        LazyCatalogOptions::all(),
        "idxdb".to_string(),
    )
    .await?;

    // With no resolver installed, the expression index (oid 80302) is NULL.
    let before = int_column(
        &ctx,
        "SELECT (pg_catalog.pg_get_indexdef(80302) IS NULL)::int",
    )
    .await?;
    assert_eq!(
        before,
        vec![1],
        "expression index must be NULL with no resolver installed"
    );

    // The resolver is process-wide, so clear it at the end to avoid leaking into
    // other tests sharing this binary.
    let resolver: IndexDefinitionResolver = Arc::new(|index: &IndexIdentity| {
        if index.schema == "public" && index.name == "indexed_lower_note_idx" {
            Some(
                "CREATE INDEX indexed_lower_note_idx ON public.indexed USING btree (lower(note))"
                    .to_string(),
            )
        } else {
            None
        }
    });
    set_index_definition_resolver(resolver);

    let outcome = async {
        // The expression index now gets the resolver-supplied text.
        let expr = string_column(&ctx, "SELECT pg_catalog.pg_get_indexdef(80302)").await?;
        assert_eq!(
            expr,
            vec![
                "CREATE INDEX indexed_lower_note_idx ON public.indexed USING btree (lower(note))"
                    .to_string()
            ],
            "expression index must use the resolver-supplied text"
        );

        // A plain index still renders structurally - the resolver is not consulted
        // for indexes pg_catalog can describe from the catalog alone.
        let plain = string_column(&ctx, "SELECT pg_catalog.pg_get_indexdef(80300)").await?;
        assert_eq!(
            plain,
            vec!["CREATE UNIQUE INDEX indexed_pkey ON public.indexed USING btree (id)".to_string()],
            "plain index must keep its structural render"
        );
        Ok::<(), DataFusionError>(())
    }
    .await;

    clear_index_definition_resolver();
    outcome
}

#[tokio::test(flavor = "multi_thread")]
async fn test_source_public_schema_replaces_the_builtin_one() -> DFResult<()> {
    // The built-in catalog carries `public` at PostgreSQL's canonical oid 2200.
    // A source-supplied `public` gets a generated oid instead. Those oids never
    // match, so shadowing built-ins by oid left both rows in place: the built-in
    // one owning nothing while holding the oid clients treat as canonical, which
    // hid every table from anything resolving public to 2200. Shadowing by name
    // is what keeps exactly one row here.
    let (ctx, _log) = get_base_session_context_with_lazy_catalog(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
        Arc::new(FakeSource),
        LazyCatalogOptions::all(),
        "lazydb1".to_string(),
    )
    .await?;

    let oids = text_column(
        &ctx,
        "SELECT oid FROM pg_catalog.pg_namespace WHERE nspname = 'public' ORDER BY oid",
    )
    .await?;

    // FakeSource models two databases, each with its own public schema, but this
    // context serves lazydb1, so only lazydb1's public appears -- and it is not
    // the built-in 2200.
    assert_eq!(
        oids,
        vec![SCHEMA1_OID.to_string()],
        "expected only the served database's public schema, got {oids:?}"
    );
    assert!(
        !oids.contains(&"2200".to_string()),
        "the built-in public@2200 must be shadowed, got {oids:?}"
    );
    Ok(())
}

/// Build a context serving `database` from `FakeSource`, with the source
/// installed before the views so they bind to it.
async fn ctx_serving_database_from_fake_source(
    database: &str,
) -> DFResult<datafusion::execution::context::SessionContext> {
    // The DataFusion catalog is named after the database, which is what the view
    // bodies inline current_database() to.
    let (ctx, _log) = get_base_session_context_with_lazy_catalog(
        Some("pg_catalog_data/pg_schema"),
        database.to_string(),
        "public".to_string(),
        Arc::new(FakeSource),
        LazyCatalogOptions::all(),
        database.to_string(),
    )
    .await?;
    Ok(ctx)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_each_database_gets_its_own_context_and_sees_only_its_own_objects() -> DFResult<()> {
    // The whole point of one context per database. FakeSource reports two
    // databases that each own a `public` schema holding one relation; a
    // connection to either must see its own and nothing of the other's, which is
    // what a PostgreSQL connection sees.
    let db1 = ctx_serving_database_from_fake_source("lazydb1").await?;
    let db2 = ctx_serving_database_from_fake_source("lazydb2").await?;

    // Exactly one `public` per context, each carrying its own database's oid.
    for (ctx, expected_oid, label) in [
        (&db1, SCHEMA1_OID, "lazydb1"),
        (&db2, SCHEMA2_OID, "lazydb2"),
    ] {
        let oids = text_column(
            ctx,
            "SELECT oid FROM pg_catalog.pg_namespace WHERE nspname = 'public' ORDER BY oid",
        )
        .await?;
        assert_eq!(
            oids,
            vec![expected_oid.to_string()],
            "{label} must see exactly its own public schema, got {oids:?}"
        );
    }

    // And only its own relation, through the join that resolves the namespace.
    let relations_sql = "SELECT c.relname FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' AND c.relkind = 'r' ORDER BY c.relname";
    assert_eq!(
        string_column(&db1, relations_sql).await?,
        vec!["users".to_string()],
        "lazydb1 must not see lazydb2's events"
    );
    assert_eq!(
        string_column(&db2, relations_sql).await?,
        vec!["events".to_string()],
        "lazydb2 must not see lazydb1's users"
    );

    // pg_database is the exception: every database is listed from either one,
    // which is how PostgreSQL answers "\l".
    let databases_sql =
        "SELECT datname FROM pg_catalog.pg_database WHERE datname LIKE 'lazydb%' ORDER BY datname";
    let both = vec!["lazydb1".to_string(), "lazydb2".to_string()];
    assert_eq!(string_column(&db1, databases_sql).await?, both);
    assert_eq!(string_column(&db2, databases_sql).await?, both);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_views_report_the_database_their_context_serves() -> DFResult<()> {
    // information_schema.tables.table_catalog comes from a view body whose
    // current_database() is inlined at CREATE VIEW time. One context per
    // database is what makes that inlined literal correct: each context inlines
    // its own database rather than a single shared catalog name.
    for (database, relation) in [("lazydb1", "users"), ("lazydb2", "events")] {
        let ctx = ctx_serving_database_from_fake_source(database).await?;
        // text_column, not string_column: table_catalog reaches the client
        // through an information_schema domain cast, so its Arrow flavor is not
        // guaranteed to be plain Utf8.
        let catalogs = text_column(
            &ctx,
            &format!(
                "SELECT table_catalog FROM information_schema.tables \
                 WHERE table_name = '{relation}'"
            ),
        )
        .await?;
        assert_eq!(
            catalogs,
            vec![database.to_string()],
            "{database}'s views must report {database} as the catalog"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_registering_a_database_the_source_does_not_report_is_an_error() -> DFResult<()> {
    // A database name that will never match anything must fail here rather than
    // present as a catalog that is mysteriously empty.
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;

    let result = register_lazy_catalog(
        &ctx,
        Arc::new(FakeSource),
        LazyCatalogOptions::all(),
        "lazydb3",
    )
    .await;

    let message = result
        .expect_err("an unknown database must fail registration")
        .to_string();
    assert!(
        message.contains("lazydb3") && message.contains("lazydb1"),
        "the error must name the database asked for and those available, got: {message}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_catalog_error_surfaces_at_registration() -> DFResult<()> {
    // Validation asks the source for its databases, so a source that cannot
    // answer fails the build rather than producing a context that errors later
    // on every catalog query.
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;

    let result = register_lazy_catalog(
        &ctx,
        Arc::new(FailingSource),
        LazyCatalogOptions::all(),
        "anydb",
    )
    .await;

    let message = result
        .expect_err("a failing source must fail registration")
        .to_string();
    assert!(
        message.contains("boom from source"),
        "expected the source's own error, got: {message}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_database_scoped_table_cannot_be_registered_as_global() -> DFResult<()> {
    // register_database_independent_lazy_catalog serves its tables a placeholder
    // database, so letting a scoped table through would report an empty catalog
    // -- the same silent-empty failure the validation above prevents.
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;

    let result = register_database_independent_lazy_catalog(
        &ctx,
        Arc::new(FakeSource),
        LazyCatalogOptions::with_tables(vec![CatalogTable::PgDatabase, CatalogTable::PgClass]),
    )
    .await;

    let message = result
        .expect_err("a database-scoped table must be refused")
        .to_string();
    assert!(
        message.contains("pg_class"),
        "the error must name the offending table, got: {message}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_builtin_public_survives_without_a_lazy_source() -> DFResult<()> {
    // With no source supplying schemas there is nothing to shadow it with, so
    // the built-in public must remain -- otherwise a plain context would have
    // no public schema at all.
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;

    let names = string_column(
        &ctx,
        "SELECT nspname FROM pg_catalog.pg_namespace WHERE nspname = 'public'",
    )
    .await?;
    assert_eq!(names, vec!["public".to_string()]);
    Ok(())
}
