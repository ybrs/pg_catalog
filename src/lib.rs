// Library module for pg_catalog functionality.
// Exposes modules so the crate can be used as a library as well as a binary.

pub mod clean_duplicate_columns;
pub mod db_table;
pub mod lazy_catalog;
pub mod lazy_pg_catalog_helpers;
pub mod logical_plan_rules;
pub mod pg_catalog_helpers;
pub mod register_table;
pub mod replace;
pub mod replace_any_group_by;
pub mod router;
pub mod scalar_to_cte;
pub mod server;
pub mod session;
pub mod user_functions;
// Re-export all public functions from pg_catalog_helpers for convenience.
pub use lazy_catalog::*;
pub use lazy_pg_catalog_helpers::*;
pub use pg_catalog_helpers::*;
// Re-export commonly used functions at crate root for convenience.
pub use router::dispatch_query;
pub use server::start_server;
pub use session::{
    build_ipc_artifact, get_base_session_context, get_base_session_context_with_lazy_catalog,
};
pub use user_functions::{
    clear_index_definition_resolver, clear_pg_sequence_last_value_resolver,
    clear_row_security_active_resolver, clear_view_definition_resolver,
    set_index_definition_resolver, set_pg_sequence_last_value_resolver,
    set_row_security_active_resolver, set_view_definition_resolver, DefinitionResolver,
    IndexDefinitionResolver, IndexIdentity, RowSecurityActiveResolver, SequenceLastValueResolver,
    ViewDefinitionResolver, ViewIdentity,
};
