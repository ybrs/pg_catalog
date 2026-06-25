use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use arrow::array::Array;
use datafusion::error::{DataFusionError, Result as DFResult};
use datafusion_pg_catalog::{
    get_base_session_context, get_base_session_context_with_lazy_catalog, register_lazy_catalog,
    register_user_database_with_callback, ColumnSpec, ConfigSettingDef, ConstraintDef, DatabaseDef,
    IndexDef, LazyCatalogOptions, LazyCatalogSource, LazyDatabaseRow, RelationDef, RelationKind,
    SchemaDef, SettingDef,
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

/// Build a base session and install the fake source over all catalog tables.
async fn ctx_with_fake_source() -> DFResult<datafusion::execution::context::SessionContext> {
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
        None,
    )
    .await?;
    register_lazy_catalog(&ctx, Arc::new(FakeSource), LazyCatalogOptions::all()).await?;
    Ok(ctx)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_register_pg_database_on_scan() -> DFResult<()> {
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
        None,
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
    let total_rows: usize = batches.iter().map(|b| b.num_rows()).sum();
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
        None,
    )
    .await?;

    // Precondition: static dataset contains the built-in databases.
    let pre_rows: usize = ctx
        .sql("SELECT datname FROM pg_catalog.pg_database")
        .await?
        .collect()
        .await?
        .iter()
        .map(|b| b.num_rows())
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
    let ctx = ctx_with_fake_source().await?;

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
    let ctx = ctx_with_fake_source().await?;

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
    let ctx = ctx_with_fake_source().await?;

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
    let ctx = ctx_with_fake_source().await?;

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

    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 2);
    let b = &batches[0];
    let name = b
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .unwrap();
    let pos = b
        .column(1)
        .as_any()
        .downcast_ref::<arrow::array::Int32Array>()
        .unwrap();
    let dt = b
        .column(2)
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .unwrap();
    let nullable = b
        .column(3)
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
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
    let ctx = ctx_with_fake_source().await?;

    // Filter pushes a relname predicate; projection selects a single column.
    let names = string_column(
        &ctx,
        "SELECT relname FROM pg_catalog.pg_class \
         WHERE relname IN ('users','events') ORDER BY relname",
    )
    .await?;
    assert_eq!(names, vec!["events".to_string(), "users".to_string()]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazy_catalog_error_propagates() -> DFResult<()> {
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
        None,
    )
    .await?;
    register_lazy_catalog(&ctx, Arc::new(FailingSource), LazyCatalogOptions::all()).await?;

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
        None,
        Arc::new(FakeSource),
        LazyCatalogOptions::all(),
    )
    .await?;

    let names = string_column(
        &ctx,
        "SELECT tablename FROM pg_catalog.pg_tables WHERE tablename IN ('users','events') ORDER BY tablename",
    )
    .await?;
    assert_eq!(names, vec!["events".to_string(), "users".to_string()]);

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
        None,
        Arc::new(FakeSource),
        LazyCatalogOptions::all(),
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
async fn test_lazy_registered_after_session_is_blind_to_views() -> DFResult<()> {
    // Control test documenting WHY get_base_session_context_with_lazy_catalog
    // exists: a view (pg_tables) is planned during session construction and binds
    // to whatever pg_class provider exists THEN. Registering the lazy source
    // afterwards rebinds the base table but NOT the already-created view, so the
    // view cannot see the lazy tables.
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
        None,
    )
    .await?;
    register_lazy_catalog(&ctx, Arc::new(FakeSource), LazyCatalogOptions::all()).await?;

    // The base table reflects the lazy rows ...
    let base = count_rows(
        &ctx,
        "SELECT count(*) FROM pg_catalog.pg_class WHERE relname = 'users'",
    )
    .await?;
    assert_eq!(base, 1, "base pg_class should see the lazy table");

    // ... but the view, bound earlier, does not.
    let via_view = count_rows(
        &ctx,
        "SELECT count(*) FROM pg_catalog.pg_tables WHERE tablename = 'users'",
    )
    .await?;
    assert_eq!(
        via_view, 0,
        "view bound before lazy registration must not see lazy tables"
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
        None,
    )
    .await?;
    register_lazy_catalog(&ctx, Arc::new(FakeSource), LazyCatalogOptions::all()).await?;
    register_lazy_catalog(&ctx, Arc::new(FakeSource), LazyCatalogOptions::all()).await?;

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

/// A source with one indexed table, to prove the relation flags reach pg_tables.
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
            let mut idx = IndexDef::new(80300, "indexed_pkey", 80200, vec![1]);
            idx.is_unique = true;
            idx.is_primary = true;
            callback(vec![idx]);
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
        None,
        Arc::new(IndexedSource),
        LazyCatalogOptions::all(),
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

    // Joining pg_index -> the index's pg_class name resolves the index by table.
    let by_table = string_column(
        &ctx,
        "SELECT i.relname FROM pg_catalog.pg_index x \
         JOIN pg_catalog.pg_class i ON i.oid = x.indexrelid \
         JOIN pg_catalog.pg_class t ON t.oid = x.indrelid \
         WHERE t.relname = 'indexed'",
    )
    .await?;
    assert_eq!(by_table, vec!["indexed_pkey".to_string()]);

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
async fn test_lazy_pg_constraint_reflects_source() -> DFResult<()> {
    // A constraint reported by the source becomes a pg_constraint row, and the
    // (now live) information_schema constraint views derive from it.
    let (ctx, _log) = get_base_session_context_with_lazy_catalog(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
        None,
        Arc::new(IndexedSource),
        LazyCatalogOptions::all(),
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
        None,
        Arc::new(IndexedSource),
        LazyCatalogOptions::all(),
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
        None,
        Arc::new(IndexedSource),
        LazyCatalogOptions::all(),
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
    let ctx = ctx_with_fake_source().await?;
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
        None,
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
        None,
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
        None,
    )
    .await?;
    register_lazy_catalog(
        &ctx,
        Arc::new(DuplicateRelationSource),
        LazyCatalogOptions::all(),
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
        None,
    )
    .await?;
    register_lazy_catalog(&ctx, Arc::new(ConfigSource), LazyCatalogOptions::all()).await?;

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
        None,
    )
    .await?;
    register_lazy_catalog(&ctx, Arc::new(ConfigSource), LazyCatalogOptions::all()).await?;

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
        None,
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
        None,
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
