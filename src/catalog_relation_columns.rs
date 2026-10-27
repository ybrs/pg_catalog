//! Which columns each relation of the embedded catalog declares.
//!
//! WHY THIS EXISTS
//! ===============
//! The SQL rewrites in [`crate::session::rewrite_filters`] are a pure
//! string -> string pipeline: no pass receives a `SessionContext`, so no pass can
//! ask the planner what a name means. That is fine for rewrites that only need
//! to recognise a syntactic shape, and not fine for the one question a
//! correlation rewrite has to answer - does this unqualified column belong to
//! the sub-query's own FROM clause, or to the query enclosing it?
//!
//! `psql`'s `\dp` writes exactly that:
//!
//! ```sql
//! FROM pg_catalog.pg_policy pol
//! WHERE ... ARRAY(SELECT rolname FROM pg_catalog.pg_roles WHERE oid = ANY (polroles))
//! ```
//!
//! `polroles` is a `pg_policy` column referenced from inside a sub-query over
//! `pg_roles`, written without a table qualifier. `PostgreSQL` resolves it by
//! looking at both relations' columns. This module is how the rewriter gets to
//! do the same.
//!
//! WHERE THE ANSWERS COME FROM
//! ===========================
//! The same embedded artifact the catalog itself is built from
//! (`pg_catalog_data/postgres-schema-nightly-ipc.zip`, generated from the YAML
//! under `pg_catalog_data/pg_schema`), so a column added to the catalog is
//! known here without a second thing to regenerate. Only each Arrow IPC
//! stream's schema message is read - the row batches behind it are never
//! decoded - so building the map costs a fraction of loading the catalog.
//!
//! WHAT IT DELIBERATELY DOES NOT COVER
//! ===================================
//! Relations the host registered at runtime: user tables, and views a lazy
//! catalog source supplies. They are not in the artifact and cannot be, since
//! this map is built without a session. A caller that cannot find a relation
//! here must therefore treat the answer as "unknown" and decline to act, never
//! as "that relation has no such column" - see
//! [`crate::scalar_to_cte`], which is the only caller today and does exactly
//! that.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::sync::OnceLock;

use arrow::ipc::reader::StreamReader;
use zip::ZipArchive;

use crate::session::SCHEMA_IPC;

/// The relation -> column-names map, built once on first use.
///
/// Keyed by lowercase relation name under both spellings a query can write:
/// qualified (`pg_catalog.pg_policy`) and bare (`pg_policy`). A bare name that
/// two schemas both declare is left out rather than resolved arbitrarily, so an
/// ambiguous name reads as "unknown" and its caller declines - see
/// [`columns_of_catalog_relation`].
static RELATION_COLUMNS: OnceLock<HashMap<String, HashSet<String>>> = OnceLock::new();

/// The column names `relation_name` declares, or `None` when the embedded
/// catalog declares no relation by that name.
///
/// `relation_name` may be written bare (`pg_policy`) or schema-qualified
/// (`pg_catalog.pg_policy`); both are matched case-insensitively, as are the
/// column names in the returned set (they are stored lowercased).
///
/// `None` means "this map does not know", NOT "no such relation exists".
/// User tables and lazily supplied views are absent by construction, so a
/// caller must treat `None` as a reason to leave the query alone.
pub(crate) fn columns_of_catalog_relation(relation_name: &str) -> Option<&'static HashSet<String>> {
    RELATION_COLUMNS
        .get_or_init(|| read_relation_columns_from_embedded_catalog(SCHEMA_IPC))
        .get(&relation_name.to_ascii_lowercase())
}

/// Read every relation's column names out of the Arrow IPC catalog artifact.
///
/// Each zip member is one relation's IPC stream, whose first message carries
/// the schema plus the `pgcat.schema` / `pgcat.table` metadata naming it. The
/// stream reader is dropped without iterating, so only that first message is
/// decompressed and the row batches are never touched.
///
/// A member that cannot be read is skipped rather than fatal: the map is an
/// optimisation for deciding whether to rewrite, and a missing relation makes
/// the caller decline, which is the same answer it gives for a user table.
/// Skipping is logged at error level even so. The map is built once and cached
/// for the life of the process, so a single unreadable member silently disables
/// every rewrite that needed that relation until restart; the log line is the
/// only way to tell that apart from a rewrite that correctly declined.
fn read_relation_columns_from_embedded_catalog(
    artifact: &[u8],
) -> HashMap<String, HashSet<String>> {
    let mut qualified: HashMap<String, HashSet<String>> = HashMap::new();
    // Each bare relation name mapped to the schemas that declare it. A name
    // two schemas both declare cannot be resolved without knowing the search
    // path, so its bare entry is dropped below.
    let mut bare_name_owners: HashMap<String, HashSet<String>> = HashMap::new();

    let mut archive = match ZipArchive::new(Cursor::new(artifact)) {
        Ok(archive) => archive,
        Err(error) => {
            log::error!(
                "embedded catalog unreadable as a zip: {error}; \
                 every column-name lookup will decline for the life of this process"
            );
            return qualified;
        }
    };
    let entry_count = archive.len();
    for index in 0..entry_count {
        let mut entry = match archive.by_index(index) {
            Ok(entry) => entry,
            Err(error) => {
                log::error!(
                    "embedded catalog entry {index} of {entry_count} unreadable: {error}; \
                     its relation is missing from the column map for the life of this process"
                );
                continue;
            }
        };
        let entry_name = entry.name().to_string();
        let stream = match StreamReader::try_new(&mut entry, None) {
            Ok(stream) => stream,
            Err(error) => {
                log::error!(
                    "embedded catalog entry {entry_name} (index {index} of {entry_count}) \
                     is not a readable IPC stream: {error}; its relation is missing from \
                     the column map for the life of this process"
                );
                continue;
            }
        };
        let schema = stream.schema();
        let metadata = schema.metadata();
        let (Some(schema_name), Some(table_name)) =
            (metadata.get("pgcat.schema"), metadata.get("pgcat.table"))
        else {
            continue;
        };

        let columns: HashSet<String> = schema
            .fields()
            .iter()
            .map(|field| field.name().to_ascii_lowercase())
            .collect();
        let schema_name = schema_name.to_ascii_lowercase();
        let table_name = table_name.to_ascii_lowercase();

        bare_name_owners
            .entry(table_name.clone())
            .or_default()
            .insert(schema_name.clone());
        qualified.insert(format!("{schema_name}.{table_name}"), columns.clone());
        qualified.insert(table_name, columns);
    }

    for (bare_name, owning_schemas) in bare_name_owners {
        if owning_schemas.len() > 1 {
            qualified.remove(&bare_name);
        }
    }

    qualified
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An artifact that is not a zip at all yields an empty map rather than a
    /// panic, because this map only decides whether a rewrite may fire and
    /// declining is always a safe answer.
    #[test]
    fn an_unreadable_artifact_yields_an_empty_map() {
        assert!(read_relation_columns_from_embedded_catalog(b"not a zip archive").is_empty());
    }

    /// A member that cannot be read is skipped, leaving the members around it
    /// in the map.
    ///
    /// This is the failure mode taskq 302 is about: the same bad read that is
    /// fatal in `parse_schema_ipc_bytes` is survivable here, and the map is
    /// cached for the life of the process, so the skip must be visible in the
    /// log rather than only in a rewrite that quietly stopped firing.
    #[test]
    fn a_member_that_cannot_be_read_is_skipped_not_fatal() {
        let mut truncated_archive = crate::session::SCHEMA_IPC.to_vec();
        // Blank the first member's compressed bytes. The central directory
        // still lists every member, so the archive opens and the surviving
        // members read normally while this one fails.
        let first_member_data = 512;
        for byte in &mut truncated_archive[first_member_data..first_member_data + 4096] {
            *byte = 0;
        }

        let columns = read_relation_columns_from_embedded_catalog(&truncated_archive);
        assert!(
            !columns.is_empty(),
            "the members that still read must survive one that does not"
        );
    }

    /// Whether `relation_name` declares `column_name`, for the assertions
    /// below; both names are matched case-insensitively.
    fn declares(relation_name: &str, column_name: &str) -> bool {
        columns_of_catalog_relation(relation_name)
            .is_some_and(|columns| columns.contains(&column_name.to_ascii_lowercase()))
    }

    /// The map answers for a relation written either way, and knows the column
    /// that motivated the module.
    #[test]
    fn a_catalog_relation_is_found_bare_and_qualified() {
        assert!(declares("pg_policy", "polroles"));
        assert!(declares("pg_catalog.pg_policy", "polroles"));
        assert!(declares("PG_POLICY", "POLROLES"));
    }

    /// A view the catalog declares is covered too, not only base tables.
    ///
    /// `pg_roles` is a view, and it is the inner relation of the `\dp`
    /// sub-query - so a map holding only base tables would leave the one case
    /// this module exists for unresolvable.
    #[test]
    fn a_catalog_view_declares_its_columns_like_a_table() {
        let columns = columns_of_catalog_relation("pg_catalog.pg_roles")
            .expect("pg_roles is declared by the embedded catalog");
        assert!(columns.contains("rolname"));
        assert!(columns.contains("oid"));
        assert!(
            !columns.contains("polroles"),
            "polroles belongs to pg_policy, not to pg_roles"
        );
    }

    /// A relation the embedded catalog does not declare answers `None`.
    ///
    /// This is the answer every user table gives, and callers rely on being
    /// able to tell it apart from a known relation that lacks the column.
    #[test]
    fn a_relation_outside_the_embedded_catalog_is_unknown() {
        assert!(columns_of_catalog_relation("orders").is_none());
        assert!(!declares("orders", "id"));
    }

    /// `information_schema` relations are in the map under their qualified name.
    #[test]
    fn information_schema_relations_are_covered() {
        let columns = columns_of_catalog_relation("information_schema.tables")
            .expect("information_schema.tables is declared by the embedded catalog");
        assert!(columns.contains("table_name"));
    }

    /// The map carries the pseudo-columns a system catalog gains, and does not
    /// invent them for a view.
    ///
    /// This is the invariant the whole module rests on: the map has to list
    /// exactly what a query can REFERENCE, not what the catalog dump declared.
    /// `build_table` in `session.rs` appends `xmin`, `ctid` and their four
    /// siblings to every `system_catalog` table and to no view, and the IPC
    /// artifact is generated through that same path - so the map inherits the
    /// distinction. Verified against a live context: `SELECT xmin FROM
    /// pg_policy` resolves and `SELECT xmin FROM pg_roles` does not.
    ///
    /// A caller reads "not declared by this relation" as "belongs to the
    /// enclosing query", so a pseudo-column missing here would make a bare
    /// `xmin` look like an outer reference.
    #[test]
    fn the_map_agrees_with_the_providers_about_pseudo_columns() {
        let system_catalog = columns_of_catalog_relation("pg_catalog.pg_policy")
            .expect("pg_policy is declared by the embedded catalog");
        let view = columns_of_catalog_relation("pg_catalog.pg_roles")
            .expect("pg_roles is declared by the embedded catalog");
        for pseudo_column in ["xmin", "xmax", "ctid", "tableoid", "cmin", "cmax"] {
            assert!(
                system_catalog.contains(pseudo_column),
                "a system catalog gains {pseudo_column}, so the map must list it"
            );
            assert!(
                !view.contains(pseudo_column),
                "a view does not gain {pseudo_column}, so the map must not list it"
            );
        }
    }
}
