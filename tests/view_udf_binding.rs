//! When does a UDF inside a view body actually run, and which instance runs?
//!
//! A view is a SQL statement evaluated when you SELECT from it, so a function
//! in its body must execute per SELECT. That is the behaviour these tests pin
//! down, because the emulation layer's design depends on it: session identity
//! (`current_user`, `current_database`) has to be registered once, before the
//! 136 catalog views are created, and still report the connecting session's
//! values.
//!
//! There are two separate questions, and conflating them led to a wrong claim:
//!   1. Does the function BODY run at SELECT time, so that state it reads is
//!      read fresh on every query? (If not, no call-time mechanism works.)
//!   2. If a DIFFERENT function is registered under the same name after the
//!      view exists, does the view pick up the new one? `DataFusion`'s CREATE
//!      VIEW stores a resolved logical plan rather than the SQL text, so this
//!      is not obviously the same question.

use std::sync::{Arc, RwLock};

use arrow::array::StringArray;
use arrow::datatypes::DataType;
use datafusion::error::Result as DFResult;
use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
use datafusion::prelude::SessionContext;
use datafusion::scalar::ScalarValue;

/// Register a no-argument UDF under `name` that returns whatever `slot` holds
/// when it is called.
fn register_slot_reader(
    ctx: &SessionContext,
    name: &str,
    slot: Arc<RwLock<String>>,
    volatility: Volatility,
) {
    let udf = create_udf(
        name,
        vec![],
        DataType::Utf8,
        volatility,
        Arc::new(move |_args| {
            let value = slot.read().expect("slot poisoned").clone();
            Ok(ColumnarValue::Scalar(ScalarValue::Utf8(Some(value))))
        }),
    );
    ctx.register_udf(udf);
}

/// Register a no-argument UDF under `name` returning a fixed value.
fn register_constant(ctx: &SessionContext, name: &str, value: &str) {
    let value = value.to_string();
    let udf = create_udf(
        name,
        vec![],
        DataType::Utf8,
        Volatility::Stable,
        Arc::new(move |_args| {
            Ok(ColumnarValue::Scalar(ScalarValue::Utf8(Some(
                value.clone(),
            ))))
        }),
    );
    ctx.register_udf(udf);
}

/// Run a single-value query and return the string it produced.
async fn value_of(ctx: &SessionContext, sql: &str) -> DFResult<String> {
    let batches = ctx.sql(sql).await?.collect().await?;
    let batch = batches.first().expect("one batch");
    let column = batch
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("utf8 column");
    Ok(column.value(0).to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_stable_udf_in_a_view_runs_at_select_time() -> DFResult<()> {
    // A Stable no-argument UDF is the shape session identity uses. If the
    // planner constant-folded it into the view's stored plan, changing the slot
    // afterwards would not show up.
    let ctx = SessionContext::new();
    let slot = Arc::new(RwLock::new("first".to_string()));
    register_slot_reader(&ctx, "who", slot.clone(), Volatility::Stable);
    ctx.sql("CREATE VIEW v AS SELECT who() AS w")
        .await?
        .collect()
        .await?;

    assert_eq!(value_of(&ctx, "SELECT w FROM v").await?, "first");

    *slot.write().unwrap() = "second".to_string();
    assert_eq!(
        value_of(&ctx, "SELECT w FROM v").await?,
        "second",
        "a view must re-evaluate its body on every SELECT"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_volatile_udf_in_a_view_runs_at_select_time() -> DFResult<()> {
    // Same, with Volatility::Volatile, to show the answer does not hinge on
    // the volatility declaration.
    let ctx = SessionContext::new();
    let slot = Arc::new(RwLock::new("first".to_string()));
    register_slot_reader(&ctx, "who", slot.clone(), Volatility::Volatile);
    ctx.sql("CREATE VIEW v AS SELECT who() AS w")
        .await?
        .collect()
        .await?;

    assert_eq!(value_of(&ctx, "SELECT w FROM v").await?, "first");
    *slot.write().unwrap() = "second".to_string();
    assert_eq!(value_of(&ctx, "SELECT w FROM v").await?, "second");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_reregistering_the_name_after_the_view_exists() -> DFResult<()> {
    // The other question: replacing the FUNCTION under the same name after the
    // view was created. CREATE VIEW stores a resolved plan, so whether the view
    // follows the new registration decides whether per-connection registration
    // could ever fix a view -- which is what riffq's register_current_user
    // attempts.
    let ctx = SessionContext::new();
    register_constant(&ctx, "who", "original");
    ctx.sql("CREATE VIEW v AS SELECT who() AS w")
        .await?
        .collect()
        .await?;
    assert_eq!(value_of(&ctx, "SELECT w FROM v").await?, "original");

    register_constant(&ctx, "who", "replacement");

    // A direct call must see the new registration.
    assert_eq!(value_of(&ctx, "SELECT who()").await?, "replacement");

    // The view: does it follow, or keep the instance it was planned with?
    let through_view = value_of(&ctx, "SELECT w FROM v").await?;
    println!("view after re-registration returned: {through_view}");
    assert!(
        through_view == "replacement" || through_view == "original",
        "unexpected value {through_view}"
    );
    Ok(())
}
