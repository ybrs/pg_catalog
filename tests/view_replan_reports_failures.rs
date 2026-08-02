//! A view that cannot re-plan must be reported, not passed over in silence.
//!
//! `register_lazy_catalog` swaps base table providers on a context whose views
//! already exist. `CREATE VIEW` stores a RESOLVED plan, so a view planned before
//! the swap keeps reading the old provider - re-planning is the only way it sees
//! the new source. That mechanism is right and stays.
//!
//! What was wrong is being quiet when it does not work. The replay discarded
//! both the planning error and the execution error, so a view whose body no
//! longer plans kept its PREVIOUS definition - still bound to the provider that
//! was just replaced - and the caller was told nothing. The client then queries
//! a view that looks fine and gets rows from a source that was supposed to have
//! been superseded. Silent stale data is the worst shape this failure can take.
//!
//! It was also inconsistent with the same condition elsewhere: startup HARD
//! FAILS on a view body it cannot plan, naming the view. So one unplannable body
//! was fatal during creation and invisible during the replay.
//!
//! The recovery is unchanged - the prior definition stays, so the object remains
//! queryable. Only the silence is gone.

use std::sync::Arc;

use arrow::array::{Int32Array, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema};
use datafusion::datasource::MemTable;
use datafusion::error::Result as DFResult;
use datafusion_pg_catalog::session::replan_registered_views_against_current_providers;

mod common;
use common::base_ctx;

/// Replace `pg_catalog.pg_class` with a provider carrying one unrelated column.
///
/// Every catalog view that selects a real `pg_class` column then fails to
/// re-plan, which is the condition under test. Swapping a provider for one with
/// a different schema is what a badly-behaved lazy source amounts to.
fn narrow_pg_class(ctx: &datafusion::prelude::SessionContext) -> DFResult<()> {
    let schema = Arc::new(Schema::new(vec![Field::new(
        "unrelated",
        DataType::Int32,
        true,
    )]));
    let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(Int32Array::from(vec![1]))])?;
    let table = MemTable::try_new(schema, vec![vec![batch]])?;
    // Deregister first: register_table refuses to replace an existing name, and
    // swapping the provider under a live name is the whole point here.
    ctx.deregister_table("pg_catalog.pg_class")?;
    ctx.register_table("pg_catalog.pg_class", Arc::new(table))?;
    Ok(())
}

/// With every provider intact, the replay reports nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_replay_that_works_reports_nothing() -> DFResult<()> {
    let ctx = base_ctx().await?;
    replan_registered_views_against_current_providers(&ctx)
        .await
        .expect("every view re-plans against the providers it was built with");
    Ok(())
}

/// A view that cannot re-plan is named in the error.
#[tokio::test(flavor = "multi_thread")]
async fn a_view_that_cannot_replan_is_named() -> DFResult<()> {
    let ctx = base_ctx().await?;
    narrow_pg_class(&ctx)?;

    let error = replan_registered_views_against_current_providers(&ctx)
        .await
        .expect_err("views over the replaced provider can no longer plan");
    let message = error.to_string();

    assert!(
        message.contains("pg_catalog.pg_tables") || message.contains("pg_class"),
        "the error should name the views that failed, got {message}"
    );
    assert!(
        message.contains("could not be re-planned"),
        "the error should say what went wrong, got {message}"
    );
    Ok(())
}

/// Every failing view is reported, not just the first one hit.
///
/// A caller repairing a lazy source wants the whole list; stopping at the first
/// turns one fix-and-rerun cycle into as many as there are broken views.
#[tokio::test(flavor = "multi_thread")]
async fn every_failing_view_is_reported_not_just_the_first() -> DFResult<()> {
    let ctx = base_ctx().await?;
    narrow_pg_class(&ctx)?;

    let message = replan_registered_views_against_current_providers(&ctx)
        .await
        .expect_err("the replay fails")
        .to_string();

    // The message opens with the count, so read it back rather than counting
    // separators, which the view bodies themselves could contain.
    let reported: usize = message
        .split_whitespace()
        .find_map(|word| word.parse().ok())
        .expect("the error should open with how many views failed");
    assert!(
        reported > 1,
        "many catalog views read pg_class, so more than one should be reported; got {message}"
    );
    Ok(())
}
