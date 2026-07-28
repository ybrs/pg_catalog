// Session management utilities.
// Loads YAML schemas into MemTables, registers UDFs and executes rewritten queries using DataFusion.
// Separated to encapsulate DataFusion setup and query execution behaviour.

use arrow::array::ArrayRef;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use datafusion::catalog::memory::{MemoryCatalogProvider, MemorySchemaProvider};
use datafusion::error::DataFusionError;
use datafusion::execution::context::SessionContext;
use serde::Deserialize;
use serde_yaml;

use crate::clean_duplicate_columns::{
    alias_unnamed_columns, disambiguate_duplicate_columns, restore_aliased_column_names,
};
use crate::replace::{
    alias_subquery_tables, decorrelate_lateral_aggregate, drop_oid_array_cast,
    drop_redundant_oid_and_regclass_casts, regclass_udfs, replace_regclass,
    replace_set_command_with_namespace, resolve_order_by_names_to_output_positions,
    resolve_regproc_columns_to_oids_in_comparisons, rewrite_array_agg_varchar_cast,
    rewrite_array_subquery, rewrite_array_upper_to_array_length,
    rewrite_available_extension_versions_source, rewrite_available_updates,
    rewrite_boolean_column_char_comparisons, rewrite_boolean_scalar_subquery_to_exists,
    rewrite_brace_array_literal, rewrite_char_cast, rewrite_correlated_limit_one_subquery_to_max,
    rewrite_exists_to_count, rewrite_information_schema_casts, rewrite_name_cast, rewrite_oid_cast,
    rewrite_pg_custom_operator, rewrite_pg_truetypid_composite_args, rewrite_regoper_cast,
    rewrite_regoperator_cast, rewrite_regproc_cast, rewrite_regprocedure_cast,
    rewrite_regtype_cast, rewrite_schema_qualified_custom_types, rewrite_schema_qualified_text,
    rewrite_schema_qualified_udtfs, rewrite_srf_to_unnest, rewrite_text_backed_type_casts,
    rewrite_time_zone_utc, rewrite_tuple_equality, rewrite_tuple_in_subquery_to_exists,
    rewrite_xid_cast, strip_default_collate,
};
use pgwire::api::Type;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::{Arc, Mutex};
use zip::ZipArchive;

use crate::user_functions::{
    register_acldefault, register_aclexplode, register_array_agg, register_current_database,
    register_current_schema, register_current_schemas, register_encode, register_format,
    register_getdatabaseencoding, register_has_database_privilege, register_has_privilege_family,
    register_has_schema_privilege, register_nameconcatoid,
    register_pg_available_extension_versions, register_pg_char_max_length,
    register_pg_char_octet_length, register_pg_column_is_updatable, register_pg_expandarray,
    register_pg_get_array, register_pg_get_function_arg_default,
    register_pg_get_function_arguments, register_pg_get_function_result,
    register_pg_get_function_sqlbody, register_pg_get_indexdef, register_pg_get_keywords,
    register_pg_get_one, register_pg_get_ruledef, register_pg_get_statisticsobjdef_columns,
    register_pg_get_triggerdef, register_pg_get_viewdef, register_pg_has_role,
    register_pg_index_position, register_pg_is_other_temp_schema, register_pg_my_temp_schema,
    register_pg_numeric_helpers, register_pg_options_to_table, register_pg_postmaster_start_time,
    register_pg_relation_is_publishable, register_pg_relation_is_updatable,
    register_pg_relation_size, register_pg_sequence_last_value, register_pg_total_relation_size,
    register_pg_truetypid_helpers, register_quote_ident, register_row_security_active,
    register_scalar_array_to_string, register_scalar_format_type, register_scalar_pg_age,
    register_scalar_pg_encoding_to_char, register_scalar_pg_get_expr,
    register_scalar_pg_get_partkeydef, register_scalar_pg_get_userbyid,
    register_scalar_pg_is_in_recovery, register_scalar_pg_proc_oid,
    register_scalar_pg_table_is_visible, register_scalar_pg_tablespace_location,
    register_scalar_regclass_oid, register_scalar_txid_current, register_session_identity,
    register_translate, register_upper, register_version_fn,
};

use crate::scalar_to_cte::rewrite_subquery_as_cte;
use bytes::Bytes;

use datafusion::common::{config::ConfigEntry, config_err};
use datafusion::scalar::ScalarValue;

/// The embedded catalog, as a zip of per-table Arrow IPC streams. Loaded at
/// startup (when no explicit schema path is given) far faster than parsing the
/// YAML. Regenerate with `cargo run --release --bin gen_schema_ipc` after the
/// YAML catalog changes; the YAML zip remains the human-editable source.
static SCHEMA_IPC: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/pg_catalog_data/postgres-schema-nightly-ipc.zip"
));
use crate::db_table::{map_pg_type, ScanRecordingMemTable, ScanTrace};
use crate::lazy_catalog::{register_lazy_catalog, LazyCatalogOptions, LazyCatalogSource};
use crate::replace_any_group_by::rewrite_group_by_for_any;
use datafusion::common::config::{ConfigExtension, ExtensionOptions};
use std::sync::OnceLock;

/// The `CREATE OR REPLACE VIEW` statements [`create_registered_views`] ran for each
/// session, in execution order, together with the schema their bodies were resolved
/// under. Keyed by [`SessionContext::session_id`] so concurrent sessions in one
/// process do not clobber each other.
///
/// A `DataFusion` view stores the logical plan it was planned from; that plan
/// captures the concrete table providers present at `CREATE VIEW` time and is not
/// re-resolved on later queries. When [`register_lazy_catalog`] swaps a base
/// table's provider for a lazy one, any view already planned against the old
/// provider keeps reading the old provider and never sees the lazy rows. Replaying
/// these statements after the swap re-plans each view against the lazy providers so
/// the views reflect the source's rows. The map is process-global because
/// [`register_lazy_catalog`] is a standalone entry point that receives only a
/// `SessionContext`; the per-session key keeps each session's statements distinct.
fn registered_view_statements() -> &'static Mutex<HashMap<String, Vec<RegisteredViewStatement>>> {
    static SLOT: OnceLock<Mutex<HashMap<String, Vec<RegisteredViewStatement>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One replayable `CREATE OR REPLACE VIEW`, plus the default schema its body must
/// be resolved under (catalog view bodies reference base tables unqualified, all of
/// which live in `pg_catalog`).
#[derive(Clone)]
struct RegisteredViewStatement {
    body_resolution_schema: String,
    create_sql: String,
}

/// Re-plan the catalog views recorded by [`create_registered_views`] so they bind
/// to the table providers currently registered.
///
/// [`register_lazy_catalog`] calls this after swapping base table providers, so the
/// views that derive from those base tables re-resolve against the lazy providers
/// instead of the built-in snapshots they were first planned against. A view body
/// that no longer plans is left as it was rather than dropped.
///
/// # Errors
///
/// Returns an error if the `SET datafusion.catalog.default_schema` statements that
/// bracket the replay fail - the view bodies themselves cannot fail this call,
/// since a body that no longer plans is deliberately ignored.
///
/// # Panics
///
/// Panics if the process-global statement registry's mutex is poisoned, which
/// means another thread panicked while recording or replaying view statements.
pub async fn replan_registered_views_against_current_providers(
    ctx: &SessionContext,
) -> datafusion::error::Result<(), DataFusionError> {
    let statements = registered_view_statements()
        .lock()
        .unwrap()
        .get(&ctx.session_id())
        .cloned()
        .unwrap_or_default();
    if statements.is_empty() {
        return Ok(());
    }

    let original_default_schema = {
        let state = ctx.state();
        state.config_options().catalog.default_schema.clone()
    };

    for statement in statements {
        set_default_schema(ctx, &statement.body_resolution_schema).await?;
        // A body that fails to re-plan leaves the prior view definition in place,
        // so the object stays queryable rather than being dropped.
        if let Ok(df) = ctx.sql(&statement.create_sql).await {
            let _ = df.collect().await;
        }
    }

    set_default_schema(ctx, &original_default_schema).await
}

/// The per-connection `PostgreSQL` GUCs this crate serves, carried on the session
/// as a `DataFusion` config extension.
///
/// Storing them in the session config rather than in a global is what lets each
/// connection answer `SHOW`/`SET` and the session-identity functions with its own
/// values while every connection shares one set of catalog tables and views.
#[derive(Clone, Debug)]
pub struct ClientOpts {
    pub application_name: String,
    pub datestyle: String,
    pub search_path: String,
    /// The role this connection authenticated as, reported by `current_user`,
    /// `session_user` and `current_role`.
    ///
    /// Per connection, and read at the moment those functions are called rather
    /// than when they are registered: the catalog views are planned once at
    /// startup, before any client exists, so anything baked in at registration
    /// would freeze every connection to one role. See
    /// [`crate::user_functions::register_session_identity`].
    ///
    /// Defaults to `postgres` for a host that never sets it, which is the value
    /// every connection used to get.
    pub session_user: String,
}

/// The role a connection reports before its host has said who connected.
pub const DEFAULT_SESSION_USER: &str = "postgres";

impl Default for ClientOpts {
    /// The GUC values a connection starts with before its host overrides any:
    /// `PostgreSQL`'s own defaults for `DateStyle` and `search_path`, an empty
    /// application name, and [`DEFAULT_SESSION_USER`] as the role.
    fn default() -> Self {
        Self {
            application_name: String::new(),
            datestyle: "ISO, MDY".to_string(),
            search_path: "\"$user\", public".to_string(),
            session_user: DEFAULT_SESSION_USER.to_string(),
        }
    }
}

/// Record the role `user` authenticated as on `ctx`, so that connection's
/// `current_user` / `session_user` / `current_role` report it.
///
/// `ctx` must be the connection's own context - a clone of the shared base, not
/// the base itself - or every connection sharing that context reports the last
/// role written.
///
/// # Errors
///
/// Returns an error if `ctx` carries no [`ClientOpts`] config extension, which
/// means it was not built by this crate and has nowhere to record the role.
pub fn set_session_user(ctx: &SessionContext, user: &str) -> datafusion::error::Result<()> {
    let state_ref = ctx.state_ref();
    let mut state = state_ref.write();
    let opts = state
        .config_mut()
        .options_mut()
        .extensions
        .get_mut::<ClientOpts>()
        .ok_or_else(|| {
            DataFusionError::Execution(
                "this session has no ClientOpts, so the session user cannot be recorded"
                    .to_string(),
            )
        })?;
    opts.session_user = user.to_string();
    Ok(())
}

impl ConfigExtension for ClientOpts {
    /// The namespace these options are addressed under, so a client writes
    /// `SET pg_catalog.application_name = ...`.
    const PREFIX: &'static str = "pg_catalog";
}

impl ExtensionOptions for ClientOpts {
    /// Downcast handle `DataFusion` uses to recover the concrete options type
    /// from the type-erased extension slot.
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    /// Mutable counterpart of `as_any`, used by [`set_session_user`] to write a
    /// connection's role.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    /// Deep-copy the options, which is how a per-connection context gets its own
    /// values instead of sharing the base context's.
    fn cloned(&self) -> Box<dyn ExtensionOptions> {
        Box::new(self.clone())
    }

    /// Apply one `SET pg_catalog.<key> = <value>` statement.
    ///
    /// `extra_float_digits` is accepted and ignored: clients set it routinely at
    /// connection time and it does not affect how this crate formats results, so
    /// rejecting it would fail those connections for nothing.
    ///
    /// # Errors
    ///
    /// Returns an error for any other key, since an unrecognized GUC would
    /// otherwise be silently dropped and read back with the wrong value.
    fn set(&mut self, key: &str, value: &str) -> datafusion::error::Result<()> {
        log::debug!("set key {key:?}");
        match key {
            "application_name" => {
                self.application_name = value.to_string();
                log::debug!("value is set!!!");
                Ok(())
            }
            "datestyle" => {
                self.datestyle = value.to_string();
                Ok(())
            }
            "search_path" => {
                self.search_path = value.to_string();
                Ok(())
            }
            "session_user" => {
                self.session_user = value.to_string();
                Ok(())
            }
            "extra_float_digits" => Ok(()),
            _ => config_err!("unknown key {key}"),
        }
    }

    /// Report every option and its current value, which is what `SHOW ALL` and
    /// `SHOW pg_catalog.<key>` read.
    fn entries(&self) -> Vec<ConfigEntry> {
        vec![
            ConfigEntry {
                key: "application_name".to_string(),
                value: Some(self.application_name.clone()),
                description: "",
            },
            ConfigEntry {
                key: "datestyle".to_string(),
                value: Some(self.datestyle.clone()),
                description: "",
            },
            ConfigEntry {
                key: "search_path".to_string(),
                value: Some(self.search_path.clone()),
                description: "",
            },
            ConfigEntry {
                key: "session_user".to_string(),
                value: Some(self.session_user.clone()),
                description: "",
            },
        ]
    }
}

/// One table or view as the YAML catalog declares it: its column name -> `PostgreSQL`
/// type map, its optional rows, and - for a view - the body it is defined by.
///
/// `pg_types` exists next to `schema` because the exporter records a few columns
/// (the `bytea` ones) under a wider type in `schema` than they really have; see
/// [`build_table`].
#[derive(Debug, Deserialize)]
struct TableDef {
    #[serde(rename = "type", default)]
    table_type: Option<String>,
    schema: BTreeMap<String, String>,
    #[serde(default)]
    pg_types: Option<BTreeMap<String, String>>,
    rows: Option<Vec<BTreeMap<String, serde_json::Value>>>,
    #[serde(default)]
    view_sql: Option<String>,
}

/// A whole YAML catalog document, nested catalog -> schema -> table -> definition.
#[derive(Debug, Deserialize)]
struct YamlSchema(HashMap<String, HashMap<String, HashMap<String, TableDef>>>);

/// A catalog table after its declaration has been turned into Arrow: the Arrow
/// schema, the materialized rows, and the view body if it is a view.
///
/// The same shape is produced by both loaders (YAML and the embedded Arrow IPC
/// artifact), so registration downstream does not care which one ran.
#[derive(Clone)]
struct ParsedTable {
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
    view_sql: Option<String>,
    is_view: bool,
}

/// A declared view queued for `CREATE OR REPLACE VIEW`, held until every base
/// table is registered so the body has something to resolve against.
#[derive(Clone)]
struct ViewToRegister {
    catalog: String,
    schema: String,
    name: String,
    sql: String,
}

/// Declared views the server serves as `CREATE VIEW`s, re-derived from the base
/// tables on every query. Every view the embedded catalog declares is listed, so
/// no view is served as a frozen snapshot.
///
/// A listed view whose body fails to plan FAILS STARTUP naming the view; it is
/// not substituted by anything. Adding a view here therefore means committing to
/// a body that plans -- see [`create_registered_views`].
///
/// A declared view left off this list would be registered as a `MemTable` holding
/// its snapshot rows, which is not view semantics at all. Nothing is off the list
/// today, and nothing should be.
const VIEWS_TO_REGISTER: &[(&str, &str)] = &[
    ("pg_catalog", "pg_views"),
    ("pg_catalog", "pg_tables"),
    ("information_schema", "table_constraints"),
    ("information_schema", "key_column_usage"),
    ("information_schema", "constraint_column_usage"),
    ("information_schema", "referential_constraints"),
    ("pg_catalog", "pg_indexes"),
    ("pg_catalog", "pg_matviews"),
    ("pg_catalog", "pg_shadow"),
    ("information_schema", "routines"),
    ("information_schema", "sequences"),
    ("information_schema", "domains"),
    ("information_schema", "attributes"),
    ("information_schema", "triggers"),
    ("information_schema", "user_defined_types"),
    ("information_schema", "column_udt_usage"),
    ("information_schema", "column_domain_usage"),
    ("information_schema", "constraint_table_usage"),
    ("information_schema", "character_sets"),
    ("information_schema", "collations"),
    (
        "information_schema",
        "collation_character_set_applicability",
    ),
    ("information_schema", "check_constraint_routine_usage"),
    ("information_schema", "column_column_usage"),
    ("information_schema", "domain_constraints"),
    ("information_schema", "domain_udt_usage"),
    ("information_schema", "enabled_roles"),
    ("information_schema", "information_schema_catalog_name"),
    ("information_schema", "routine_column_usage"),
    ("information_schema", "routine_routine_usage"),
    ("information_schema", "routine_sequence_usage"),
    ("information_schema", "routine_table_usage"),
    ("information_schema", "triggered_update_columns"),
    ("information_schema", "transforms"),
    ("information_schema", "view_column_usage"),
    ("information_schema", "view_routine_usage"),
    ("information_schema", "view_table_usage"),
    // The most-queried introspection views. They derive from pg_class /
    // pg_attribute / pg_namespace, so register_user_relation writes only those base
    // tables and the lazy mechanism wraps only those base tables; these views
    // re-derive their rows rather than being shadowed by a frozen table.
    ("information_schema", "tables"),
    ("information_schema", "columns"),
    ("information_schema", "schemata"),
    ("information_schema", "views"),
    ("information_schema", "check_constraints"),
    ("information_schema", "parameters"),
    ("information_schema", "element_types"),
    ("pg_catalog", "pg_roles"),
    ("pg_catalog", "pg_user"),
    ("information_schema", "_pg_foreign_data_wrappers"),
    ("information_schema", "_pg_foreign_servers"),
    ("information_schema", "_pg_foreign_table_columns"),
    ("information_schema", "_pg_foreign_tables"),
    ("pg_catalog", "pg_sequences"),
    ("information_schema", "_pg_user_mappings"),
    ("information_schema", "administrable_role_authorizations"),
    ("information_schema", "applicable_roles"),
    ("information_schema", "column_options"),
    ("information_schema", "column_privileges"),
    ("information_schema", "data_type_privileges"),
    ("information_schema", "foreign_data_wrapper_options"),
    ("information_schema", "foreign_data_wrappers"),
    ("information_schema", "foreign_server_options"),
    ("information_schema", "foreign_servers"),
    ("information_schema", "foreign_table_options"),
    ("information_schema", "foreign_tables"),
    ("information_schema", "role_column_grants"),
    ("information_schema", "role_routine_grants"),
    ("information_schema", "role_table_grants"),
    ("information_schema", "role_udt_grants"),
    ("information_schema", "role_usage_grants"),
    ("information_schema", "routine_privileges"),
    ("information_schema", "table_privileges"),
    ("information_schema", "udt_privileges"),
    ("information_schema", "usage_privileges"),
    ("information_schema", "user_mapping_options"),
    ("information_schema", "user_mappings"),
    ("pg_catalog", "pg_rules"),
    ("pg_catalog", "pg_stat_sys_indexes"),
    ("pg_catalog", "pg_stat_sys_tables"),
    ("pg_catalog", "pg_stat_user_indexes"),
    ("pg_catalog", "pg_stat_user_tables"),
    ("pg_catalog", "pg_stat_xact_sys_tables"),
    ("pg_catalog", "pg_stat_xact_user_tables"),
    ("pg_catalog", "pg_statio_sys_indexes"),
    ("pg_catalog", "pg_statio_sys_sequences"),
    ("pg_catalog", "pg_statio_sys_tables"),
    ("pg_catalog", "pg_statio_user_indexes"),
    ("pg_catalog", "pg_statio_user_sequences"),
    ("pg_catalog", "pg_statio_user_tables"),
    ("pg_catalog", "pg_user_mappings"),
    // Runtime-function-backed views, now plannable because their statistics /
    // live-state functions are registered (resolver-backed, empty by default).
    ("pg_catalog", "pg_stat_all_tables"),
    ("pg_catalog", "pg_stat_all_indexes"),
    ("pg_catalog", "pg_stat_xact_all_tables"),
    ("pg_catalog", "pg_statio_all_indexes"),
    ("pg_catalog", "pg_statio_all_sequences"),
    ("pg_catalog", "pg_stat_database"),
    ("pg_catalog", "pg_stat_database_conflicts"),
    ("pg_catalog", "pg_stat_user_functions"),
    ("pg_catalog", "pg_stat_xact_user_functions"),
    ("pg_catalog", "pg_stat_activity"),
    ("pg_catalog", "pg_stat_replication"),
    ("pg_catalog", "pg_stat_gssapi"),
    ("pg_catalog", "pg_stat_ssl"),
    ("pg_catalog", "pg_stat_io"),
    ("pg_catalog", "pg_stat_slru"),
    ("pg_catalog", "pg_stat_subscription"),
    ("pg_catalog", "pg_stat_recovery_prefetch"),
    ("pg_catalog", "pg_stat_progress_analyze"),
    ("pg_catalog", "pg_stat_progress_basebackup"),
    ("pg_catalog", "pg_stat_progress_cluster"),
    ("pg_catalog", "pg_stat_progress_copy"),
    ("pg_catalog", "pg_stat_progress_vacuum"),
    ("pg_catalog", "pg_locks"),
    ("pg_catalog", "pg_cursors"),
    ("pg_catalog", "pg_prepared_statements"),
    ("pg_catalog", "pg_prepared_xacts"),
    ("pg_catalog", "pg_file_settings"),
    ("pg_catalog", "pg_wait_events"),
    ("pg_catalog", "pg_backend_memory_contexts"),
    ("pg_catalog", "pg_shmem_allocations"),
    ("pg_catalog", "pg_replication_slots"),
    ("pg_catalog", "pg_replication_origin_status"),
    ("information_schema", "user_mapping_options"),
    // Views unblocked by the record-returning, visibility-predicate, and remaining
    // scalar runtime functions (resolver-backed, empty by default).
    ("pg_catalog", "pg_stat_archiver"),
    ("pg_catalog", "pg_stat_wal"),
    ("pg_catalog", "pg_stat_wal_receiver"),
    ("pg_catalog", "pg_stat_replication_slots"),
    ("pg_catalog", "pg_stat_subscription_stats"),
    ("pg_catalog", "pg_stat_bgwriter"),
    ("pg_catalog", "pg_stat_checkpointer"),
    ("pg_catalog", "pg_stat_progress_create_index"),
    ("pg_catalog", "pg_seclabels"),
    ("pg_catalog", "pg_publication_tables"),
    ("pg_catalog", "pg_stats_ext"),
    ("pg_catalog", "pg_stats_ext_exprs"),
    // Unblocked by rewriting catalog `anyarray` / `name[]` casts to their text types
    // (pg_user_mappings and user_mapping_options, already listed above, are unblocked
    // by the same rewrite).
    ("pg_catalog", "pg_stats"),
    ("pg_catalog", "pg_policies"),
    // Unblocked by not injecting a GROUP BY for a bare `IS NOT NULL` projection, and by
    // renaming the backing table function so it no longer shadows the view's name.
    ("pg_catalog", "pg_available_extension_versions"),
    // Unblocked by decorrelating its correlated LATERAL aggregate joins.
    ("pg_catalog", "pg_statio_all_tables"),
    // Served via a simplified body (see SIMPLIFIED_VIEW_BODIES) - the declared bodies use
    // engine features not yet supported, but over empty/aliasable sources the simplified
    // form is equivalent.
    ("pg_catalog", "pg_group"),
    ("pg_catalog", "pg_publication_tables"),
    ("pg_catalog", "pg_stats_ext"),
    ("pg_catalog", "pg_stats_ext_exprs"),
];

/// Whether to attempt registering a declared view as a `CREATE VIEW`.
///
/// Only views on the `VIEWS_TO_REGISTER` list are attempted. Every declared view
/// is on it, so in practice every view is served live; the list is the record of
/// that commitment rather than a subset. A listed view whose body fails to plan
/// fails startup in [`create_registered_views`] -- it is not substituted.
fn should_attempt_as_view(schema_name: &str, table_name: &str, is_view: bool) -> bool {
    is_view
        && VIEWS_TO_REGISTER
            .iter()
            .any(|(schema, table)| *schema == schema_name && *table == table_name)
}

/// Wrap `ident` in double quotes for use in generated SQL, doubling any quote it
/// contains so a catalog name can never break out of the identifier.
fn quote_identifier(ident: &str) -> String {
    let escaped = ident.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

/// Wrap `value` in single quotes for use in generated SQL, doubling any quote it
/// contains so the literal cannot terminate early.
fn quote_literal(value: &str) -> String {
    let escaped = value.replace('\'', "''");
    format!("'{escaped}'")
}

/// Render `catalog.schema.table` with every part quoted, the form a generated
/// `CREATE VIEW` needs so the object lands in its own schema regardless of the
/// session's current default.
fn format_fully_qualified_name(catalog: &str, schema: &str, table: &str) -> String {
    format!(
        "{}.{}.{}",
        quote_identifier(catalog),
        quote_identifier(schema),
        quote_identifier(table)
    )
}

/// Strip surrounding whitespace and the trailing statement terminator from a
/// declared view body, so it can be spliced after `CREATE OR REPLACE VIEW ... AS`
/// without leaving a stray semicolon mid-statement.
fn normalize_view_sql(sql: &str) -> String {
    let trimmed = sql.trim();
    let without_semicolon = trimmed.trim_end_matches(';').trim();
    without_semicolon.to_string()
}

/// Point `ctx` at `schema` for unqualified name resolution.
///
/// View bodies are planned under `pg_catalog` regardless of which schema the view
/// itself lives in, so view creation and replay both move this setting and put it
/// back afterwards.
///
/// # Errors
///
/// Returns an error if the `SET` statement fails to plan or execute.
async fn set_default_schema(ctx: &SessionContext, schema: &str) -> datafusion::error::Result<()> {
    let stmt = format!(
        "SET datafusion.catalog.default_schema = {}",
        quote_literal(schema)
    );
    ctx.sql(&stmt).await?.collect().await?;
    Ok(())
}

/// Return `batch` with every column named in `name_map` renamed to its mapped
/// name, reusing the original column data.
///
/// Used to undo the aliases the rewrite passes introduce, so the client sees the
/// column names its own query asked for.
///
/// # Panics
///
/// Panics if the renamed schema does not accept the original columns, which
/// cannot happen here: only names change, never the column types or count.
fn rename_columns(batch: &RecordBatch, name_map: &HashMap<String, String>) -> RecordBatch {
    let new_fields = batch
        .schema()
        .fields()
        .iter()
        .map(|old_field| {
            let new_name = name_map
                .get(old_field.name())
                .map_or_else(|| old_field.name().as_str(), std::string::String::as_str);
            Field::new(
                new_name,
                old_field.data_type().clone(),
                old_field.is_nullable(),
            )
        })
        .collect::<Vec<_>>();

    let new_schema = std::sync::Arc::new(Schema::new(new_fields));
    RecordBatch::try_new(new_schema, batch.columns().to_vec()).unwrap()
}

/// Remove system columns from `batches` if they were not explicitly referenced
/// in the original SQL statement. `PostgreSQL` exposes virtual system columns
/// like `xmin` and `ctid` which are hidden from `SELECT *` results. We emulate
/// this behaviour by checking if the SQL contains the column name. If not,
/// the column is pruned from the result batches and schema.
fn remove_virtual_system_columns(
    sql: &str,
    batches: Vec<RecordBatch>,
    schema: Arc<Schema>,
) -> (Vec<RecordBatch>, Arc<Schema>) {
    let lowered = sql.to_lowercase();
    let system_cols = ["xmin", "xmax", "ctid", "tableoid", "cmin", "cmax"];

    let mut indices: Vec<usize> = Vec::new();
    for (i, field) in schema.fields().iter().enumerate() {
        let name = field.name().to_lowercase();
        if !system_cols.contains(&name.as_str()) || lowered.contains(&name) {
            indices.push(i);
        }
    }

    if indices.len() == schema.fields().len() {
        return (batches, schema);
    }

    let fields = indices
        .iter()
        .map(|i| schema.field(*i).clone())
        .collect::<Vec<_>>();
    let new_schema = Arc::new(Schema::new(fields));

    let new_batches = batches
        .into_iter()
        .map(|b| b.project(&indices).unwrap())
        .collect();

    (new_batches, new_schema)
}

/// Log each bound statement parameter at debug level.
///
/// The wire protocol delivers parameters as opaque big-endian bytes, so this
/// guesses a readable rendering from the width - 4 bytes as `u32`, 8 as `u64` -
/// and falls back to the raw bytes. It is a debugging aid only: nothing here
/// feeds the values that are actually bound to the query.
pub fn print_params(params: &[Option<Bytes>]) {
    for (i, param) in params.iter().enumerate() {
        let Some(bytes) = param else {
            log::debug!("param[{i}] is NULL");
            continue;
        };
        if let Ok(four) = <[u8; 4]>::try_from(&bytes[..]) {
            let v = u32::from_be_bytes(four);
            log::debug!("param[{i}] as u32: {v}");
        } else if let Ok(eight) = <[u8; 8]>::try_from(&bytes[..]) {
            let v = u64::from_be_bytes(eight);
            log::debug!("param[{i}] as u64: {v}");
        } else {
            log::debug!(
                "param[{}] raw bytes ({} bytes): {:?}",
                i,
                bytes.len(),
                bytes
            );
        }
    }
}

/// Run the input SQL through all available rewrite passes and return
/// the transformed query together with any alias mappings produced.
///
/// The alias map is `generated alias -> original column name`: one pass names the
/// unnamed top-level projections so later passes can address them, and the caller
/// uses the map to put the client's own column names back on the result.
///
/// The pass order is load-bearing; each pass that depends on another running
/// first says so at its call site below.
///
/// # Errors
///
/// Returns an error if a pass cannot parse the SQL it is handed - the passes
/// parse with sqlparser's `PostgreSQL` dialect, so a statement that dialect
/// rejects fails here rather than reaching the planner.
///
/// # Panics
///
/// Panics if the array-subquery or brace-array-literal pass fails, which means
/// the SQL those two passes were handed no longer parses even though the passes
/// before them accepted it.
pub fn rewrite_filters(sql: &str) -> datafusion::error::Result<(String, HashMap<String, String>)> {
    let sql = replace_set_command_with_namespace(sql)?;
    let sql = strip_default_collate(&sql)?;
    let sql = rewrite_time_zone_utc(&sql)?;
    let sql = rewrite_regoper_cast(&sql)?;
    let sql = rewrite_regoperator_cast(&sql)?;
    let sql = rewrite_regprocedure_cast(&sql)?;
    let sql = rewrite_regproc_cast(&sql)?;
    let sql = rewrite_available_updates(&sql)?;
    let sql = rewrite_array_subquery(&sql).unwrap();
    let sql = rewrite_brace_array_literal(&sql).unwrap();
    let sql = rewrite_pg_custom_operator(&sql)?;
    let sql = rewrite_schema_qualified_text(&sql)?;
    let sql = rewrite_schema_qualified_custom_types(&sql)?;
    // Expand `_pg_truetypid(a.*, t.*)` whole-row args into the columns those
    // functions read, so DataFusion can bind them (it has no composite type).
    let sql = rewrite_pg_truetypid_composite_args(&sql)?;
    let sql = rewrite_information_schema_casts(&sql)?;
    let sql = rewrite_schema_qualified_udtfs(&sql)?;
    let sql = rewrite_available_extension_versions_source(&sql)?;
    let sql = rewrite_char_cast(&sql)?;
    let sql = rewrite_array_upper_to_array_length(&sql)?;
    let sql = replace_regclass(&sql)?;
    let sql = rewrite_regtype_cast(&sql)?;
    let sql = rewrite_xid_cast(&sql)?;
    let sql = rewrite_name_cast(&sql)?;
    // Map catalog type names the planner rejects (`anyarray`, `name[]`) to the
    // concrete text types this catalog stores those columns as.
    let sql = rewrite_text_backed_type_casts(&sql)?;
    let sql = rewrite_oid_cast(&sql)?;
    // Resolve the function-name columns PostgreSQL types as `regproc` (`typreceive`,
    // `amhandler`, ...) to OIDs where a query compares them against one, e.g.
    // `JOIN pg_proc ON pg_proc.oid = a.typreceive`.
    let sql = resolve_regproc_columns_to_oids_in_comparisons(&sql)?;
    // Turn `<bool-column> = 't'` (the ODBC SQLPrimaryKeys form) into
    // `<bool-column> = true`; DataFusion does not coerce Boolean = Utf8.
    let sql = rewrite_boolean_column_char_comparisons(&sql)?;
    // Drop value-preserving `::regclass` / `::oid` casts on column expressions
    // (e.g. `c.oid::regclass`, `proargtypes::oid`).
    let sql = drop_redundant_oid_and_regclass_casts(&sql)?;
    // Drop `::oid[]` array casts (planner can't take a bare `oid` element type;
    // the underlying columns are already integer arrays in this catalog).
    let sql = drop_oid_array_cast(&sql)?;
    let sql = rewrite_array_agg_varchar_cast(&sql)?;
    let sql = rewrite_tuple_equality(&sql)?;
    // Turn correlated `LEFT JOIN LATERAL (SELECT agg(...) WHERE inner.k = outer.c) ON true`
    // into a grouped equi-join the planner can handle (DataFusion has no physical plan for
    // the correlated reference such a LATERAL aggregate leaves behind).
    let sql = decorrelate_lateral_aggregate(&sql)?;
    // Sort by the selected column when ORDER BY names one, before the passes below
    // start renaming SELECT list entries.
    let sql = resolve_order_by_names_to_output_positions(&sql)?;
    let sql = alias_subquery_tables(&sql)?;
    // Give duplicate column names in nested projections distinct aliases so
    // DataFusion's optimizer doesn't hit its name-mismatch assertion (e.g. the
    // two `nspname`s in constraint_column_usage's derived table).
    let sql = disambiguate_duplicate_columns(&sql)?;
    let (sql, aliases) = alias_unnamed_columns(&sql)?;
    let sql = rewrite_subquery_as_cte(&sql);

    log::debug!("before group by {sql}");
    let sql = rewrite_group_by_for_any(&sql);

    Ok((sql, aliases))
}

/// Plan `sql` on `ctx`, run it, and rename the result columns per `aliases`.
///
/// `scalars`, when present, are bound as the query's positional parameters.
/// Returns the collected batches and the renamed Arrow schema. Shared by the
/// native attempt and the correlated-subquery UDF fallback in
/// [`rewrite_and_execute_sql`].
async fn plan_collect_and_rename(
    ctx: &SessionContext,
    sql: &str,
    scalars: Option<Vec<ScalarValue>>,
    aliases: &HashMap<String, String>,
) -> datafusion::error::Result<(Vec<RecordBatch>, Arc<Schema>)> {
    let df = match scalars {
        Some(scalars) => ctx.sql(sql).await?.with_param_values(scalars)?,
        None => ctx.sql(sql).await?,
    };

    let renamed_fields = df
        .schema()
        .fields()
        .iter()
        .map(|f| {
            let new_name = aliases
                .get(f.name())
                .map_or_else(|| f.name().as_str(), std::string::String::as_str);
            Field::new(new_name, f.data_type().clone(), f.is_nullable())
        })
        .collect::<Vec<_>>();
    let schema = Arc::new(Schema::new(renamed_fields));

    let results = df.collect().await?;
    let results = results
        .iter()
        .map(|batch| rename_columns(batch, aliases))
        .collect::<Vec<_>>();
    Ok((results, schema))
}

/// Rewrite a client's `PostgreSQL` statement into SQL `DataFusion` can plan, run
/// it on `ctx`, and return the result batches with the schema the client expects.
///
/// `param_values` are the raw wire-format bytes of the bound parameters and
/// `param_types` their declared `PostgreSQL` types; both must be present for any
/// binding to happen, since a value cannot be decoded without its type. The
/// result has `PostgreSQL`'s virtual system columns (`xmin`, `ctid`, ...) pruned
/// unless the statement named one, matching what a real server returns for
/// `SELECT *`.
///
/// # Errors
///
/// Returns an error if a rewrite pass cannot parse the statement, or if planning,
/// parameter binding, or execution fails.
///
/// # Panics
///
/// Panics if a bound parameter cannot be decoded as its declared type: a
/// fixed-width integer type whose value is not that many bytes wide, a text type
/// whose bytes are not UTF-8, or a `PostgreSQL` type this crate has no mapping
/// for yet. These are protocol-level mismatches from the client, not query errors.
pub async fn rewrite_and_execute_sql(
    ctx: &SessionContext,
    sql: &str,
    param_values: Option<Vec<Option<Bytes>>>,
    param_types: Option<Vec<Type>>,
) -> datafusion::error::Result<(Vec<RecordBatch>, Arc<Schema>)> {
    log::debug!("input sql {sql:?}");

    // A correlated scalar subquery with `LIMIT 1` decorrelates into a plan that
    // limits the whole subquery relation rather than each outer row, silently
    // returning NULL instead of the matching value. Turning it into a `max`
    // aggregate keeps the meaning and decorrelates correctly. Runs on the
    // client's SQL before the passes below reshape it, so the shape it matches
    // is the one the client actually sent.
    let sql = rewrite_correlated_limit_one_subquery_to_max(sql)?;

    // Turn `(srf(x)).field` set-returning-function projections into an
    // `unnest(List<Struct>)` form DataFusion can plan. Runs BEFORE rewrite_filters
    // so the resulting `__srf_unnest['field']` access is in place before the
    // group-by-injection heuristic there inspects the projection.
    let sql = rewrite_srf_to_unnest(&sql)?;

    let (sql, aliases) = rewrite_filters(&sql)?;

    // Turn a correlated boolean scalar subquery used as a predicate
    // (getTypeInfo's `... OR (SELECT c.relkind = 'c' FROM pg_class c WHERE
    // c.oid = t.typrelid)`) into an equivalent EXISTS, so the pass below can
    // reduce it to a count DataFusion plans. Must run before rewrite_exists_to_count.
    let sql = rewrite_boolean_scalar_subquery_to_exists(&sql)?;

    // DataFusion 54 decorrelates correlated subqueries natively; the one gap is
    // `EXISTS` used as a scalar value (e.g. inside CASE), which we convert to a
    // `(SELECT count(*) ...) > 0` scalar subquery it can plan.
    let sql = rewrite_exists_to_count(&sql)?;

    // Multi-column `(...) IN (SELECT ... )` -> correlated `EXISTS` (DataFusion
    // can't plan multi-column IN). Runs AFTER rewrite_exists_to_count so the
    // EXISTS it emits stays a native WHERE predicate.
    let sql = rewrite_tuple_in_subquery_to_exists(&sql)?;

    let scalars: Option<Vec<ScalarValue>> = if let (Some(params), Some(types)) =
        (param_values, param_types)
    {
        log::debug!("params {params:?}");
        print_params(&params);

        let mut scalars = Vec::new();
        for (param, typ) in params.into_iter().zip(types) {
            let value = match (param, typ) {
                (Some(bytes), Type::INT2) => {
                    let v = i16::from_be_bytes(bytes[..].try_into().unwrap());
                    ScalarValue::Int16(Some(v))
                }
                (Some(bytes), Type::INT8) => {
                    let v = i64::from_be_bytes(bytes[..].try_into().unwrap());
                    ScalarValue::Int64(Some(v))
                }
                (Some(bytes), Type::INT4) => {
                    let v = i32::from_be_bytes(bytes[..].try_into().unwrap());
                    ScalarValue::Int32(Some(v))
                }
                (Some(bytes), Type::OID) => {
                    // OID values are 32-bit unsigned integers. We map them to
                    // BIGINT to align with `rewrite_oid_cast`, which rewrites
                    // `::oid` casts on parameters to BIGINT.
                    let v = u32::from_be_bytes(bytes[..].try_into().unwrap());
                    ScalarValue::Int64(Some(i64::from(v)))
                }
                (
                    Some(bytes),
                    Type::VARCHAR | Type::TEXT | Type::BPCHAR | Type::NAME | Type::UNKNOWN,
                ) => {
                    let s = String::from_utf8(bytes.to_vec()).unwrap();
                    ScalarValue::Utf8(Some(s))
                }
                (None, Type::INT2) => ScalarValue::Int16(None),
                // OID binds as Int64 for the same reason the non-NULL arm above
                // does, so a NULL oid must carry the same type.
                (None, Type::INT8 | Type::OID) => ScalarValue::Int64(None),
                (None, Type::INT4) => ScalarValue::Int32(None),
                (None, Type::VARCHAR | Type::TEXT | Type::BPCHAR | Type::NAME | Type::UNKNOWN) => {
                    ScalarValue::Utf8(None)
                }
                (param_bytes, unsupported_type) => {
                    panic!("unsupported param {param_bytes:?} type {unsupported_type:?}");
                }
            };
            scalars.push(value);
        }
        Some(scalars)
    } else {
        None
    };

    let (results, schema) = plan_collect_and_rename(ctx, &sql, scalars, &aliases).await?;

    let (results, schema) = remove_virtual_system_columns(&sql, results, schema);

    Ok((results, schema))
}

/// Run a client statement through [`rewrite_and_execute_sql`], logging the
/// statement, its parameters and the error when it fails.
///
/// This is the entry point a server front end should call: a failure here is
/// something a user typed, and the rewritten SQL alone is rarely enough to see
/// what went wrong.
///
/// # Errors
///
/// Returns the error from [`rewrite_and_execute_sql`] unchanged, after logging it.
pub async fn execute_sql(
    ctx: &SessionContext,
    sql: &str,
    param_values: Option<Vec<Option<Bytes>>>,
    param_types: Option<Vec<Type>>,
) -> datafusion::error::Result<(Vec<RecordBatch>, Arc<Schema>)> {
    let params_for_log = param_values.clone();
    match rewrite_and_execute_sql(ctx, sql, param_values, param_types).await {
        Ok(v) => Ok(v),
        Err(e) => {
            log::error!("exec_error query: {sql:?}");
            log::error!("exec_error params: {params_for_log:?}");
            log::error!("exec_error error: {e:?}");
            Err(e)
        }
    }
}

/// Load the catalog definition selected by `schema_path`.
///
/// `None` or an empty string selects the embedded Arrow IPC artifact; otherwise
/// the path may be a YAML file, a directory of YAML files, or a zip of them.
///
/// # Panics
///
/// Panics if `schema_path` names something that is neither a file nor a
/// directory, or if the selected catalog cannot be read or parsed. The catalog is
/// what every query resolves against, so a session built on a half-loaded one
/// would answer wrongly rather than fail.
fn parse_schema(
    schema_path: Option<&str>,
) -> HashMap<String, HashMap<String, HashMap<String, ParsedTable>>> {
    if let Some(schema_path) = schema_path {
        if schema_path.is_empty() {
            // Empty path means "use the embedded catalog" -> fast IPC artifact.
            return parse_schema_ipc_bytes(SCHEMA_IPC);
        }
        let path = Path::new(schema_path);
        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("zip") {
            parse_schema_zip(schema_path)
        } else if path.is_file() {
            parse_schema_file(schema_path)
        } else if path.is_dir() {
            parse_schema_dir(schema_path)
        } else {
            panic!("schema_path {schema_path} is neither a file nor a directory");
        }
    } else {
        // No path -> embedded catalog, loaded from the Arrow IPC artifact. This
        // skips YAML parsing + JSON->Arrow conversion (the entire ~1.85s cold
        // start); explicit file/dir/zip paths still load YAML.
        parse_schema_ipc_bytes(SCHEMA_IPC)
    }
}

/// Parse one YAML catalog file at `path` into a schema map.
///
/// # Panics
///
/// Panics if the file cannot be read or does not parse as a catalog document.
fn parse_schema_file(path: &str) -> HashMap<String, HashMap<String, HashMap<String, ParsedTable>>> {
    let contents = fs::read_to_string(path).expect("Failed to read schema file");
    parse_schema_contents(&contents)
}

/// Whether the zip entry named `entry_name` carries the file extension
/// `extension` (written without its dot), compared case-insensitively.
///
/// Archive members are matched on their extension rather than on a trailing
/// substring so a name that merely ends in those letters is not mistaken for one
/// of ours.
fn has_extension(entry_name: &str, extension: &str) -> bool {
    Path::new(entry_name)
        .extension()
        .is_some_and(|found| found.eq_ignore_ascii_case(extension))
}

/// Parse every `.yaml` entry of a zip archive into a merged schema map.
///
/// Reads each YAML member through `parse_schema_contents` and merges the
/// results with `merge_schema_maps`. Non-`.yaml` entries are skipped. The
/// archive may come from any seekable reader (a file or an in-memory buffer).
///
/// # Panics
///
/// Panics if the archive cannot be opened, an entry cannot be read, or a YAML
/// member does not parse as a catalog document.
fn parse_schema_zip_reader<R: std::io::Read + std::io::Seek>(
    reader: R,
) -> HashMap<String, HashMap<String, HashMap<String, ParsedTable>>> {
    let mut archive = ZipArchive::new(reader).expect("Failed to read zip file");
    let mut all = HashMap::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).expect("Invalid zip entry");
        if !has_extension(entry.name(), "yaml") {
            continue;
        }
        let mut contents = String::new();
        entry
            .read_to_string(&mut contents)
            .expect("Failed to read zip entry");
        let parsed = parse_schema_contents(&contents);
        merge_schema_maps(&mut all, parsed);
    }
    all
}

/// Parse the YAML schema zip located at `path` into a merged schema map.
///
/// # Panics
///
/// Panics if the file cannot be opened, or if the archive or any YAML member
/// inside it cannot be parsed.
fn parse_schema_zip(path: &str) -> HashMap<String, HashMap<String, HashMap<String, ParsedTable>>> {
    let file = fs::File::open(path).expect("Failed to open schema zip file");
    parse_schema_zip_reader(file)
}

/// Parse an in-memory YAML schema zip into a merged schema map.
///
/// # Panics
///
/// Panics if the archive or any YAML member inside it cannot be parsed.
fn parse_schema_zip_bytes(
    bytes: &[u8],
) -> HashMap<String, HashMap<String, HashMap<String, ParsedTable>>> {
    parse_schema_zip_reader(Cursor::new(bytes))
}

/// Serialize the parsed catalog into a zip of per-table Arrow IPC streams.
///
/// Each table becomes one IPC stream whose Arrow schema carries the table's
/// identity and view info under `pgcat.*` metadata keys. This is the
/// fast-loading counterpart to the YAML zip: reading it back skips YAML parsing
/// and the JSON->Arrow conversion entirely (those two dominate cold start -
/// ~1.85s vs ~9ms for everything else).
///
/// # Panics
///
/// Panics if writing the IPC streams or the zip fails. This runs only in the
/// `gen_schema_ipc` tool, where a partially written artifact is worse than a
/// failed run.
fn schemas_to_ipc_zip(
    schemas: &HashMap<String, HashMap<String, HashMap<String, ParsedTable>>>,
) -> Vec<u8> {
    use arrow::ipc::writer::StreamWriter;
    use std::io::Write as _;
    use zip::write::FileOptions;

    let mut out: Vec<u8> = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(Cursor::new(&mut out));
        let options: FileOptions<()> = FileOptions::default();
        let mut idx = 0usize;
        for (catalog, schemas) in schemas {
            for (schema_name, tables) in schemas {
                for (table, parsed) in tables {
                    let mut md = HashMap::new();
                    md.insert("pgcat.catalog".to_string(), catalog.clone());
                    md.insert("pgcat.schema".to_string(), schema_name.clone());
                    md.insert("pgcat.table".to_string(), table.clone());
                    md.insert(
                        "pgcat.is_view".to_string(),
                        if parsed.is_view { "1" } else { "0" }.to_string(),
                    );
                    if let Some(sql) = &parsed.view_sql {
                        md.insert("pgcat.view_sql".to_string(), sql.clone());
                    }
                    let meta_schema = Arc::new(Schema::new_with_metadata(
                        parsed.schema.fields().clone(),
                        md,
                    ));

                    let mut buf: Vec<u8> = Vec::new();
                    {
                        let mut writer =
                            StreamWriter::try_new(&mut buf, &meta_schema).expect("ipc writer");
                        for batch in &parsed.batches {
                            // Rebind each batch onto the metadata-carrying schema.
                            let rebound =
                                RecordBatch::try_new(meta_schema.clone(), batch.columns().to_vec())
                                    .expect("ipc batch");
                            writer.write(&rebound).expect("ipc write");
                        }
                        writer.finish().expect("ipc finish");
                    }

                    zip.start_file(format!("{idx:05}.arrow"), options)
                        .expect("zip entry");
                    zip.write_all(&buf).expect("zip write");
                    idx += 1;
                }
            }
        }
        zip.finish().expect("zip finish");
    }
    out
}

/// Load the catalog from a zip of per-table Arrow IPC streams (the fast path for
/// the embedded artifact). The inverse of [`schemas_to_ipc_zip`].
///
/// # Panics
///
/// Panics if the archive cannot be read, an entry is not a readable IPC stream,
/// or a stream is missing the `pgcat.catalog` / `pgcat.schema` / `pgcat.table`
/// metadata that says where its table belongs.
fn parse_schema_ipc_bytes(
    bytes: &[u8],
) -> HashMap<String, HashMap<String, HashMap<String, ParsedTable>>> {
    use arrow::ipc::reader::StreamReader;

    let reader = Cursor::new(bytes);
    let mut archive = ZipArchive::new(reader).expect("Failed to read ipc zip");
    let mut all: HashMap<String, HashMap<String, HashMap<String, ParsedTable>>> = HashMap::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).expect("Invalid zip entry");
        if !has_extension(entry.name(), "arrow") {
            continue;
        }
        let mut buf: Vec<u8> = Vec::new();
        entry.read_to_end(&mut buf).expect("read ipc entry");

        let mut stream = StreamReader::try_new(Cursor::new(buf), None).expect("ipc reader");
        let md_schema = stream.schema();
        let md = md_schema.metadata();
        let catalog = md
            .get("pgcat.catalog")
            .cloned()
            .expect("missing catalog md");
        let schema_name = md.get("pgcat.schema").cloned().expect("missing schema md");
        let table = md.get("pgcat.table").cloned().expect("missing table md");
        let is_view = md.get("pgcat.is_view").is_some_and(|v| v == "1");
        let view_sql = md.get("pgcat.view_sql").cloned();

        // Drop the pgcat.* metadata so the schema matches the YAML path exactly.
        let clean_schema: SchemaRef = Arc::new(Schema::new(md_schema.fields().clone()));
        let mut batches = Vec::new();
        for batch in stream.by_ref() {
            let batch = batch.expect("ipc batch");
            batches.push(
                RecordBatch::try_new(clean_schema.clone(), batch.columns().to_vec())
                    .expect("rebind batch"),
            );
        }

        let parsed = ParsedTable {
            schema: clean_schema,
            batches,
            view_sql,
            is_view,
        };
        all.entry(catalog)
            .or_default()
            .entry(schema_name)
            .or_default()
            .insert(table, parsed);
    }
    all
}

/// Build the embedded IPC catalog artifact from the YAML schema zip. Used by the
/// `gen_schema_ipc` tool to (re)generate `postgres-schema-nightly-ipc.zip`
/// whenever the YAML catalog changes.
///
/// # Panics
///
/// Panics if `yaml_zip_bytes` is not a readable zip of parseable catalog YAML, or
/// if the IPC artifact cannot be written.
#[must_use]
pub fn build_ipc_artifact(yaml_zip_bytes: &[u8]) -> Vec<u8> {
    let schemas = parse_schema_zip_bytes(yaml_zip_bytes);
    schemas_to_ipc_zip(&schemas)
}

/// Parse one YAML catalog document into a schema map, materializing every
/// declared table's rows into Arrow along the way.
///
/// # Panics
///
/// Panics if `contents` is not a valid catalog document, or if a declared table's
/// rows do not fit the schema it declares.
fn parse_schema_contents(
    contents: &str,
) -> HashMap<String, HashMap<String, HashMap<String, ParsedTable>>> {
    let parsed: YamlSchema = serde_yaml::from_str(contents).expect("Invalid YAML");
    parsed
        .0
        .into_iter()
        .map(|(catalog, schemas)| {
            let schemas = schemas
                .into_iter()
                .map(|(schema, tables)| {
                    let tables = tables
                        .into_iter()
                        .map(|(table, def)| {
                            let parsed = build_table(def);
                            (table, parsed)
                        })
                        .collect();
                    (schema, tables)
                })
                .collect();
            (catalog, schemas)
        })
        .collect()
}

/// Parse every `.yaml` file directly inside `dir_path` into one merged schema
/// map, so a catalog can be split across files by subsystem.
///
/// # Panics
///
/// Panics if the directory cannot be listed, or if any YAML file in it cannot be
/// read or parsed.
fn parse_schema_dir(
    dir_path: &str,
) -> HashMap<String, HashMap<String, HashMap<String, ParsedTable>>> {
    let mut all = HashMap::new();

    for entry in fs::read_dir(dir_path).expect("Failed to read directory") {
        let path = entry.expect("Invalid dir entry").path();
        if path.extension().and_then(|s| s.to_str()) == Some("yaml") {
            let partial = parse_schema_file(path.to_str().unwrap());

            merge_schema_maps(&mut all, partial);
        }
    }

    all
}

/// Merge the catalog map `addition` into `target`, combining them catalog by
/// catalog and schema by schema.
///
/// A table declared in both wins from `addition`, so a later-read file overrides
/// an earlier one rather than the two being rejected as a conflict.
fn merge_schema_maps(
    target: &mut HashMap<String, HashMap<String, HashMap<String, ParsedTable>>>,
    addition: HashMap<String, HashMap<String, HashMap<String, ParsedTable>>>,
) {
    for (catalog, schemas) in addition {
        let catalog_entry = target.entry(catalog).or_default();
        for (schema, tables) in schemas {
            let schema_entry = catalog_entry.entry(schema).or_default();
            schema_entry.extend(tables);
        }
    }
}

/// Turn one declared table into its Arrow schema and rows.
///
/// A `bytea` column is taken from `pg_types` rather than `schema`, because the
/// exporter writes those columns' `schema` entry as text. A `system_catalog`
/// table additionally gains `PostgreSQL`'s pseudo-columns (`xmin`, `ctid`, ...),
/// which no dump contains but clients still select by name.
///
/// # Panics
///
/// Panics if the declared rows cannot be materialized under the declared schema,
/// which means the catalog data and its column types disagree.
fn build_table(def: TableDef) -> ParsedTable {
    let TableDef {
        table_type,
        schema,
        pg_types,
        rows,
        view_sql,
    } = def;

    let mut fields: Vec<Field> = schema
        .iter()
        .map(|(col, typ)| {
            let mapped_typ = pg_types
                .as_ref()
                .and_then(|m| m.get(col))
                .map(std::string::String::as_str)
                .filter(|t| *t == "bytea")
                .unwrap_or(typ);
            Field::new(col, map_pg_type(mapped_typ), true)
        })
        .collect();

    let is_system_catalog = matches!(table_type.as_deref(), Some("system_catalog"));
    let is_view = matches!(table_type.as_deref(), Some("view"));
    let system_cols = ["xmin", "xmax", "ctid", "tableoid", "cmin", "cmax"];

    // System catalogs expose a handful of pseudo-columns (xmin, ctid, ...). Add
    // them to the field set once, up front, so both the populated and the empty
    // paths agree on the resulting schema.
    if is_system_catalog {
        for col in system_cols {
            if !fields.iter().any(|f| f.name() == col) {
                fields.push(Field::new(col, DataType::Int32, true));
            }
        }
    }

    let schema_ref = Arc::new(Schema::new(fields.clone()));

    let batches = if let Some(mut rows) = rows {
        // The pseudo-columns never appear in the YAML rows, so seed each row with
        // the historical constant (value 1) before materializing the batch.
        if is_system_catalog {
            for row in &mut rows {
                for col in system_cols {
                    row.entry(col.to_string())
                        .or_insert(serde_json::Value::from(1));
                }
            }
        }
        vec![rows_to_record_batch(&schema_ref, &rows)
            .expect("failed to build record batch from YAML-defined rows")]
    } else {
        vec![RecordBatch::new_empty(schema_ref.clone())]
    };

    ParsedTable {
        schema: schema_ref,
        batches,
        view_sql,
        is_view,
    }
}

/// Materialize `rows` into a single Arrow [`RecordBatch`] shaped by `schema`.
///
/// Each row is a `column name -> JSON value` map (the shape produced both by the
/// YAML loader and by the lazy catalog row-builders). For every field in
/// `schema` the matching value is pulled from each row (missing keys become
/// NULL) and converted according to the field's Arrow `DataType`. This is the
/// single source of truth for turning catalog rows into Arrow data, shared by
/// the static YAML path ([`build_table`]) and the lazy provider path.
///
/// # Errors
///
/// Returns an error if the built columns do not form a valid batch under
/// `schema`. One column is built per field, in order, so this means a field whose
/// Arrow type the column builders cannot produce values for.
pub fn rows_to_record_batch(
    schema: &SchemaRef,
    rows: &[BTreeMap<String, serde_json::Value>],
) -> Result<RecordBatch, DataFusionError> {
    let fields = schema.fields();
    let mut cols: Vec<Vec<serde_json::Value>> = vec![vec![]; fields.len()];
    for row in rows {
        for (i, field) in fields.iter().enumerate() {
            cols[i].push(
                row.get(field.name())
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            );
        }
    }

    let arrays = fields
        .iter()
        .zip(cols)
        .map(|(field, column_values)| json_values_to_array(field.data_type(), column_values))
        .collect::<Vec<_>>();

    RecordBatch::try_new(schema.clone(), arrays).map_err(DataFusionError::from)
}

/// Convert one column's JSON values into an Arrow array of `data_type`.
///
/// A value that does not fit the column's type becomes NULL instead of failing
/// the whole batch: these rows come from the checked-in catalog and from the lazy
/// catalog's row builders, where a single odd cell must not take an entire
/// catalog table out of service. A `data_type` with no dedicated arm falls back to
/// the value's JSON text, which is what this catalog's remaining columns hold.
fn json_values_to_array(data_type: &DataType, values: Vec<serde_json::Value>) -> ArrayRef {
    use arrow::array::{
        BinaryBuilder, BooleanArray, Float32Array, Float64Array, Int32Array, Int64Array,
        StringArray,
    };

    match data_type {
        DataType::Utf8 => Arc::new(StringArray::from(
            values
                .into_iter()
                .map(|v| v.as_str().map(std::string::ToString::to_string))
                .collect::<Vec<_>>(),
        )),
        DataType::Int32 => Arc::new(Int32Array::from(
            values.iter().map(json_value_as_i32).collect::<Vec<_>>(),
        )),
        DataType::Int64 => Arc::new(Int64Array::from(
            values.into_iter().map(|v| v.as_i64()).collect::<Vec<_>>(),
        )),
        // `as_f64` accepts both integer and float JSON numbers, so a row
        // written as `json!(0)` or `json!(410.0)` both materialize here.
        DataType::Float32 => Arc::new(Float32Array::from(
            values.iter().map(json_value_as_f32).collect::<Vec<_>>(),
        )),
        DataType::Float64 => Arc::new(Float64Array::from(
            values.into_iter().map(|v| v.as_f64()).collect::<Vec<_>>(),
        )),
        DataType::Boolean => Arc::new(BooleanArray::from(
            values.into_iter().map(|v| v.as_bool()).collect::<Vec<_>>(),
        )),
        DataType::Binary => {
            let mut builder = BinaryBuilder::new();
            for value in values {
                match value.as_str() {
                    Some(text) => builder.append_value(text.as_bytes()),
                    None => builder.append_null(),
                }
            }
            Arc::new(builder.finish())
        }
        DataType::List(inner) if inner.data_type() == &DataType::Utf8 => {
            json_values_to_utf8_list_array(values)
        }
        DataType::List(inner) if inner.data_type() == &DataType::Int64 => {
            json_values_to_int64_list_array(values)
        }
        DataType::List(inner) if inner.data_type() == &DataType::Int32 => {
            json_values_to_int32_list_array(values)
        }
        _ => Arc::new(StringArray::from(
            values
                .into_iter()
                .map(|v| Some(v.to_string()))
                .collect::<Vec<_>>(),
        )),
    }
}

/// A JSON value as an `i32`, or `None` when it is not a number or does not fit
/// `PostgreSQL`'s 32-bit integer range.
///
/// Every int4 the catalog holds (oids, attribute numbers, type lengths) is inside
/// that range, so an out-of-range value means the catalog data is wrong; NULL says
/// so, where a wrapping cast would hand out a different, plausible-looking number.
fn json_value_as_i32(value: &serde_json::Value) -> Option<i32> {
    value.as_i64().and_then(|number| i32::try_from(number).ok())
}

/// A JSON value as an `f32`, or `None` when it is not a number.
///
/// `serde_json` parses every number as `f64`, so a `float4` column has to narrow
/// here. The narrowing is the column's declared precision rather than a loss the
/// caller could avoid: the value is one `PostgreSQL` itself stores as a float4.
#[allow(clippy::cast_possible_truncation)]
fn json_value_as_f32(value: &serde_json::Value) -> Option<f32> {
    value.as_f64().map(|number| number as f32)
}

/// Build a `List<Utf8>` column from one column's JSON values.
///
/// A JSON array becomes a list of its elements, a JSON null becomes a NULL list,
/// and any other scalar becomes a one-element list holding its JSON text - the
/// form a text-array catalog column takes when the dump wrote a bare value.
fn json_values_to_utf8_list_array(values: Vec<serde_json::Value>) -> ArrayRef {
    use arrow::array::{ListBuilder, StringBuilder};

    let mut builder = ListBuilder::new(StringBuilder::new());
    for value in values {
        if let Some(items) = value.as_array() {
            for item in items {
                match item.as_str() {
                    Some(text) => builder.values().append_value(text),
                    None => builder.values().append_null(),
                }
            }
            builder.append(true);
        } else if value.is_null() {
            builder.append(false);
        } else {
            builder.values().append_value(value.to_string());
            builder.append(true);
        }
    }
    Arc::new(builder.finish())
}

/// Build a `List<Int64>` column from one column's JSON values.
///
/// A JSON array becomes a list of its elements. A string is read as a
/// whitespace-separated vector, which is how `PostgreSQL` renders `int2vector` /
/// `oidvector` columns such as `pg_index.indkey`. Anything else - a JSON null or a
/// scalar that is not an array or a string - becomes a NULL list.
fn json_values_to_int64_list_array(values: Vec<serde_json::Value>) -> ArrayRef {
    use arrow::array::{Int64Builder, ListBuilder};

    let mut builder = ListBuilder::new(Int64Builder::new());
    for value in values {
        if let Some(items) = value.as_array() {
            for item in items {
                match item.as_i64() {
                    Some(number) => builder.values().append_value(number),
                    None => builder.values().append_null(),
                }
            }
            builder.append(true);
        } else if let Some(text) = value.as_str() {
            for part in text.split_whitespace() {
                match part.parse::<i64>() {
                    Ok(number) => builder.values().append_value(number),
                    Err(_) => builder.values().append_null(),
                }
            }
            builder.append(true);
        } else {
            builder.append(false);
        }
    }
    Arc::new(builder.finish())
}

/// Build a `List<Int32>` column from one column's JSON values.
///
/// The `List<Int64>` rules apply (see [`json_values_to_int64_list_array`]), with
/// each element additionally range-checked by [`json_value_as_i32`].
fn json_values_to_int32_list_array(values: Vec<serde_json::Value>) -> ArrayRef {
    use arrow::array::{Int32Builder, ListBuilder};

    let mut builder = ListBuilder::new(Int32Builder::new());
    for value in values {
        if let Some(items) = value.as_array() {
            for item in items {
                match json_value_as_i32(item) {
                    Some(number) => builder.values().append_value(number),
                    None => builder.values().append_null(),
                }
            }
            builder.append(true);
        } else if let Some(text) = value.as_str() {
            for part in text.split_whitespace() {
                match part.parse::<i32>() {
                    Ok(number) => builder.values().append_value(number),
                    Err(_) => builder.values().append_null(),
                }
            }
            builder.append(true);
        } else {
            builder.append(false);
        }
    }
    Arc::new(builder.finish())
}

/// Register every parsed table on `ctx` and return the declared views, which the
/// caller creates once all base tables exist.
///
/// Tables become [`ScanRecordingMemTable`]s writing into `log`, so a caller can
/// see which catalog tables a query actually scanned. The catalog named `public`
/// is the database name the catalog was exported under, so its schemas are
/// registered under `default_catalog` instead - that is what makes the exported
/// catalog answer under whatever database name this session serves.
///
/// # Errors
///
/// Returns an error if a table cannot be registered under its schema, which means
/// two tables in the parsed catalog claim the same name.
fn register_catalogs_from_schemas(
    ctx: &SessionContext,
    schemas: HashMap<String, HashMap<String, HashMap<String, ParsedTable>>>,
    default_catalog: &str,
    log: &Arc<Mutex<Vec<ScanTrace>>>,
) -> datafusion::error::Result<Vec<ViewToRegister>, DataFusionError> {
    let mut views_to_register: Vec<ViewToRegister> = Vec::new();

    for (catalog_name, schemas) in schemas {
        // "public" is the *database* name we used in exports
        // so we copy the schema/tables under that database to default_catalog/database
        let current_catalog = if catalog_name == "public" {
            default_catalog.to_string()
        } else {
            catalog_name.clone()
        };

        let catalog_provider = if let Some(catalog_provider) = ctx.catalog(&current_catalog) {
            catalog_provider
        } else {
            let catalog_provider = Arc::new(MemoryCatalogProvider::new());
            ctx.register_catalog(&current_catalog, catalog_provider.clone());
            catalog_provider
        };

        for (schema_name, tables) in schemas {
            let schema_provider =
                if let Some(schema_provider) = catalog_provider.schema(&schema_name) {
                    schema_provider
                } else {
                    Arc::new(MemorySchemaProvider::new())
                };

            let _ = catalog_provider.register_schema(&schema_name, schema_provider.clone());
            log::debug!("catalog/database: {current_catalog:?} schema: {schema_name:?}");

            for (table, table_info) in tables {
                let ParsedTable {
                    schema: schema_ref,
                    batches,
                    view_sql,
                    is_view,
                } = table_info;

                if should_attempt_as_view(schema_name.as_str(), table.as_str(), is_view) {
                    if let Some(sql) = view_sql {
                        // Defer to create_registered_views, which creates these in
                        // dependency order. A body that will not plan fails startup
                        // there rather than being substituted by anything.
                        views_to_register.push(ViewToRegister {
                            catalog: current_catalog.clone(),
                            schema: schema_name.clone(),
                            name: table.clone(),
                            sql,
                        });
                        continue;
                    }
                    log::warn!(
                        "view_sql missing for view {schema_name}.{table}; registering its snapshot as a table"
                    );
                }

                let table_name = table.clone();
                log::debug!("-- table {:?}", &table);

                let base =
                    ScanRecordingMemTable::new(table_name, schema_ref, Arc::clone(log), batches);
                let provider: Arc<dyn datafusion::datasource::TableProvider> = Arc::new(base);
                schema_provider.register_table(table, provider)?;
            }
        }
    }

    Ok(views_to_register)
}

/// View bodies served in place of the declared `view_sql` for views the engine cannot yet
/// plan as written. Each is an equivalent the engine *can* plan: it keeps every column and
/// every row the engine can compute and substitutes NULL only where an unsupported feature
/// (a correlated `UNNEST`, composite-record field access, or the `VARIADIC ARRAY[...]` the
/// parser rejects) would otherwise sit. None is lossy for the data this catalog holds:
/// `pg_publication`, `pg_statistic_ext` and `pg_statistic_ext_data` are empty, and the
/// `pg_group` body is the declared query with table aliases the engine resolves. When the
/// engine gains the missing feature, drop the entry and the declared `view_sql` serves again.
const SIMPLIFIED_VIEW_BODIES: &[(&str, &str, &str)] = &[
    // The declared body's correlated `pg_authid.oid` only resolves when the outer table
    // carries an explicit alias; this is that query, aliased (still 15 groups, real grolist).
    (
        "pg_catalog",
        "pg_group",
        "SELECT a.rolname AS groname, a.oid AS grosysid, \
           ARRAY(SELECT m.member FROM pg_auth_members m WHERE m.roleid = a.oid) AS grolist \
         FROM pg_authid a WHERE NOT a.rolcanlogin",
    ),
    // Declared body uses `LATERAL pg_get_publication_tables(VARIADIC ARRAY[...])`, which the
    // parser rejects; pg_publication is empty, so projecting its columns is equivalent.
    (
        "pg_catalog",
        "pg_publication_tables",
        "SELECT p.pubname, NULL::text AS schemaname, NULL::text AS tablename, \
           NULL::text[] AS attnames, NULL::text AS rowfilter FROM pg_publication p",
    ),
    // Declared body needs correlated `UNNEST(stxkeys)` and `pg_mcv_list_items`;
    // pg_statistic_ext is empty, so engine-supported base columns plus NULL extended-stats
    // columns match.
    (
        "pg_catalog",
        "pg_stats_ext",
        "SELECT cn.nspname AS schemaname, c.relname AS tablename, \
           sn.nspname AS statistics_schemaname, s.stxname AS statistics_name, \
           pg_get_userbyid(s.stxowner) AS statistics_owner, NULL::text[] AS attnames, \
           NULL::text[] AS exprs, s.stxkind AS kinds, sd.stxdinherit AS inherited, \
           sd.stxdndistinct AS n_distinct, sd.stxddependencies AS dependencies, \
           NULL::text[] AS most_common_vals, NULL::text[] AS most_common_val_nulls, \
           NULL::text[] AS most_common_freqs, NULL::text[] AS most_common_base_freqs \
         FROM pg_statistic_ext s JOIN pg_class c ON c.oid = s.stxrelid \
           JOIN pg_statistic_ext_data sd ON s.oid = sd.stxoid \
           LEFT JOIN pg_namespace cn ON cn.oid = c.relnamespace \
           LEFT JOIN pg_namespace sn ON sn.oid = s.stxnamespace",
    ),
    // Declared body needs composite-record field access `(stat.a).stanullfrac`;
    // pg_statistic_ext is empty, so base columns plus NULL stat columns match.
    (
        "pg_catalog",
        "pg_stats_ext_exprs",
        "SELECT cn.nspname AS schemaname, c.relname AS tablename, \
           sn.nspname AS statistics_schemaname, s.stxname AS statistics_name, \
           pg_get_userbyid(s.stxowner) AS statistics_owner, NULL::text AS expr, \
           sd.stxdinherit AS inherited, NULL::float4 AS null_frac, NULL::int AS avg_width, \
           NULL::float4 AS n_distinct, NULL::text AS most_common_vals, \
           NULL::text[] AS most_common_freqs, NULL::text AS histogram_bounds, \
           NULL::float4 AS correlation, NULL::text AS most_common_elems, \
           NULL::text[] AS most_common_elem_freqs, NULL::text[] AS elem_count_histogram \
         FROM pg_statistic_ext s JOIN pg_class c ON c.oid = s.stxrelid \
           LEFT JOIN pg_statistic_ext_data sd ON s.oid = sd.stxoid \
           LEFT JOIN pg_namespace cn ON cn.oid = c.relnamespace \
           LEFT JOIN pg_namespace sn ON sn.oid = s.stxnamespace",
    ),
];

/// The simplified body to serve for a view, if one is registered (see
/// [`SIMPLIFIED_VIEW_BODIES`]).
fn simplified_view_body(schema: &str, name: &str) -> Option<&'static str> {
    SIMPLIFIED_VIEW_BODIES
        .iter()
        .find(|(s, n, _)| *s == schema && *n == name)
        .map(|(_, _, body)| *body)
}

/// Build and run the `CREATE OR REPLACE VIEW` for one declared view.
///
/// Returns `Ok(())` when the body planned and the view was registered. An `Err`
/// means the body could not be planned in the current context - either a genuine
/// engine/UDF gap or simply a view it depends on not existing yet; the caller
/// distinguishes the two by retrying (see [`create_registered_views`]).
async fn try_create_view(
    ctx: &SessionContext,
    view: &ViewToRegister,
    body_resolution_schema: &str,
) -> datafusion::error::Result<(), DataFusionError> {
    let qualified = format_fully_qualified_name(&view.catalog, &view.schema, &view.name);
    let definition = match simplified_view_body(&view.schema, &view.name) {
        Some(body) => body.to_string(),
        None => normalize_view_sql(&view.sql),
    };
    if definition.is_empty() {
        return Err(DataFusionError::Execution(format!(
            "view_sql for {qualified} is empty"
        )));
    }

    let rewritten_select = {
        // current_database() is left alone: it is a registered UDF that reads the
        // executing session's default catalog when called, so a view body can
        // call it directly rather than having a literal substituted in before
        // planning.
        let rewritten = rewrite_srf_to_unnest(&definition)?;
        // rewrite_filters aliases every unnamed top-level column to `alias_N` for
        // result-set disambiguation and returns the alias -> real-name map. A view
        // keeps its projection names as its schema, so restore the real names before
        // CREATE VIEW; otherwise the view would expose `alias_N` to its readers.
        let (rewritten, column_aliases) = rewrite_filters(&rewritten)?;
        let rewritten = rewrite_exists_to_count(&rewritten)?;
        let rewritten = rewrite_tuple_in_subquery_to_exists(&rewritten)?;
        restore_aliased_column_names(&rewritten, &column_aliases)?
    };
    let create_sql = format!("CREATE OR REPLACE VIEW {qualified} AS {rewritten_select}");
    ctx.sql(&create_sql).await?.collect().await?;

    // Record the exact statement so the view can be re-planned against later
    // provider swaps (see replan_registered_views_against_current_providers).
    registered_view_statements()
        .lock()
        .unwrap()
        .entry(ctx.session_id())
        .or_default()
        .push(RegisteredViewStatement {
            body_resolution_schema: body_resolution_schema.to_string(),
            create_sql,
        });
    Ok(())
}

/// The order catalog views are created in, one `schema.view` per line.
///
/// Generated by `cargo run --bin gen_view_order`, which brute-forces the order
/// once by retrying until every view planned and recording what worked. Doing
/// it offline keeps startup to a single pass and, unlike deriving dependencies
/// from the view bodies, cannot be wrong: the order recorded is an order that
/// provably worked.
const VIEW_CREATION_ORDER: &str = include_str!("../pg_catalog_data/view_creation_order.txt");

/// Parse [`VIEW_CREATION_ORDER`] into keys, ignoring blanks and `#` comments.
fn precomputed_view_order() -> Vec<String> {
    VIEW_CREATION_ORDER
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(std::string::ToString::to_string)
        .collect()
}

/// Build a session context and report the order its catalog views were created
/// in, for `gen_view_order` to record.
///
/// This is the discovery path: it ignores the committed order and lets the
/// retry loop find one, so the result reflects what actually plans rather than
/// what a dependency analysis predicts.
///
/// # Errors
///
/// Returns an error if the session cannot be built, which for this path means at
/// least one declared view body never planned no matter what order was tried.
pub async fn discover_view_creation_order() -> datafusion::error::Result<Vec<String>> {
    let (_ctx, order) = build_base_session_context_inner(
        None,
        "datafusion".to_string(),
        "public".to_string(),
        None,
        ViewOrder::Discover,
    )
    .await?;
    Ok(order)
}

/// How [`create_registered_views`] decides what order to try views in.
enum ViewOrder<'a> {
    /// Follow a precomputed order, so every view's dependencies already exist
    /// by the time it is tried and one pass suffices.
    Precomputed(&'a [String]),
    /// Discover an order by trying views repeatedly until no more succeed.
    /// Only the generator behind `gen_view_order` uses this; it is what
    /// produces the precomputed order in the first place.
    Discover,
}

/// The key a view is identified by in the precomputed order: `schema.name`.
///
/// The catalog is deliberately excluded, since the same views are registered
/// under whichever catalog the session defaults to.
fn view_order_key(schema: &str, name: &str) -> String {
    format!("{schema}.{name}")
}

/// Create every declared view on `ctx` as a real `CREATE OR REPLACE VIEW` and
/// return the order they were created in.
///
/// A view body may reference another view, so `order` decides what is tried when;
/// see [`ViewOrder`]. Views are created rather than materialized so they re-derive
/// from the base tables on every query instead of serving snapshot rows.
///
/// # Errors
///
/// Returns an error if a pass creates nothing while views are still pending: the
/// remaining bodies cannot be planned at all, which is a bug in this project's own
/// catalog rather than anything a user did, so startup fails naming every view
/// involved instead of quietly serving fewer views than the catalog declares.
///
/// # Panics
///
/// Panics if the process-global statement registry's mutex is poisoned, which
/// means another thread panicked while recording or replaying view statements.
async fn create_registered_views(
    ctx: &SessionContext,
    views: Vec<ViewToRegister>,
    log: Arc<Mutex<Vec<ScanTrace>>>,
    order: ViewOrder<'_>,
) -> datafusion::error::Result<Vec<String>, DataFusionError> {
    // Catalog view bodies reference their base tables (pg_class, pg_attribute,
    // pg_constraint, ...) unqualified, and those all live in pg_catalog - so
    // resolve every view body under pg_catalog regardless of which schema the
    // view itself belongs to. Each CREATE statement is fully schema-qualified, so a
    // view still lands in its own schema (e.g. information_schema.table_constraints).
    const VIEW_BODY_RESOLUTION_SCHEMA: &str = "pg_catalog";

    if views.is_empty() {
        return Ok(Vec::new());
    }

    // Order the attempts before the loop runs. With a precomputed order each
    // view's dependencies are already in place, so the loop converges in one
    // pass; sorting by name in discovery mode keeps regeneration stable rather
    // than reshuffling with HashMap iteration order.
    let mut views = views;
    match order {
        ViewOrder::Precomputed(precomputed) => {
            let position: HashMap<&str, usize> = precomputed
                .iter()
                .enumerate()
                .map(|(index, key)| (key.as_str(), index))
                .collect();
            // A view missing from the order sorts last, where the retry net
            // still catches it; the staleness itself is reported below.
            views.sort_by_key(|view| {
                let key = view_order_key(&view.schema, &view.name);
                (
                    position.get(key.as_str()).copied().unwrap_or(usize::MAX),
                    key,
                )
            });
        }
        ViewOrder::Discover => {
            views.sort_by_key(|view| view_order_key(&view.schema, &view.name));
        }
    }

    // Start from an empty record for this session so the slot ends up holding
    // exactly this session's successfully created views, ready to replay against
    // later provider swaps.
    registered_view_statements()
        .lock()
        .unwrap()
        .remove(&ctx.session_id());

    let state = ctx.state();
    let original_default_schema = state.config_options().catalog.default_schema.clone();
    drop(state);

    // A view body may reference another declared view, so the order they are
    // tried in matters. The precomputed order puts every view after the views
    // it depends on, so a single pass creates them all. The retry that follows
    // is a backstop for a stale order file, not the mechanism: if it ever
    // creates anything, the order needs regenerating and that is reported.
    //
    // Discovery mode has no such order and relies on the retry to find one,
    // which is exactly what `gen_view_order` records.
    let result: Result<Vec<String>, DataFusionError> = async {
        set_default_schema(ctx, VIEW_BODY_RESOLUTION_SCHEMA).await?;
        let mut pending = views;
        // Every attempt costs a full rewrite and plan whether it succeeds or
        // fails, so the attempt count - not the view count - is what this
        // phase's runtime tracks.
        let mut passes = 0usize;
        let mut attempts = 0usize;
        let mut created_order = Vec::new();
        loop {
            passes += 1;
            attempts += pending.len();
            let mut still_failing = Vec::new();
            let mut created_this_pass = 0usize;
            let mut last_error: Option<(String, DataFusionError)> = None;
            for view in pending {
                match try_create_view(ctx, &view, VIEW_BODY_RESOLUTION_SCHEMA).await {
                    Ok(()) => {
                        created_this_pass += 1;
                        created_order.push(view_order_key(&view.schema, &view.name));
                    }
                    Err(err) => {
                        last_error = Some((
                            format!("{}.{}.{}", view.catalog, view.schema, view.name),
                            err,
                        ));
                        still_failing.push(view);
                    }
                }
            }
            if created_this_pass == 0 {
                // Nothing moved, so the remaining views cannot be planned at
                // all -- an ordering problem would have let at least one
                // through. These views ship with this project rather than
                // coming from users, so this is our bug and startup fails
                // naming it, instead of substituting stale snapshot rows for
                // what the view is supposed to compute.
                let detail = match last_error {
                    Some((name, err)) => format!("{name}: {err}"),
                    None => "no error recorded".to_string(),
                };
                let names: Vec<String> = still_failing
                    .iter()
                    .map(|view| format!("{}.{}", view.schema, view.name))
                    .collect();
                break Err(DataFusionError::Execution(format!(
                    "{} declared catalog view(s) could not be planned: [{}]. first error {}",
                    names.len(),
                    names.join(", "),
                    detail,
                )));
            }
            pending = still_failing;
            if pending.is_empty() {
                log::debug!("view creation: {passes} passes, {attempts} plan attempts");
                if passes > 1 && matches!(order, ViewOrder::Precomputed(_)) {
                    log::warn!(
                        "catalog views needed {passes} passes despite a precomputed order; \
                         regenerate it with `cargo run --bin gen_view_order`"
                    );
                }
                break Ok(created_order);
            }
        }
    }
    .await;

    // Restore the original default schema on every exit path; a view error takes
    // precedence over a restore error.
    let restored = set_default_schema(ctx, &original_default_schema).await;
    let created_order = result?;
    restored?;
    let _ = log;
    Ok(created_order)
}

/// Build the session every connection is cloned from: the catalog tables, the
/// `PostgreSQL` compatibility functions, and the catalog views planned over them.
///
/// `schema_path` selects the catalog definition (see [`parse_schema`]); `None`
/// uses the embedded one, which is the fast path. Returns the context together
/// with the scan-trace log its catalog tables record into.
///
/// `default_catalog` is also the database name the session serves, because
/// `current_database()` reports the session's default catalog.
///
/// # Errors
///
/// Returns an error if a compatibility function cannot be registered, or if a
/// declared catalog view cannot be planned - see [`create_registered_views`].
pub async fn get_base_session_context(
    schema_path: Option<&str>,
    default_catalog: String,
    default_schema: String,
) -> datafusion::error::Result<(SessionContext, Arc<Mutex<Vec<ScanTrace>>>)> {
    build_base_session_context(schema_path, default_catalog, default_schema, None).await
}

/// Like [`get_base_session_context`], but installs a lazy catalog `source` (over
/// `options`) **before** the catalog views are created.
///
/// This matters because the catalog's real views (every declared view whose body
/// plans - see [`create_registered_views`]) are planned during session construction
/// and bind to the table providers that exist at that moment. Registering the lazy
/// source here - before view creation - makes those views resolve against the lazy
/// providers, so they reflect the source's rows.
/// Calling [`register_lazy_catalog`] *after* `get_base_session_context` only
/// rebinds the base tables; the already-created views keep pointing at the
/// original providers and never see the lazy rows.
///
/// The resulting context serves the single database `database` names, and
/// `default_catalog` should be that same name: `current_database()` reports the
/// executing session's default catalog, so a context whose catalog and database
/// disagree serves one database's rows while naming another. Serving several
/// databases means calling this once per database.
///
/// # Errors
///
/// Returns an error if the lazy source cannot be registered, or for the same
/// reasons [`get_base_session_context`] does.
pub async fn get_base_session_context_with_lazy_catalog(
    schema_path: Option<&str>,
    default_catalog: String,
    default_schema: String,
    source: Arc<dyn LazyCatalogSource>,
    options: LazyCatalogOptions,
    database: String,
) -> datafusion::error::Result<(SessionContext, Arc<Mutex<Vec<ScanTrace>>>)> {
    build_base_session_context(
        schema_path,
        default_catalog,
        default_schema,
        Some((source, options, database)),
    )
    .await
}

/// Shared implementation behind [`get_base_session_context`] and
/// [`get_base_session_context_with_lazy_catalog`]. When `lazy_catalog` is
/// `Some`, the source is registered after the base tables are built but before
/// the catalog views are created.
///
/// # Errors
///
/// Returns an error if the session cannot be built - see
/// [`build_base_session_context_inner`].
async fn build_base_session_context(
    schema_path: Option<&str>,
    default_catalog: String,
    default_schema: String,
    lazy_catalog: Option<(Arc<dyn LazyCatalogSource>, LazyCatalogOptions, String)>,
) -> datafusion::error::Result<(SessionContext, Arc<Mutex<Vec<ScanTrace>>>)> {
    let order = precomputed_view_order();
    let (ctx, _created) = build_base_session_context_inner(
        schema_path,
        default_catalog,
        default_schema,
        lazy_catalog,
        ViewOrder::Precomputed(&order),
    )
    .await?;
    Ok(ctx)
}

/// Shared body of [`build_base_session_context`] and
/// [`discover_view_creation_order`], differing only in how view order is chosen.
///
/// Returns the context (with its scan-trace log) and the order its views were
/// created in.
///
/// # Errors
///
/// Returns an error if the catalog tables cannot be registered, if a
/// compatibility function cannot be registered, if a lazy catalog source was
/// given and fails to install, or if a declared catalog view cannot be planned.
async fn build_base_session_context_inner(
    schema_path: Option<&str>,
    default_catalog: String,
    default_schema: String,
    lazy_catalog: Option<(Arc<dyn LazyCatalogSource>, LazyCatalogOptions, String)>,
    view_order: ViewOrder<'_>,
) -> datafusion::error::Result<((SessionContext, Arc<Mutex<Vec<ScanTrace>>>), Vec<String>)> {
    let scan_traces: Arc<Mutex<Vec<ScanTrace>>> = Arc::new(Mutex::new(Vec::new()));

    // Session construction takes seconds, and which phase owns that time is not
    // obvious from the outside, so each phase reports its own elapsed time.
    let build_started = std::time::Instant::now();
    let mut phase_started = build_started;
    let mut log_phase = move |name: &str| {
        log::debug!("session build phase {name}: {:?}", phase_started.elapsed());
        phase_started = std::time::Instant::now();
    };

    let schemas = parse_schema(schema_path);
    log_phase("parse_schema");
    let mut session_config = datafusion::execution::context::SessionConfig::new()
        .with_default_catalog_and_schema(&default_catalog, &default_schema)
        .with_option_extension(ClientOpts::default());

    // This should be false, otherwise datafusion uses it's own inf schema
    session_config.options_mut().catalog.information_schema = false;

    let ctx: SessionContext = SessionContext::new_with_config(session_config);
    let pending_views =
        register_catalogs_from_schemas(&ctx, schemas, &default_catalog, &scan_traces)?;
    log_phase("register_catalogs_from_schemas");

    register_pg_compatibility_functions(&ctx).await?;
    log_phase("register_functions");

    // Install the lazy catalog providers BEFORE the views are created, so the
    // catalog views plan against the lazy providers and reflect their rows.
    if let Some((source, options, database)) = lazy_catalog {
        register_lazy_catalog(&ctx, source, options, &database).await?;
    }
    log_phase("register_lazy_catalog");

    let created_order =
        create_registered_views(&ctx, pending_views, scan_traces.clone(), view_order).await?;
    log_phase("create_registered_views");
    log::debug!("session build total: {:?}", build_started.elapsed());

    let catalogs = ctx.catalog_names();
    log::info!("registered catalogs: {catalogs:?}");

    Ok(((ctx, scan_traces), created_order))
}

/// Register the `PostgreSQL` functions this crate emulates on `ctx`.
///
/// These must all be in place before the catalog views are created: the view
/// bodies call them (`pg_get_userbyid`, `format_type`, the `pg_stat_*` runtime
/// resolvers, ...), and a body naming a function the session does not have fails
/// to plan.
///
/// # Errors
///
/// Returns an error if any registration fails, which means two functions were
/// registered under one name or a function's signature was rejected.
async fn register_pg_compatibility_functions(
    ctx: &SessionContext,
) -> datafusion::error::Result<()> {
    for f in regclass_udfs(ctx) {
        ctx.register_udf(f);
    }

    ctx.register_udtf(
        "regclass_oid",
        Arc::new(crate::user_functions::RegClassOidFunc),
    );

    register_scalar_regclass_oid(ctx)?;
    register_scalar_pg_proc_oid(ctx)?;
    register_scalar_pg_tablespace_location(ctx)?;
    register_scalar_format_type(ctx).await?;
    ctx.register_udtf(
        "regclass_oid",
        Arc::new(crate::user_functions::RegClassOidFunc),
    );

    register_current_schema(ctx)?;
    register_current_schemas(ctx)?;

    register_scalar_pg_get_expr(ctx)?;
    register_scalar_pg_get_partkeydef(ctx)?;
    register_scalar_pg_table_is_visible(ctx)?;
    register_scalar_pg_get_userbyid(ctx)?;
    register_scalar_pg_encoding_to_char(ctx)?;
    register_scalar_array_to_string(ctx)?;
    register_pg_get_one(ctx)?;
    register_pg_get_array(ctx)?;
    register_array_agg(ctx)?;
    register_pg_get_statisticsobjdef_columns(ctx)?;
    register_pg_relation_is_publishable(ctx)?;
    register_has_database_privilege(ctx)?;
    register_has_schema_privilege(ctx)?;
    register_has_privilege_family(ctx)?;
    register_nameconcatoid(ctx)?;
    register_pg_has_role(ctx)?;
    register_pg_is_other_temp_schema(ctx)?;
    register_pg_my_temp_schema(ctx)?;
    register_getdatabaseencoding(ctx)?;
    register_pg_relation_is_updatable(ctx)?;
    register_pg_sequence_last_value(ctx)?;
    register_row_security_active(ctx)?;
    register_session_identity(ctx)?;
    register_current_database(ctx)?;
    crate::runtime_function_resolvers::register_all_scalar_resolvers(ctx);
    crate::runtime_function_resolvers::register_all_table_resolvers(ctx);
    register_pg_column_is_updatable(ctx)?;
    register_pg_get_function_arg_default(ctx)?;
    register_format(ctx)?;
    register_pg_char_max_length(ctx)?;
    register_pg_char_octet_length(ctx)?;
    register_pg_index_position(ctx)?;
    register_pg_numeric_helpers(ctx)?;
    register_pg_truetypid_helpers(ctx)?;
    register_pg_options_to_table(ctx)?;
    register_pg_expandarray(ctx)?;
    register_aclexplode(ctx)?;
    register_acldefault(ctx)?;
    register_pg_postmaster_start_time(ctx)?;
    register_pg_relation_size(ctx)?;
    register_pg_total_relation_size(ctx)?;
    register_scalar_pg_age(ctx)?;
    register_scalar_pg_is_in_recovery(ctx)?;
    register_scalar_txid_current(ctx)?;
    register_quote_ident(ctx)?;
    register_translate(ctx)?;
    register_pg_available_extension_versions(ctx)?;
    register_pg_get_keywords(ctx)?;
    register_pg_get_viewdef(ctx)?;
    register_pg_get_function_arguments(ctx)?;
    register_pg_get_function_result(ctx)?;
    register_pg_get_function_sqlbody(ctx)?;
    register_pg_get_indexdef(ctx)?;
    register_pg_get_triggerdef(ctx)?;
    register_pg_get_ruledef(ctx)?;
    register_encode(ctx)?;
    register_upper(ctx)?;
    register_version_fn(ctx)?;
    Ok(())
}

/// Loader and result-shaping tests: what the catalog files turn into, and what a
/// result set looks like by the time a client sees it.
#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::ArrayRef;
    use arrow::array::{Int32Array, StringArray};
    use arrow::datatypes::DataType;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// A YAML table's columns keep their declared types and its rows keep their
    /// values through the load.
    #[test]
    fn test_parse_schema_file() {
        let yaml = r"
public:
  myschema:
    employees:
      type: table
      schema:
        id: int
        name: varchar
      rows:
        - id: 1
          name: Alice
        - id: 2
          name: Bob
";

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{yaml}").unwrap();

        let parsed = parse_schema_file(file.path().to_str().unwrap());

        let myschema = parsed.get("public").unwrap().get("myschema").unwrap();
        let table = myschema.get("employees").unwrap();
        let schema_ref = table.schema.clone();
        let batches = &table.batches;

        let fields = schema_ref.fields();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name(), "id");
        assert_eq!(fields[0].data_type(), &DataType::Int32);
        assert_eq!(fields[1].name(), "name");
        assert_eq!(fields[1].data_type(), &DataType::Utf8);

        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        assert_eq!(batch.num_rows(), 2);

        let id_array = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        assert_eq!(id_array.value(0), 1);
        assert_eq!(id_array.value(1), 2);

        let name_array = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(name_array.value(0), "Alice");
        assert_eq!(name_array.value(1), "Bob");
    }

    /// Float columns keep their values at the Arrow width their declared
    /// `PostgreSQL` type maps to, whether the YAML wrote them as floats or
    /// integers.
    #[test]
    fn test_float_column_round_trips() {
        // Regression: a float column used to map to Utf8 and its numeric values
        // silently became NULL. It must now keep its values at the matching Arrow
        // width (float4 -> Float32, double precision -> Float64), whether written
        // as a float (1.5) or an integer (3) in the YAML.
        use arrow::array::{Array, Float32Array, Float64Array};

        let yaml = r"
public:
  s:
    stats:
      type: table
      schema:
        est: float4
        ratio: double precision
      rows:
        - est: 410.0
          ratio: 3
        - est: 0.0
          ratio: 1.25
";
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{yaml}").unwrap();
        let parsed = parse_schema_file(file.path().to_str().unwrap());
        let table = parsed
            .get("public")
            .unwrap()
            .get("s")
            .unwrap()
            .get("stats")
            .unwrap();

        let fields = table.schema.fields();
        assert_eq!(fields[0].data_type(), &DataType::Float32); // float4
        assert_eq!(fields[1].data_type(), &DataType::Float64); // double precision

        let batch = &table.batches[0];
        let est = batch
            .column(0)
            .as_any()
            .downcast_ref::<Float32Array>()
            .expect("est (float4) must be Float32, not NULL/Utf8");
        let ratio = batch
            .column(1)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("ratio (double precision) must be Float64");

        // Every value here is exactly representable at its width, so the tolerance
        // only exists to keep the comparison off an exact float equality.
        assert!(est.is_valid(0) && (est.value(0) - 410.0_f32).abs() < f32::EPSILON);
        assert!(est.is_valid(1) && est.value(1).abs() < f32::EPSILON);
        // An integer literal in a float column also materializes (not NULL).
        assert!(ratio.is_valid(0) && (ratio.value(0) - 3.0).abs() < f64::EPSILON);
        assert!(ratio.is_valid(1) && (ratio.value(1) - 1.25).abs() < f64::EPSILON);
    }

    /// Renaming every column rewrites the names while reusing the original
    /// column data rather than copying it.
    #[test]
    fn test_rename_columns_all() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Utf8, true),
        ]));

        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int32Array::from(vec![1, 2])) as ArrayRef,
                Arc::new(StringArray::from(vec!["x", "y"])) as ArrayRef,
            ],
        )
        .unwrap();

        let mut map = HashMap::new();
        map.insert("a".to_string(), "alpha".to_string());
        map.insert("b".to_string(), "beta".to_string());

        let renamed = rename_columns(&batch, &map);

        assert_eq!(renamed.schema().field(0).name(), "alpha");
        assert_eq!(renamed.schema().field(1).name(), "beta");

        assert!(Arc::ptr_eq(batch.column(0), renamed.column(0)));
        assert!(Arc::ptr_eq(batch.column(1), renamed.column(1)));
    }

    /// A `_text` column loads as an Arrow list whose elements are the YAML
    /// sequence's entries.
    #[test]
    fn test_parse_schema_text_array() {
        use arrow::array::ListArray;
        let yaml = r#"
public:
  myschema:
    cfgtable:
      type: table
      schema:
        cfg: _text
      rows:
        - cfg:
            - "x"
            - "y"
"#;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, yaml.as_bytes()).unwrap();

        let parsed = parse_schema_file(file.path().to_str().unwrap());
        let myschema = parsed.get("public").unwrap().get("myschema").unwrap();
        let table = myschema.get("cfgtable").unwrap();

        let field = &table.schema.fields()[0];
        assert!(matches!(field.data_type(), DataType::List(_)));

        let batch = &table.batches[0];
        let list = batch
            .column(0)
            .as_any()
            .downcast_ref::<ListArray>()
            .unwrap();
        let binding = list.value(0);
        let inner = binding.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(inner.value(0), "x");
        assert_eq!(inner.value(1), "y");
    }

    /// A column the rename map does not mention keeps the name it had.
    #[test]
    fn test_rename_columns_partial() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("name", DataType::Utf8, false),
        ]));

        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int32Array::from(vec![1, 2, 3])) as ArrayRef,
                Arc::new(StringArray::from(vec!["Alice", "Bob", "Carol"])) as ArrayRef,
            ],
        )
        .unwrap();

        let mut map = HashMap::new();
        map.insert("name".to_string(), "username".to_string());

        let renamed = rename_columns(&batch, &map);

        assert_eq!(renamed.schema().field(0).name(), "id");
        assert_eq!(renamed.schema().field(1).name(), "username");

        assert!(Arc::ptr_eq(batch.column(0), renamed.column(0)));
        assert!(Arc::ptr_eq(batch.column(1), renamed.column(1)));
    }

    /// A system column is pruned from a `SELECT *` result but kept when the
    /// statement named it, which is what a real server does.
    #[test]
    fn test_remove_virtual_system_columns() {
        use arrow::array::Int32Array;

        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("xmin", DataType::Int32, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from(vec![1])) as ArrayRef,
                Arc::new(Int32Array::from(vec![42])) as ArrayRef,
            ],
        )
        .unwrap();

        let (out, out_schema) =
            remove_virtual_system_columns("SELECT * FROM t", vec![batch.clone()], schema.clone());
        assert_eq!(out_schema.fields().len(), 1);
        assert_eq!(out_schema.field(0).name(), "id");
        assert_eq!(out[0].num_columns(), 1);

        // when the result already only contains the system column, it should be preserved
        let xmin_schema = Arc::new(Schema::new(vec![Field::new(
            "xmin",
            DataType::Int32,
            false,
        )]));
        let xmin_batch = RecordBatch::try_new(
            xmin_schema.clone(),
            vec![Arc::new(Int32Array::from(vec![42])) as ArrayRef],
        )
        .unwrap();

        let (out, out_schema) = remove_virtual_system_columns(
            "SELECT xmin FROM t",
            vec![xmin_batch.clone()],
            xmin_schema.clone(),
        );
        assert_eq!(out_schema.fields().len(), 1);
        assert_eq!(out_schema.field(0).name(), "xmin");
        assert_eq!(out[0].num_columns(), 1);
    }
}

/// Tests that a declared view which cannot be planned fails startup loudly,
/// rather than being served as something else.
#[cfg(test)]
mod view_failure_tests {
    use super::*;
    use datafusion::prelude::SessionContext;

    /// Build a context with an empty `pg_catalog` schema to register views into.
    fn context_with_pg_catalog() -> SessionContext {
        let config = datafusion::execution::context::SessionConfig::new()
            .with_default_catalog_and_schema("datafusion", "public")
            .with_option_extension(ClientOpts::default());
        let ctx = SessionContext::new_with_config(config);
        let catalog = ctx.catalog("datafusion").expect("default catalog exists");
        catalog
            .register_schema("pg_catalog", Arc::new(MemorySchemaProvider::new()))
            .expect("register pg_catalog");
        ctx
    }

    /// A view body that cannot be planned fails startup and leaves nothing
    /// registered under the view's name.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_unplannable_view_fails_startup_instead_of_being_materialized() {
        // The regression guard for the removed fallback. A body that cannot be
        // planned used to be replaced by the view's snapshot from the embedded
        // PostgreSQL dump, which served another server's rows under this
        // server's view name. It must now fail loudly instead.
        let ctx = context_with_pg_catalog();
        let views = vec![ViewToRegister {
            catalog: "datafusion".to_string(),
            schema: "pg_catalog".to_string(),
            name: "broken_view".to_string(),
            sql: "SELECT * FROM a_relation_that_does_not_exist".to_string(),
        }];

        let log = Arc::new(Mutex::new(Vec::new()));
        let order = vec!["pg_catalog.broken_view".to_string()];
        let result =
            create_registered_views(&ctx, views, log, ViewOrder::Precomputed(&order)).await;

        let error = result.expect_err("an unplannable view must fail startup");
        let message = error.to_string();
        assert!(
            message.contains("broken_view"),
            "the error must name the offending view, got: {message}"
        );

        // And nothing may be left behind under that name: a half-registered
        // table would be exactly the silent substitution this removes.
        let registered = ctx
            .catalog("datafusion")
            .and_then(|catalog| catalog.schema("pg_catalog"))
            .expect("pg_catalog schema")
            .table_names();
        assert!(
            !registered.iter().any(|name| name == "broken_view"),
            "no table may be registered for a view that failed to plan, got {registered:?}"
        );
    }

    /// The startup failure names every view that could not be planned, not just
    /// the first one hit.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_view_creation_reports_every_unplannable_view() {
        // The error names all of them, not just the first, so one startup
        // failure is enough to see the whole problem.
        let ctx = context_with_pg_catalog();
        let views = vec![
            ViewToRegister {
                catalog: "datafusion".to_string(),
                schema: "pg_catalog".to_string(),
                name: "broken_one".to_string(),
                sql: "SELECT * FROM missing_one".to_string(),
            },
            ViewToRegister {
                catalog: "datafusion".to_string(),
                schema: "pg_catalog".to_string(),
                name: "broken_two".to_string(),
                sql: "SELECT * FROM missing_two".to_string(),
            },
        ];

        let log = Arc::new(Mutex::new(Vec::new()));
        let error = create_registered_views(&ctx, views, log, ViewOrder::Discover)
            .await
            .expect_err("unplannable views must fail startup");
        let message = error.to_string();
        assert!(
            message.contains("broken_one") && message.contains("broken_two"),
            "the error must name every failing view, got: {message}"
        );
    }
}
