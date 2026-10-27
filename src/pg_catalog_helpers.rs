//! The wire-format column vocabulary shared by everything that declares a
//! relation's columns.
//!
//! This module used to hold a second, INSERT-based registration mechanism -
//! `register_user_database` and friends, which wrote user objects straight into
//! the built-in catalog tables at startup, alongside a per-context OID counter
//! and a global registry remembering which database registered which schema.
//! That mechanism was one of TWO independent implementations of "put the
//! host's objects into the catalog", and it is gone: every host now goes
//! through one [`crate::lazy_catalog::LazyCatalogSource`], with up-front
//! declarations wrapped by [`crate::declared_catalog_source`], which also
//! carries the numbering and deduplication rules the eager path guaranteed.
//!
//! What remains here is the piece both registration styles always shared: how
//! an integration SPELLS a column - a `name -> ColumnDef` map with the type as
//! text - and how that spelling resolves to the `pg_type` OIDs and
//! `information_schema` type names the catalog serves.

use serde::Deserialize;

use crate::lazy_catalog::ColumnSpec;
use std::collections::BTreeMap;

/// One column of a user relation as the integration describes it on the wire.
///
/// The column NAME is not a field: callers pass each column as a single-entry
/// `name -> ColumnDef` map, so the name lives in the map key and this struct
/// carries only the column's properties.
#[derive(Debug, Clone, Deserialize)]
pub struct ColumnDef {
    /// Declared type text exactly as the integration writes it (`int`,
    /// `varchar(64)`, `text`, ...). Resolved to a `pg_type` OID by
    /// [`map_type_to_oid`]; unrecognized text falls back to `text`.
    #[serde(rename = "type")]
    pub col_type: String,
    /// Whether the column accepts NULL. Drives `pg_attribute.attnotnull` and the
    /// `information_schema.columns.is_nullable` flag derived from it.
    pub nullable: bool,
    /// Whether the column has a default expression. Drives `pg_attribute.atthasdef`
    /// and a `pg_attrdef` row; the default *text* is supplied at query time by the
    /// integration-supplied definition-text resolver (NULL until one is installed).
    /// Defaults to false, so existing callers and wire payloads need not set it.
    #[serde(default)]
    pub has_default: bool,
}

/// Resolve a column's declared type string (as the integration writes it, e.g.
/// `int`, `varchar(64)`, `text`) to its `PostgreSQL` `pg_type` OID. The OID is the
/// single source of truth for both `pg_attribute.atttypid` and the
/// `information_schema.columns` type names (via [`oid_to_type_names`]), so the
/// two never disagree. Unrecognized types fall back to `text` (OID 25), the
/// permissive default used across this module.
pub(crate) fn map_type_to_oid(t: &str) -> i32 {
    let lower = t.to_lowercase();
    match lower.as_str() {
        "int" | "integer" | "int4" => 23,
        "bigint" | "int8" => 20,
        "bool" | "boolean" => 16,
        other if other.starts_with("varchar") || other.starts_with("character varying") => 1043,
        _ => 25, // default to text
    }
}

/// Map a `PostgreSQL` type OID back to its `(data_type, udt_name)` pair as used in
/// `information_schema.columns`.
///
/// This is the inverse of [`map_type_to_oid`] and lets every registration style
/// render `information_schema.columns` rows from a column's `pg_type` OID
/// alone. Unknown OIDs fall back to `text`, mirroring the permissive default
/// used elsewhere in this module.
pub(crate) fn oid_to_type_names(oid: i32) -> (String, String) {
    match oid {
        23 => ("integer".to_string(), "int4".to_string()),
        20 => ("bigint".to_string(), "int8".to_string()),
        16 => ("boolean".to_string(), "bool".to_string()),
        1043 => ("character varying".to_string(), "varchar".to_string()),
        // OID 25 is text itself, and every unrecognized OID falls back to text,
        // so both land on the same names.
        _ => ("text".to_string(), "text".to_string()),
    }
}

/// Convert the wire-format column descriptions (each a single-entry
/// `name -> ColumnDef` map) into the `ColumnSpec` list the shared catalog
/// row-builders consume, resolving each declared type string to its `pg_type`
/// OID. A map with no entry is skipped.
pub(crate) fn column_specs_from_defs(columns: &[BTreeMap<String, ColumnDef>]) -> Vec<ColumnSpec> {
    columns
        .iter()
        .filter_map(|column| {
            column.iter().next().map(|(name, def)| {
                let spec =
                    ColumnSpec::new(name.clone(), map_type_to_oid(&def.col_type), def.nullable);
                if def.has_default {
                    spec.with_default()
                } else {
                    spec
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The declared type strings resolve to the OIDs the catalog serves, and
    /// round-trip to the `information_schema` names those OIDs stand for.
    #[test]
    fn declared_type_strings_round_trip_through_their_oids() {
        for (declared, oid, data_type, udt_name) in [
            ("int", 23, "integer", "int4"),
            ("bigint", 20, "bigint", "int8"),
            ("bool", 16, "boolean", "bool"),
            ("varchar(64)", 1043, "character varying", "varchar"),
            ("text", 25, "text", "text"),
            ("no_such_type", 25, "text", "text"),
        ] {
            assert_eq!(map_type_to_oid(declared), oid, "{declared}");
            let (resolved_data_type, resolved_udt) = oid_to_type_names(oid);
            assert_eq!(resolved_data_type, data_type, "{declared}");
            assert_eq!(resolved_udt, udt_name, "{declared}");
        }
    }

    /// The wire format's single-entry maps become ordered `ColumnSpec`s, with
    /// the default flag carried through.
    #[test]
    fn wire_columns_become_specs_in_declaration_order() {
        let mut id = BTreeMap::new();
        id.insert(
            "id".to_string(),
            ColumnDef {
                col_type: "int".to_string(),
                nullable: false,
                has_default: true,
            },
        );
        let mut name = BTreeMap::new();
        name.insert(
            "name".to_string(),
            ColumnDef {
                col_type: "text".to_string(),
                nullable: true,
                has_default: false,
            },
        );

        let specs = column_specs_from_defs(&[id, name]);
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "id");
        assert!(!specs[0].nullable);
        assert!(specs[0].has_default);
        assert_eq!(specs[1].name, "name");
        assert_eq!(specs[1].type_oid, 25);
        assert!(!specs[1].has_default);
    }
}
