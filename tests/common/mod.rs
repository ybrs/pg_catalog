//! Shared helpers for the pg_catalog integration test crates. Each test file
//! pulls these in with `mod common;` (Rust compiles `tests/common/mod.rs` as a
//! submodule of each test crate, not as its own test binary). Not every crate
//! uses every helper, so per-crate dead-code warnings are allowed here.
#![allow(dead_code)]

use datafusion::error::Result as DFResult;
use datafusion::execution::context::SessionContext;
use datafusion_pg_catalog::get_base_session_context;

/// Build a base session context over the embedded catalog - the shipped Arrow IPC
/// artifact, loaded via the `None` schema path (the same fast path production
/// uses, with no YAML parsing). Catalog `pgtry`, schema `public`.
pub async fn base_ctx() -> DFResult<SessionContext> {
    let (ctx, _log) =
        get_base_session_context(None, "pgtry".to_string(), "public".to_string(), None).await?;
    Ok(ctx)
}
