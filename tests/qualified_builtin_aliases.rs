//! `pg_catalog.`-qualified spellings of functions `DataFusion` provides bare.
//!
//! `PostgreSQL` puts every built-in function in the `pg_catalog` schema and
//! clients write them that way. `DataFusion` knows `concat_ws` and knew nothing
//! called `pg_catalog.concat_ws`, so psql's `\drg` failed with "function
//! `pg_catalog.concat_ws()` does not exist" while the function sat there under
//! its bare name.
//!
//! An audit over every `pg_catalog.<fn>(` call in psql's backslash SQL (both
//! 17.4 and 18.4) and in the captured client queries found `concat_ws` as the
//! only one a real client calls today - but `concat` and `length` were in the
//! same state, so the fix registers a list rather than the one name that
//! happened to be reported.
//!
//! Two properties matter and are tested separately: the qualified spelling has
//! to RESOLVE, and it has to mean the same thing as the bare one. A test that
//! only checked resolution would pass on an alias pointing at the wrong
//! function.
//!
//! The third test is the one that would catch a real regression: aliasing runs
//! last in session construction precisely so a `pg_catalog` function this crate
//! implements ITSELF is never replaced by a `DataFusion` builtin sharing its
//! bare name. `version()` is the sharp case - `DataFusion` has one, and it
//! reports `DataFusion`'s version, not `PostgreSQL`'s.

use datafusion::error::Result as DFResult;
use datafusion::prelude::SessionContext;
use datafusion_pg_catalog::session::execute_sql;

mod common;
use common::base_ctx;

/// The single text value `sql` projects, or None when it is NULL or absent.
async fn single_text(ctx: &SessionContext, sql: &str) -> DFResult<Option<String>> {
    use arrow::array::Array;

    let (batches, _schema) = execute_sql(ctx, sql, None, None).await?;
    for batch in &batches {
        if batch.num_rows() > 0 {
            let column = arrow::compute::cast(batch.column(0), &arrow::datatypes::DataType::Utf8)?;
            if column.is_null(0) {
                return Ok(None);
            }
            return Ok(Some(
                arrow::array::as_string_array(&column).value(0).to_string(),
            ));
        }
    }
    Ok(None)
}

/// One call written both ways: bare, and `pg_catalog`-qualified.
const CALLS: &[(&str, &str)] = &[
    // The call psql's \drg makes, and the reason this exists.
    (
        "concat_ws(',', 'a', 'b')",
        "pg_catalog.concat_ws(',', 'a', 'b')",
    ),
    ("concat('a', 'b')", "pg_catalog.concat('a', 'b')"),
    ("length('abc')", "pg_catalog.length('abc')"),
    ("lower('ABC')", "pg_catalog.lower('ABC')"),
    ("upper('abc')", "pg_catalog.upper('abc')"),
    ("md5('a')", "pg_catalog.md5('a')"),
    ("btrim('  a  ')", "pg_catalog.btrim('  a  ')"),
    (
        "replace('abc', 'b', 'x')",
        "pg_catalog.replace('abc', 'b', 'x')",
    ),
    (
        "split_part('a,b', ',', 2)",
        "pg_catalog.split_part('a,b', ',', 2)",
    ),
    ("reverse('abc')", "pg_catalog.reverse('abc')"),
];

/// Every qualified spelling resolves.
#[tokio::test(flavor = "multi_thread")]
async fn the_qualified_spelling_resolves() -> DFResult<()> {
    let ctx = base_ctx().await?;
    for (_bare, qualified) in CALLS {
        execute_sql(&ctx, &format!("SELECT {qualified}"), None, None)
            .await
            .unwrap_or_else(|error| panic!("{qualified} should resolve: {error}"));
    }
    Ok(())
}

/// And answers what the bare spelling answers.
///
/// Resolution alone would be satisfied by an alias bound to the wrong
/// function; this is what says the two names mean one thing.
#[tokio::test(flavor = "multi_thread")]
async fn the_qualified_spelling_means_the_same_as_the_bare_one() -> DFResult<()> {
    let ctx = base_ctx().await?;
    for (bare, qualified) in CALLS {
        let from_bare = single_text(&ctx, &format!("SELECT {bare}")).await?;
        let from_qualified = single_text(&ctx, &format!("SELECT {qualified}")).await?;
        assert_eq!(
            from_bare, from_qualified,
            "{bare} and {qualified} must be the same function"
        );
        assert!(
            from_bare.is_some(),
            "{bare} should answer something for this test to mean anything"
        );
    }
    Ok(())
}

/// This crate's own `pg_catalog` functions are not replaced by builtins.
///
/// The aliasing pass runs last for exactly this reason. `version` is the case
/// that would show it: `DataFusion` ships one, and if the builtin won here the
/// server would report `DataFusion`'s version to a client asking `PostgreSQL`'s.
#[tokio::test(flavor = "multi_thread")]
async fn a_builtin_never_replaces_this_catalogs_own_function() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let version = single_text(&ctx, "SELECT pg_catalog.version()")
        .await?
        .expect("version() answers");
    assert!(
        version.starts_with("PostgreSQL"),
        "pg_catalog.version() must stay this catalog's own, got {version:?}"
    );

    // array_to_string is registered by this crate AND is a DataFusion builtin
    // name, so it is the other side of the same guarantee.
    let joined = single_text(
        &ctx,
        "SELECT pg_catalog.array_to_string(ARRAY['a', 'b'], ',')",
    )
    .await?;
    assert_eq!(joined.as_deref(), Some("a,b"));
    Ok(())
}
