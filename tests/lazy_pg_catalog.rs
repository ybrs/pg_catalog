use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

use datafusion_pg_catalog::{get_base_session_context, register_user_database_with_callback, LazyDatabaseRow};

#[tokio::test]
async fn test_lazy_register_pg_database_on_scan() -> datafusion::error::Result<()> {
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
            LazyDatabaseRow::new("lazy_db1", 27735),
            LazyDatabaseRow::new("lazy_db2", 27735),
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
