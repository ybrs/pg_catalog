//! A [`LazyCatalogSource`] merging the answers of several sources.
//!
//! WHY THIS EXISTS
//! ===============
//! A host often knows part of its catalog up front and part only at runtime:
//! a fixed base database (`auth`) declared before start, plus databases that
//! appear while the server runs - user-created databases, adhoc mounts, a
//! `DuckDB` file attached on demand. Those are naturally TWO sources - a
//! [`crate::declared_catalog_source::DeclaredCatalogSource`] and a live one -
//! and before this module riffq forced a choice: installing a live source
//! silently discarded every declared registration.
//!
//! `CombinedCatalogSource` removes the choice. It holds an ordered list of
//! sources and answers every trait method with the UNION of their answers,
//! deduplicated so the merge downstream never sees the duplicate rows that
//! fail a catalog scan.
//!
//! COLLISIONS ARE ERRORS, EXCEPT FOR SCHEMAS
//! =========================================
//! Two sources reporting the same DATABASE name, or the same RELATION name
//! under one schema, is refused loudly: those are `PostgreSQL`'s uniqueness
//! keys, and two objects claiming one name means one of them is lying - a
//! runtime mount reusing the declared base's name, or two mounts fighting.
//! Serving either silently would hand clients rows from an object they did
//! not name. The error names the object, so the scan that hits it says what
//! collided instead of which join broke.
//!
//! SCHEMAS dedup by name with the earlier source winning, deliberately: one
//! schema name shared across sources is not two objects claiming one identity
//! but the SAME namespace being contributed to - it is exactly how a live
//! source mounts tables into a declared database's `public`, under the
//! canonical 2200 row.
//!
//! A DATABASE BOTH SOURCES CONTRIBUTE TO MERGES, which is the point of the
//! union: relations are fetched per schema NAME, so callback tables reported
//! under a declared database's `public` land under the DECLARED `public` -
//! the canonical OID 2200 one - alongside the declared tables, provided no
//! relation NAME collides.
//!
//! WHAT IS NOT MERGED. A relation's COLUMNS come from the first source with a
//! non-empty answer for it, not from a union: column lists are ordinal
//! (`attnum` is a position), so interleaving two sources' lists would invent
//! a table neither source described. The winner is whoever answers - which is
//! the source that reported the relation, since the others do not know it.
//!
//! OIDs are still the sources' own, passed through verbatim. Keeping them
//! from colliding ACROSS sources is the host's responsibility, exactly as
//! keeping one source's OIDs consistent always was; a declared source numbers
//! from [`crate::lazy_catalog::FIRST_USER_OID`] upward while hash-derived
//! schemes sit far above, so the default arrangement does not collide.

use std::collections::HashSet;
use std::sync::Arc;

use datafusion::error::{DataFusionError, Result as DFResult};

use crate::lazy_catalog::{
    ColumnSpec, ConfigSettingDef, ConstraintDef, DatabaseDef, IndexDef, LazyCatalogSource,
    RelationDef, SchemaDef, SettingDef,
};

/// An ordered union over any number of [`LazyCatalogSource`]s: every method
/// answers with all sources' objects, a database or relation name claimed by
/// two sources refused loudly and schemas deduplicated to the earliest
/// source. See the module documentation for the rules and their reasons.
pub struct CombinedCatalogSource {
    /// The sources in precedence order - index 0 wins every collision.
    sources: Vec<Arc<dyn LazyCatalogSource>>,
}

impl CombinedCatalogSource {
    /// Combine `sources`. Order matters only for schemas, where the earliest
    /// source's row survives a name overlap; database and relation overlaps
    /// are errors whichever side reports them.
    #[must_use]
    pub fn new(sources: Vec<Arc<dyn LazyCatalogSource>>) -> Self {
        Self { sources }
    }
}

/// Collect one callback-shaped query against `source` into a vector.
///
/// The trait delivers rows through a callback that may fire any number of
/// times; the dedup below needs the rows in hand, so each source's answer is
/// materialized first.
fn collected<T>(query: impl Fn(&mut dyn FnMut(Vec<T>)) -> DFResult<()>) -> DFResult<Vec<T>> {
    let mut out = Vec::new();
    query(&mut |defs| out.extend(defs))?;
    Ok(out)
}

impl LazyCatalogSource for CombinedCatalogSource {
    /// Every source's databases; one name reported by two sources is refused.
    fn databases(&self, callback: &mut dyn FnMut(Vec<DatabaseDef>)) -> DFResult<()> {
        let mut seen = HashSet::new();
        for source in &self.sources {
            let defs = collected(|cb| source.databases(cb))?;
            for def in &defs {
                if !seen.insert(def.datname.clone()) {
                    return Err(DataFusionError::Execution(format!(
                        "database \"{}\" is reported by more than one catalog source",
                        def.datname
                    )));
                }
            }
            callback(defs);
        }
        Ok(())
    }

    /// Every source's schemas in `database`, deduplicated by name.
    ///
    /// This dedup is what lands a later source's tables under an earlier
    /// source's schema: the earlier `public` survives with its OID, and the
    /// later source's relations are fetched by schema NAME regardless.
    fn schemas(&self, database: &str, callback: &mut dyn FnMut(Vec<SchemaDef>)) -> DFResult<()> {
        let mut seen = HashSet::new();
        for source in &self.sources {
            let mut defs = collected(|cb| source.schemas(database, cb))?;
            defs.retain(|def| seen.insert(def.name.clone()));
            callback(defs);
        }
        Ok(())
    }

    /// Every source's relations in `database`.`schema`; one name reported by
    /// two sources is refused.
    fn relations(
        &self,
        database: &str,
        schema: &str,
        callback: &mut dyn FnMut(Vec<RelationDef>),
    ) -> DFResult<()> {
        let mut seen = HashSet::new();
        for source in &self.sources {
            let defs = collected(|cb| source.relations(database, schema, cb))?;
            for def in &defs {
                if !seen.insert(def.name.clone()) {
                    return Err(DataFusionError::Execution(format!(
                        "relation \"{}\" in \"{database}\".\"{schema}\" is reported \
                         by more than one catalog source",
                        def.name
                    )));
                }
            }
            callback(defs);
        }
        Ok(())
    }

    /// The columns of the first source that answers for `relation`.
    ///
    /// Not a union - `attnum` is ordinal, so two sources' lists cannot be
    /// interleaved. A source that does not know the relation answers with
    /// nothing and the question moves to the next, so the answer comes from
    /// the source that reported the relation. A relation declared with zero
    /// columns therefore reads as unknown here; no host declares one.
    fn columns(
        &self,
        database: &str,
        schema: &str,
        relation: &str,
        callback: &mut dyn FnMut(Vec<ColumnSpec>),
    ) -> DFResult<()> {
        for source in &self.sources {
            let specs = collected(|cb| source.columns(database, schema, relation, cb))?;
            if !specs.is_empty() {
                callback(specs);
                return Ok(());
            }
        }
        Ok(())
    }

    /// Every source's indexes in `database`.`schema`, deduplicated by the
    /// index's own `pg_class` OID.
    fn indexes(
        &self,
        database: &str,
        schema: &str,
        callback: &mut dyn FnMut(Vec<IndexDef>),
    ) -> DFResult<()> {
        let mut seen = HashSet::new();
        for source in &self.sources {
            let mut defs = collected(|cb| source.indexes(database, schema, cb))?;
            defs.retain(|def| seen.insert(def.index_oid));
            callback(defs);
        }
        Ok(())
    }

    /// Every source's constraints in `database`.`schema`, deduplicated by
    /// constraint OID.
    fn constraints(
        &self,
        database: &str,
        schema: &str,
        callback: &mut dyn FnMut(Vec<ConstraintDef>),
    ) -> DFResult<()> {
        let mut seen = HashSet::new();
        for source in &self.sources {
            let mut defs = collected(|cb| source.constraints(database, schema, cb))?;
            defs.retain(|def| seen.insert(def.oid));
            callback(defs);
        }
        Ok(())
    }

    /// Every source's `pg_config` overrides, deduplicated by setting name.
    fn config(&self, callback: &mut dyn FnMut(Vec<ConfigSettingDef>)) -> DFResult<()> {
        let mut seen = HashSet::new();
        for source in &self.sources {
            let mut defs = collected(|cb| source.config(cb))?;
            defs.retain(|def| seen.insert(def.name.clone()));
            callback(defs);
        }
        Ok(())
    }

    /// Every source's `pg_settings` overrides, deduplicated by parameter name.
    fn settings(&self, callback: &mut dyn FnMut(Vec<SettingDef>)) -> DFResult<()> {
        let mut seen = HashSet::new();
        for source in &self.sources {
            let mut defs = collected(|cb| source.settings(cb))?;
            defs.retain(|def| seen.insert(def.name.clone()));
            callback(defs);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed in-memory source, the shape every test here composes from.
    struct FixedSource {
        databases: Vec<DatabaseDef>,
        /// `(database, schema)` pairs.
        schemas: Vec<(String, SchemaDef)>,
        /// `(database, schema, relation, columns)`.
        relations: Vec<(String, String, RelationDef, Vec<ColumnSpec>)>,
    }

    impl LazyCatalogSource for FixedSource {
        fn databases(&self, callback: &mut dyn FnMut(Vec<DatabaseDef>)) -> DFResult<()> {
            callback(self.databases.clone());
            Ok(())
        }
        fn schemas(
            &self,
            database: &str,
            callback: &mut dyn FnMut(Vec<SchemaDef>),
        ) -> DFResult<()> {
            callback(
                self.schemas
                    .iter()
                    .filter(|(db, _)| db == database)
                    .map(|(_, def)| def.clone())
                    .collect(),
            );
            Ok(())
        }
        fn relations(
            &self,
            database: &str,
            schema: &str,
            callback: &mut dyn FnMut(Vec<RelationDef>),
        ) -> DFResult<()> {
            callback(
                self.relations
                    .iter()
                    .filter(|(db, sch, ..)| db == database && sch == schema)
                    .map(|(_, _, def, _)| def.clone())
                    .collect(),
            );
            Ok(())
        }
        fn columns(
            &self,
            database: &str,
            schema: &str,
            relation: &str,
            callback: &mut dyn FnMut(Vec<ColumnSpec>),
        ) -> DFResult<()> {
            callback(
                self.relations
                    .iter()
                    .find(|(db, sch, def, _)| {
                        db == database && sch == schema && def.name == relation
                    })
                    .map(|(_, _, _, cols)| cols.clone())
                    .unwrap_or_default(),
            );
            Ok(())
        }
    }

    /// The declared-base-plus-live-mounts arrangement: `auth` from the base
    /// source, `adhoc` from the live one.
    fn base_plus_live() -> CombinedCatalogSource {
        let base = FixedSource {
            databases: vec![DatabaseDef::new(16384, "auth", 10)],
            schemas: vec![("auth".into(), SchemaDef::new(2200, "public"))],
            relations: vec![(
                "auth".into(),
                "public".into(),
                RelationDef::table(16385, 16386, "users"),
                vec![ColumnSpec::new("id", 23, false)],
            )],
        };
        let live = FixedSource {
            databases: vec![DatabaseDef::new(70001, "adhoc", 10)],
            schemas: vec![("adhoc".into(), SchemaDef::new(70100, "main"))],
            relations: vec![(
                "adhoc".into(),
                "main".into(),
                RelationDef::table(70200, 70201, "events"),
                vec![ColumnSpec::new("ts", 25, true)],
            )],
        };
        CombinedCatalogSource::new(vec![Arc::new(base), Arc::new(live)])
    }

    fn database_names(source: &CombinedCatalogSource) -> Vec<String> {
        let mut out = Vec::new();
        source
            .databases(&mut |defs| out.extend(defs.into_iter().map(|d| d.datname)))
            .unwrap();
        out
    }

    fn relation_names(source: &CombinedCatalogSource, db: &str, schema: &str) -> Vec<String> {
        let mut out = Vec::new();
        source
            .relations(db, schema, &mut |defs| {
                out.extend(defs.into_iter().map(|d| d.name));
            })
            .unwrap();
        out
    }

    /// Both sources' databases are listed, each serving its own objects.
    #[test]
    fn the_union_serves_both_sources_databases() {
        let combined = base_plus_live();
        assert_eq!(database_names(&combined), vec!["auth", "adhoc"]);
        assert_eq!(relation_names(&combined, "auth", "public"), vec!["users"]);
        assert_eq!(relation_names(&combined, "adhoc", "main"), vec!["events"]);
    }

    /// A database name reported by two sources is refused, naming it.
    ///
    /// Serving either row silently would hand clients rows from an object
    /// they did not name - a runtime mount reusing the declared base's name
    /// is a configuration error to surface, not to paper over.
    #[test]
    fn a_database_name_reported_by_two_sources_is_refused() {
        let base = FixedSource {
            databases: vec![DatabaseDef::new(16384, "auth", 10)],
            schemas: vec![],
            relations: vec![],
        };
        let live = FixedSource {
            databases: vec![DatabaseDef::new(99999, "auth", 10)],
            schemas: vec![],
            relations: vec![],
        };
        let combined = CombinedCatalogSource::new(vec![Arc::new(base), Arc::new(live)]);

        let error = combined.databases(&mut |_| {}).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("database \"auth\" is reported by more than one catalog source"),
            "{error}"
        );
    }

    /// A database both sources CONTRIBUTE to merges their relations, under
    /// the earlier source's schema identity.
    ///
    /// This is the arrangement the module exists for: callback tables landing
    /// inside a declared base database, under its canonical public.
    #[test]
    fn a_shared_database_merges_relations_under_the_earlier_schema() {
        let base = FixedSource {
            databases: vec![DatabaseDef::new(16384, "auth", 10)],
            schemas: vec![("auth".into(), SchemaDef::new(2200, "public"))],
            relations: vec![(
                "auth".into(),
                "public".into(),
                RelationDef::table(16385, 16386, "users"),
                vec![ColumnSpec::new("id", 23, false)],
            )],
        };
        let live = FixedSource {
            databases: vec![],
            schemas: vec![("auth".into(), SchemaDef::new(70100, "public"))],
            relations: vec![(
                "auth".into(),
                "public".into(),
                RelationDef::table(70200, 70201, "sessions"),
                vec![ColumnSpec::new("token", 25, false)],
            )],
        };
        let combined = CombinedCatalogSource::new(vec![Arc::new(base), Arc::new(live)]);

        let mut schemas = Vec::new();
        combined
            .schemas("auth", &mut |defs| schemas.extend(defs))
            .unwrap();
        assert_eq!(schemas.len(), 1, "one public, not two");
        assert_eq!(schemas[0].oid, 2200, "the canonical identity wins");

        assert_eq!(
            relation_names(&combined, "auth", "public"),
            vec!["users", "sessions"],
            "both sources' tables, under that one schema"
        );
    }

    /// A relation's columns come from the source that reported it.
    #[test]
    fn columns_come_from_the_source_that_owns_the_relation() {
        let combined = base_plus_live();
        let mut users = Vec::new();
        combined
            .columns("auth", "public", "users", &mut |specs| users.extend(specs))
            .unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].name, "id");

        // The live source's relation: the base answers nothing for it, so the
        // question falls through rather than stopping at the first source.
        let mut events = Vec::new();
        combined
            .columns("adhoc", "main", "events", &mut |specs| {
                events.extend(specs);
            })
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "ts");
    }

    /// A relation name claimed by two sources in one schema is refused.
    ///
    /// This is where a silent choice would be worst: whichever table won,
    /// clients would read rows and columns from an object they never picked.
    #[test]
    fn a_relation_name_reported_by_two_sources_is_refused() {
        let base = FixedSource {
            databases: vec![DatabaseDef::new(16384, "auth", 10)],
            schemas: vec![("auth".into(), SchemaDef::new(2200, "public"))],
            relations: vec![(
                "auth".into(),
                "public".into(),
                RelationDef::table(16385, 16386, "users"),
                vec![ColumnSpec::new("id", 23, false)],
            )],
        };
        let live = FixedSource {
            databases: vec![],
            schemas: vec![],
            relations: vec![(
                "auth".into(),
                "public".into(),
                RelationDef::table(70200, 70201, "users"),
                vec![ColumnSpec::new("impostor", 25, true)],
            )],
        };
        let combined = CombinedCatalogSource::new(vec![Arc::new(base), Arc::new(live)]);

        let error = combined
            .relations("auth", "public", &mut |_| {})
            .unwrap_err();
        assert!(
            error.to_string().contains(
                "relation \"users\" in \"auth\".\"public\" is reported by more than one \
                 catalog source"
            ),
            "{error}"
        );
    }

    /// An error in any source is reported, not swallowed by the union.
    #[test]
    fn a_failing_source_fails_the_combined_answer() {
        struct FailingSource;
        impl LazyCatalogSource for FailingSource {
            fn databases(&self, _cb: &mut dyn FnMut(Vec<DatabaseDef>)) -> DFResult<()> {
                Err(datafusion::error::DataFusionError::Execution(
                    "the mount is gone".to_string(),
                ))
            }
            fn schemas(&self, _d: &str, _cb: &mut dyn FnMut(Vec<SchemaDef>)) -> DFResult<()> {
                Ok(())
            }
            fn relations(
                &self,
                _d: &str,
                _s: &str,
                _cb: &mut dyn FnMut(Vec<RelationDef>),
            ) -> DFResult<()> {
                Ok(())
            }
            fn columns(
                &self,
                _d: &str,
                _s: &str,
                _r: &str,
                _cb: &mut dyn FnMut(Vec<ColumnSpec>),
            ) -> DFResult<()> {
                Ok(())
            }
        }
        let combined = CombinedCatalogSource::new(vec![
            Arc::new(FixedSource {
                databases: vec![DatabaseDef::new(16384, "auth", 10)],
                schemas: vec![],
                relations: vec![],
            }),
            Arc::new(FailingSource),
        ]);
        let result = combined.databases(&mut |_| {});
        assert!(result.is_err(), "a failing source must be reported");
    }
}
