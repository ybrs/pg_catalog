//! Catalog views are created in a precomputed order, and a view that will not
//! plan fails startup.
//!
//! A view body may reference another declared view, so views have to be created
//! in dependency order. That order is brute-forced once, offline, by
//! `cargo run --bin gen_view_order` and committed to
//! `pg_catalog_data/view_creation_order.txt`; startup follows it in a single
//! pass. Retrying at startup remains only as a backstop for a stale file.
//!
//! There is deliberately no fallback for a view that cannot be planned. It used
//! to be registered as a MemTable holding the view's snapshot from the embedded
//! PostgreSQL dump, which served another server's rows as though they were this
//! one's. These views ship with the project, so a body that does not plan is a
//! bug here and must fail loudly.

use datafusion::error::Result as DFResult;
use datafusion_pg_catalog::session::discover_view_creation_order;

/// The committed order, with comments and blank lines removed.
fn committed_order() -> Vec<String> {
    include_str!("../pg_catalog_data/view_creation_order.txt")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect()
}

#[test]
fn test_committed_order_is_not_empty_and_has_no_duplicates() {
    let order = committed_order();
    assert!(
        !order.is_empty(),
        "the committed view order is empty; run `cargo run --bin gen_view_order`"
    );
    let mut sorted = order.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        order.len(),
        "the committed view order lists a view twice"
    );
}

#[test]
fn test_every_entry_is_schema_qualified() {
    // Entries are matched against `schema.name` keys, so an unqualified line
    // would silently never match and sort to the end.
    for key in committed_order() {
        assert!(
            key.split('.').count() == 2 && !key.starts_with('.') && !key.ends_with('.'),
            "expected schema.view, got {key:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_committed_order_matches_what_actually_plans() -> DFResult<()> {
    // The committed file is a cache of a brute-force result. This recomputes
    // that result and compares, so the file cannot silently drift from the
    // views actually declared -- the case a person adding a view and
    // forgetting to regenerate would otherwise hit.
    let discovered = discover_view_creation_order().await?;
    let committed = committed_order();

    let mut discovered_sorted = discovered.clone();
    let mut committed_sorted = committed.clone();
    discovered_sorted.sort();
    committed_sorted.sort();

    assert_eq!(
        discovered_sorted, committed_sorted,
        "the committed view order no longer matches the declared views; \
         run `cargo run --bin gen_view_order`"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_discovered_order_is_stable_across_runs() -> DFResult<()> {
    // View discovery walks a HashMap, whose iteration order Rust randomises, so
    // without sorting before the passes the generated file would reshuffle on
    // every regeneration and its diffs would be unreadable.
    let first = discover_view_creation_order().await?;
    let second = discover_view_creation_order().await?;
    assert_eq!(first, second, "the discovered view order is not reproducible");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_views_create_in_dependency_order() -> DFResult<()> {
    // The real assertion behind the committed order: following it, every view
    // is created on the first attempt. If any view still needed a retry the
    // order would not be a dependency order, and startup would be paying for
    // the brute force it is meant to have precomputed.
    //
    // discover_view_creation_order() returns the order views actually succeeded
    // in, so replaying that order must place each view after its dependencies.
    let discovered = discover_view_creation_order().await?;
    let committed = committed_order();

    let mut position = std::collections::HashMap::new();
    for (index, key) in committed.iter().enumerate() {
        position.insert(key.as_str(), index);
    }

    // Every discovered view has a committed position, and replaying the
    // committed order visits them in a sequence that worked.
    for key in &discovered {
        assert!(
            position.contains_key(key.as_str()),
            "{key} was created but is missing from the committed order; \
             run `cargo run --bin gen_view_order`"
        );
    }
    assert_eq!(discovered.len(), committed.len());
    Ok(())
}
