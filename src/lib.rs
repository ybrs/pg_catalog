// Library module for pg_catalog functionality.
// Exposes modules so the crate can be used as a library as well as a binary.

pub mod clean_duplicate_columns;
pub mod register_table;
pub mod server;
pub mod db_table;
pub mod replace;
pub mod session;
pub mod logical_plan_rules;
pub mod scalar_to_cte;
pub mod replace_any_group_by;
pub mod router;
pub mod user_functions;
pub mod pg_catalog_helpers;
// Re-export all public functions from pg_catalog_helpers for convenience.
pub use pg_catalog_helpers::*;
// Re-export commonly used functions at crate root for convenience.
pub use router::dispatch_query;
pub use server::start_server;
pub use session::get_base_session_context;

/// Path to the extracted `pg_schema` directory downloaded at build time.
pub const DEFAULT_SCHEMA_DIR: &str = env!("DEFAULT_SCHEMA_DIR");
