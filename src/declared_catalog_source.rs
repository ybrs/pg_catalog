//! A [`LazyCatalogSource`] built from up-front declarations.
//!
//! WHY THIS EXISTS
//! ===============
//! This crate used to carry two independent implementations of "put the host's
//! objects into the catalog": the lazy path, where a source is asked for its
//! objects on every scan, and an eager path, where `register_*` helpers ran
//! `INSERT INTO pg_catalog.*` against the built-in tables at startup. Two
//! implementations of one thing meant every behaviour existed twice and could
//! disagree. This module is the collapse onto one: a host that declares its
//! objects up front - riffq's `register_database` / `register_schema` /
//! `register_table`, or this crate's own demo server - has those declarations
//! wrapped in a `DeclaredCatalogSource`, which is then installed exactly the
//! way a live source is. One merge path, one set of row builders, one set of
//! semantics.
//!
//! OIDS COME FROM THE REGISTRY. Every declared object's OID is resolved
//! through a [`CatalogOidRegistry`]: the first declaration of a name allocates
//! the next OID and records it, and every later run - after a restart, after
//! the host reorders its declarations - gets the recorded value back. With a
//! file-backed registry the OIDs are therefore stable across restarts AND
//! across declaration reordering; with the in-memory registry they are stable
//! only within one run, which is why that fallback is deprecated. Allocation
//! is global across databases, as `PostgreSQL`'s own cluster-wide OID counter
//! is. Nothing here is ever derived from a name by hashing - that was tried
//! for the catalog and rejected, because hash collisions in a 31-bit space are
//! silent for relations.
//!
//! UNIQUENESS IS ENFORCED, `PostgreSQL`-STYLE. Declaring the same database
//! twice, the same schema twice under one database, or the same relation
//! twice under one schema is an ERROR naming the duplicate - exactly as
//! `CREATE DATABASE`/`CREATE TABLE` refuse a name that exists - not a silent
//! skip. A declaration referencing a database that was never declared is an
//! error for the same reason. The one create-on-first-use convenience kept
//! from the old path: a TABLE may name a schema nothing declared explicitly,
//! and the schema is declared by that reference.
//!
//! A SCHEMA NAMED `public` KEEPS THE BUILT-IN IDENTITY: OID 2200, owner
//! 6171, never touching the registry. The built-in catalog ships that row in
//! every database, and the lazy merge shadows built-in namespaces BY NAME -
//! so a declared `public` that carried an allocated OID would REPLACE the
//! canonical row with its own, hiding every table from clients that resolve
//! `public` to 2200. Emitting the built-in identity makes the replacement a
//! no-op. A live source that supplies its own OID for `public` keeps it,
//! because a host's own identifiers always pass through untouched.
//!
//! ALSO PRESERVED FROM THE RETIRED EAGER PATH: the input language is type
//! strings (`"int"`, `"text"`), resolved through the same `map_type_to_oid`,
//! and the fixed `pg_database` metadata the eager INSERT wrote - including
//! the ACL riffq's wire tests read back to exercise `text[]` decoding.

use std::collections::BTreeMap;

use datafusion::error::{DataFusionError, Result as DFResult};

use crate::catalog_oid_registry::CatalogOidRegistry;
use crate::lazy_catalog::{
    ColumnSpec, DatabaseDef, LazyCatalogSource, RelationDef, SchemaDef, DEFAULT_OWNER_ROLE_OID,
};
use crate::pg_catalog_helpers::{column_specs_from_defs, ColumnDef};

/// The `pg_namespace.oid` `PostgreSQL` gives the `public` schema in every
/// database, and the built-in catalog ships. A declared schema of that name
/// reuses it - see the module documentation for why inventing a fresh OID
/// would hide the schema's tables.
pub const PUBLIC_SCHEMA_OID: i32 = 2200;

/// The `pg_namespace.nspowner` of the built-in `public` row: the
/// `pg_database_owner` pseudo-role, as in `PostgreSQL` itself. Re-emitted with
/// a declared `public` so shadowing the built-in row by name replaces it with
/// an identical one.
pub const PUBLIC_SCHEMA_OWNER_OID: i32 = 6171;

/// One table declaration: `(database, schema, table, columns)`, each column a
/// single-entry `name -> ColumnDef` map in declaration order - the shape a
/// host's `register_table` call records.
pub type DeclaredTable = (String, String, String, Vec<BTreeMap<String, ColumnDef>>);

/// The `pg_database.datacl` every declared database reports, copied from what
/// the eager path always wrote. Not cosmetic: riffq's `test_datacl_capture`
/// and `test_text_array_return` read this value over the wire to exercise
/// `text[]` decoding, so it is part of the observable contract.
const DECLARED_DATABASE_ACL: [&str; 2] = ["=Tc/dbuser", "dbuser=CTc/dbuser"];

/// The catalog a host declared up front, served through the one lazy merge
/// path. See the module documentation for the semantics it guarantees.
///
/// Build one with [`DeclaredCatalogSource::from_declarations`], from exactly
/// the values a host's `register_*` calls recorded.
#[derive(Debug)]
pub struct DeclaredCatalogSource {
    /// Every declared database, in declaration order, with registry OIDs.
    databases: Vec<DatabaseDef>,
    /// Every schema of every database: `(database name, SchemaDef)`, with
    /// registry OIDs, including schemas declared only by a table naming them.
    schemas: Vec<(String, SchemaDef)>,
    /// Every relation: `(database name, schema name, RelationDef, columns)`,
    /// with registry OIDs and columns resolved to [`ColumnSpec`]s.
    relations: Vec<(String, String, RelationDef, Vec<ColumnSpec>)>,
}

impl DeclaredCatalogSource {
    /// Wrap a host's declarations, resolving every OID through `registry`.
    ///
    /// `databases` are the names handed to `register_database`, in call order;
    /// `schemas` the `(database, schema)` pairs from `register_schema`; and
    /// `tables` the `(database, schema, table, columns)` tuples from
    /// `register_table`. A table's schema does not need a `register_schema`
    /// call of its own - naming it in the table declaration declares it.
    ///
    /// # Errors
    ///
    /// Returns a `PostgreSQL`-style "already exists" error for a duplicate
    /// declaration - the same database twice, the same schema twice under one
    /// database, the same relation twice under one schema - and a "does not
    /// exist" error for a schema or table declared under a database that was
    /// never itself declared. Also propagates any error from the registry's
    /// backing store.
    pub fn from_declarations(
        databases: &[String],
        schemas: &[(String, String)],
        tables: &[DeclaredTable],
        registry: &dyn CatalogOidRegistry,
    ) -> DFResult<Self> {
        let mut database_defs: Vec<DatabaseDef> = Vec::new();
        for name in databases {
            if database_defs.iter().any(|def| def.datname == *name) {
                return Err(DataFusionError::Execution(format!(
                    "database \"{name}\" already exists"
                )));
            }
            database_defs.push(declared_database_row(
                name,
                registry.resolve_database(name)?,
            ));
        }
        let database_exists = |name: &str| database_defs.iter().any(|def| def.datname == name);

        let mut schema_defs: Vec<(String, SchemaDef)> = Vec::new();
        for (database, schema_name) in schemas {
            if !database_exists(database) {
                return Err(DataFusionError::Execution(format!(
                    "cannot declare schema \"{schema_name}\": database \"{database}\" does not exist"
                )));
            }
            if schema_defs
                .iter()
                .any(|(db, def)| db == database && def.name == *schema_name)
            {
                return Err(DataFusionError::Execution(format!(
                    "schema \"{schema_name}\" already exists in database \"{database}\""
                )));
            }
            schema_defs.push((
                database.clone(),
                declared_schema_row(database, schema_name, registry)?,
            ));
        }

        let mut relation_defs: Vec<(String, String, RelationDef, Vec<ColumnSpec>)> = Vec::new();
        for (database, schema_name, table_name, columns) in tables {
            if !database_exists(database) {
                return Err(DataFusionError::Execution(format!(
                    "cannot declare relation \"{table_name}\": database \"{database}\" does not exist"
                )));
            }
            if relation_defs.iter().any(|(db, sch, def, _)| {
                db == database && sch == schema_name && def.name == *table_name
            }) {
                return Err(DataFusionError::Execution(format!(
                    "relation \"{table_name}\" already exists in schema \"{database}\".\"{schema_name}\""
                )));
            }
            // A table may name a schema nothing declared explicitly; the
            // reference declares it. Not a duplicate - only an explicit
            // re-declaration is.
            let schema_already_declared = schema_defs
                .iter()
                .any(|(db, def)| db == database && def.name == *schema_name);
            if !schema_already_declared {
                schema_defs.push((
                    database.clone(),
                    declared_schema_row(database, schema_name, registry)?,
                ));
            }

            let (relation_oid, rowtype_oid) =
                registry.resolve_relation(database, schema_name, table_name)?;
            relation_defs.push((
                database.clone(),
                schema_name.clone(),
                RelationDef::table(relation_oid, rowtype_oid, table_name.clone()),
                column_specs_from_defs(columns),
            ));
        }

        Ok(Self {
            databases: database_defs,
            schemas: schema_defs,
            relations: relation_defs,
        })
    }
}

impl LazyCatalogSource for DeclaredCatalogSource {
    /// Every declared database - `pg_database` lists them all from any
    /// connection.
    fn databases(&self, callback: &mut dyn FnMut(Vec<DatabaseDef>)) -> DFResult<()> {
        callback(self.databases.clone());
        Ok(())
    }

    /// The schemas declared under `database`, including those declared only by
    /// a table naming them.
    fn schemas(&self, database: &str, callback: &mut dyn FnMut(Vec<SchemaDef>)) -> DFResult<()> {
        callback(
            self.schemas
                .iter()
                .filter(|(db, _)| db == database)
                .map(|(_, def)| def.clone())
                .collect(),
        );
        Ok(())
    }

    /// The tables declared under `database`.`schema`.
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

    /// The declared columns of one table, in declaration order.
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
                .find(|(db, sch, def, _)| db == database && sch == schema && def.name == relation)
                .map(|(_, _, _, columns)| columns.clone())
                .unwrap_or_default(),
        );
        Ok(())
    }
}

/// The `pg_database` row for a declared database: the fixed metadata the eager
/// path always wrote. Everything not set here already defaults to the value
/// the eager INSERT used - see `build_pg_database_row`.
fn declared_database_row(name: &str, oid: i32) -> DatabaseDef {
    let mut row = DatabaseDef::new(oid, name, DEFAULT_OWNER_ROLE_OID);
    row.datacl = Some(
        DECLARED_DATABASE_ACL
            .iter()
            .map(ToString::to_string)
            .collect(),
    );
    row
}

/// The `pg_namespace` row for a declared schema.
///
/// `public` reuses the built-in identity and never touches the registry; any
/// other name resolves through it. See [`PUBLIC_SCHEMA_OID`].
fn declared_schema_row(
    database: &str,
    schema_name: &str,
    registry: &dyn CatalogOidRegistry,
) -> DFResult<SchemaDef> {
    if schema_name == "public" {
        let mut def = SchemaDef::new(PUBLIC_SCHEMA_OID, schema_name);
        def.owner_oid = Some(PUBLIC_SCHEMA_OWNER_OID);
        return Ok(def);
    }
    Ok(SchemaDef::new(
        registry.resolve_schema(database, schema_name)?,
        schema_name,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_oid_registry::{InMemoryCatalogOidRegistry, RedbCatalogOidRegistry};
    use crate::lazy_catalog::FIRST_USER_OID;

    /// One declared column in the wire format the source takes.
    fn column(name: &str, col_type: &str) -> BTreeMap<String, ColumnDef> {
        let mut map = BTreeMap::new();
        map.insert(
            name.to_string(),
            ColumnDef {
                col_type: col_type.to_string(),
                nullable: true,
                has_default: false,
            },
        );
        map
    }

    /// The two-database fixture the riffq contract tests declare.
    fn two_database_declarations() -> (Vec<String>, Vec<(String, String)>, Vec<DeclaredTable>) {
        (
            vec!["db1".to_string(), "db2".to_string()],
            vec![
                ("db1".to_string(), "s1".to_string()),
                ("db2".to_string(), "s2".to_string()),
            ],
            vec![
                (
                    "db1".to_string(),
                    "s1".to_string(),
                    "t1".to_string(),
                    vec![column("id", "int")],
                ),
                (
                    "db2".to_string(),
                    "s2".to_string(),
                    "t2".to_string(),
                    vec![column("id", "int")],
                ),
            ],
        )
    }

    /// Collect the schemas the source reports for one database.
    fn schemas_of(source: &DeclaredCatalogSource, database: &str) -> Vec<SchemaDef> {
        let mut out = Vec::new();
        source
            .schemas(database, &mut |defs| out.extend(defs))
            .unwrap();
        out
    }

    /// Collect the relations the source reports for one schema.
    fn relations_of(
        source: &DeclaredCatalogSource,
        database: &str,
        schema: &str,
    ) -> Vec<RelationDef> {
        let mut out = Vec::new();
        source
            .relations(database, schema, &mut |defs| out.extend(defs))
            .unwrap();
        out
    }

    /// OIDs come from one global counter, and a relation's rowtype takes the
    /// next consecutive value - `PostgreSQL`'s own arrangement.
    #[test]
    fn oids_allocate_globally_with_consecutive_rowtypes() {
        let registry = InMemoryCatalogOidRegistry::new();
        let (databases, schemas, tables) = two_database_declarations();
        let source =
            DeclaredCatalogSource::from_declarations(&databases, &schemas, &tables, &registry)
                .unwrap();

        let mut database_oids = Vec::new();
        source
            .databases(&mut |defs| database_oids.extend(defs.into_iter().map(|d| d.oid)))
            .unwrap();
        assert_eq!(database_oids, vec![FIRST_USER_OID, FIRST_USER_OID + 1]);

        let t1 = relations_of(&source, "db1", "s1");
        let t2 = relations_of(&source, "db2", "s2");
        assert_eq!(t1[0].reltype_oid, t1[0].oid + 1);
        assert_ne!(
            t1[0].oid, t2[0].oid,
            "one cluster-wide counter, as PostgreSQL allocates"
        );

        // Everything distinct: the registry never hands one number to two
        // objects, whatever their kind.
        let all = [
            database_oids[0],
            database_oids[1],
            schemas_of(&source, "db1")[0].oid,
            schemas_of(&source, "db2")[0].oid,
            t1[0].oid,
            t1[0].reltype_oid,
            t2[0].oid,
            t2[0].reltype_oid,
        ];
        let distinct: std::collections::HashSet<i32> = all.iter().copied().collect();
        assert_eq!(distinct.len(), all.len());
    }

    /// Reordering the declarations against a persistent registry changes no
    /// OID - the property the registry exists for, unreachable by any counter.
    #[test]
    fn a_persistent_registry_makes_declaration_order_irrelevant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.redb");
        let (databases, schemas, tables) = two_database_declarations();

        let first_run: Vec<i32> = {
            let registry = RedbCatalogOidRegistry::open(&path).unwrap();
            let source =
                DeclaredCatalogSource::from_declarations(&databases, &schemas, &tables, &registry)
                    .unwrap();
            [
                relations_of(&source, "db1", "s1")[0].oid,
                relations_of(&source, "db2", "s2")[0].oid,
            ]
            .to_vec()
        };

        // A restart with every list REVERSED.
        let registry = RedbCatalogOidRegistry::open(&path).unwrap();
        let reversed_databases: Vec<String> = databases.iter().rev().cloned().collect();
        let reversed_schemas: Vec<(String, String)> = schemas.iter().rev().cloned().collect();
        let reversed_tables: Vec<DeclaredTable> = tables.iter().rev().cloned().collect();
        let source = DeclaredCatalogSource::from_declarations(
            &reversed_databases,
            &reversed_schemas,
            &reversed_tables,
            &registry,
        )
        .unwrap();

        assert_eq!(relations_of(&source, "db1", "s1")[0].oid, first_run[0]);
        assert_eq!(relations_of(&source, "db2", "s2")[0].oid, first_run[1]);
    }

    /// A declared `public` keeps the built-in identity and consumes nothing
    /// from the registry.
    #[test]
    fn a_declared_public_schema_keeps_the_builtin_identity() {
        let registry = InMemoryCatalogOidRegistry::new();
        let source = DeclaredCatalogSource::from_declarations(
            &["appdb".to_string()],
            &[("appdb".to_string(), "public".to_string())],
            &[(
                "appdb".to_string(),
                "public".to_string(),
                "users".to_string(),
                vec![column("id", "int")],
            )],
            &registry,
        )
        .unwrap();

        let schemas = schemas_of(&source, "appdb");
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].oid, PUBLIC_SCHEMA_OID);
        assert_eq!(schemas[0].owner_oid, Some(PUBLIC_SCHEMA_OWNER_OID));

        // public consumed no counter value: the table follows the database
        // directly.
        let relations = relations_of(&source, "appdb", "public");
        assert_eq!(relations[0].oid, FIRST_USER_OID + 1);
    }

    /// Each kind of duplicate declaration is refused with an "already exists"
    /// error naming the object, as `PostgreSQL` refuses CREATE of an existing
    /// name.
    #[test]
    fn duplicate_declarations_are_refused_by_name() {
        let registry = InMemoryCatalogOidRegistry::new();

        let twice = ["db".to_string(), "db".to_string()];
        let error =
            DeclaredCatalogSource::from_declarations(&twice, &[], &[], &registry).unwrap_err();
        assert!(
            error.to_string().contains("database \"db\" already exists"),
            "{error}"
        );

        let databases = ["db".to_string()];
        let schema_twice = [
            ("db".to_string(), "s".to_string()),
            ("db".to_string(), "s".to_string()),
        ];
        let error =
            DeclaredCatalogSource::from_declarations(&databases, &schema_twice, &[], &registry)
                .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("schema \"s\" already exists in database \"db\""),
            "{error}"
        );

        let table_twice: Vec<DeclaredTable> = vec![
            (
                "db".to_string(),
                "s".to_string(),
                "t".to_string(),
                vec![column("id", "int")],
            ),
            (
                "db".to_string(),
                "s".to_string(),
                "t".to_string(),
                vec![column("id", "int")],
            ),
        ];
        let error =
            DeclaredCatalogSource::from_declarations(&databases, &[], &table_twice, &registry)
                .unwrap_err();
        assert!(
            error.to_string().contains("relation \"t\" already exists"),
            "{error}"
        );
    }

    /// A declaration under a database nothing declared is refused, not
    /// silently dropped.
    #[test]
    fn a_declaration_under_an_undeclared_database_is_refused() {
        let registry = InMemoryCatalogOidRegistry::new();
        let databases = ["declared".to_string()];

        let stray_table: Vec<DeclaredTable> = vec![(
            "ghost".to_string(),
            "s".to_string(),
            "t".to_string(),
            vec![column("id", "int")],
        )];
        let error =
            DeclaredCatalogSource::from_declarations(&databases, &[], &stray_table, &registry)
                .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("database \"ghost\" does not exist"),
            "{error}"
        );

        let stray_schema = [("ghost".to_string(), "s".to_string())];
        let error =
            DeclaredCatalogSource::from_declarations(&databases, &stray_schema, &[], &registry)
                .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("database \"ghost\" does not exist"),
            "{error}"
        );
    }

    /// A schema named only by a table declaration exists anyway - the
    /// create-on-first-use convenience `register_table` always had - and a
    /// table under an explicitly declared schema is not a duplicate of it.
    #[test]
    fn a_schema_named_only_by_a_table_declaration_is_declared() {
        let registry = InMemoryCatalogOidRegistry::new();
        let source = DeclaredCatalogSource::from_declarations(
            &["db".to_string()],
            &[],
            &[(
                "db".to_string(),
                "undeclared".to_string(),
                "t".to_string(),
                vec![column("id", "int")],
            )],
            &registry,
        )
        .unwrap();

        let schemas = schemas_of(&source, "db");
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "undeclared");
        assert_eq!(relations_of(&source, "db", "undeclared").len(), 1);
    }

    /// Column type strings resolve to the same `pg_type` OIDs as always.
    #[test]
    fn column_type_strings_resolve_to_pg_type_oids() {
        let registry = InMemoryCatalogOidRegistry::new();
        let source = DeclaredCatalogSource::from_declarations(
            &["db".to_string()],
            &[],
            &[(
                "db".to_string(),
                "s".to_string(),
                "t".to_string(),
                vec![column("id", "int"), column("name", "text")],
            )],
            &registry,
        )
        .unwrap();

        let mut columns = Vec::new();
        source
            .columns("db", "s", "t", &mut |specs| columns.extend(specs))
            .unwrap();
        assert_eq!(columns[0].type_oid, 23, "int -> int4");
        assert_eq!(columns[1].type_oid, 25, "text -> text");
    }

    /// The declared database ACL is the one the eager path always wrote.
    #[test]
    fn declared_databases_carry_the_contractual_acl() {
        let registry = InMemoryCatalogOidRegistry::new();
        let (databases, schemas, tables) = two_database_declarations();
        let source =
            DeclaredCatalogSource::from_declarations(&databases, &schemas, &tables, &registry)
                .unwrap();
        let mut defs = Vec::new();
        source.databases(&mut |batch| defs.extend(batch)).unwrap();
        for def in &defs {
            assert_eq!(
                def.datacl.as_deref(),
                Some(&["=Tc/dbuser".to_string(), "dbuser=CTc/dbuser".to_string()][..])
            );
        }
    }
}
