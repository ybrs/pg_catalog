use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use arrow::array::Array;
use datafusion_pg_catalog::{
    get_base_session_context, register_user_database_with_callback, LazyDatabaseRow,
};

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

#[tokio::test]
async fn test_lazy_replaces_pg_database_rows() -> datafusion::error::Result<()> {
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
        None,
    )
    .await?;

    // Precondition: static dataset contains three rows: postgres, template0, template1
    let pre_df = ctx
        .sql("SELECT datname FROM pg_catalog.pg_database ORDER BY datname")
        .await?;
    let pre_batches = pre_df.collect().await?;
    let pre_rows: usize = pre_batches.iter().map(|b| b.num_rows()).sum();
    assert!(
        pre_rows >= 3,
        "expected at least the static databases before registration"
    );

    // Register a callback that returns only two custom databases.
    let fetcher = || {
        vec![
            LazyDatabaseRow::new("only_lazy_1", 27735),
            LazyDatabaseRow::new("only_lazy_2", 27735),
        ]
    };
    register_user_database_with_callback(&ctx, Arc::new(fetcher)).await?;

    // After registration, results should come exclusively from the callback.
    let post_df = ctx
        .sql("SELECT datname FROM pg_catalog.pg_database ORDER BY datname")
        .await?;
    let post_batches = post_df.collect().await?;
    let mut names: Vec<String> = Vec::new();
    for b in &post_batches {
        let arr = b
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap();
        for i in 0..arr.len() {
            if arr.is_valid(i) {
                names.push(arr.value(i).to_string());
            }
        }
    }
    assert_eq!(
        names,
        vec!["only_lazy_1".to_string(), "only_lazy_2".to_string()]
    );

    Ok(())
}
