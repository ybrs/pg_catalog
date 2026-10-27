//! The stateful registry every declared catalog object gets its OID from.
//!
//! WHY THIS EXISTS
//! ===============
//! An OID is an identity, and identities have to survive things a counter in
//! memory does not: a server restart, and the host reordering its
//! declarations. Before this registry, declared objects were numbered
//! deterministically from declaration order - stable only for as long as the
//! order never changed - and the only alternative was deriving OIDs from
//! names by hashing, which was tried for the catalog's own numbering and
//! REVERTED by the owner: a 31-bit hash space has birthday collisions, and a
//! relation-OID collision is silent (both rows insert; `pg_attribute`
//! cross-joins one table's columns onto the other). The rule since then is
//! that the catalog never hashes. This registry is what replaces both: the
//! first registration of a name allocates the next OID and RECORDS it, and
//! every later registration of that name - tomorrow, after a restart, after
//! the host shuffles its declaration order - returns the recorded value.
//!
//! ALLOCATION IS GLOBAL, LIKE POSTGRESQL'S. `PostgreSQL` allocates OIDs from
//! one cluster-wide counter, so two databases' tables never share a number in
//! practice even though uniqueness is only required per database. This
//! registry does the same: one counter across every database it serves. That
//! deliberately supersedes the earlier per-database same-floor numbering
//! (where two databases' first tables both landed on 16385), which was an
//! artifact of per-context counters, not a contract worth keeping.
//!
//! UNIQUENESS IS `PostgreSQL`'S. The keys are the ones `PostgreSQL` itself
//! enforces: a database name is unique in the cluster (`pg_database_datname_index`),
//! a schema name within its database (`pg_namespace_nspname_index`), a
//! relation name within its schema (`pg_class_relname_nsp_index`). The
//! registry enforces them by construction - one key, one OID - and the
//! declaration layer on top errors on a duplicate within one declaration set
//! rather than silently skipping it.
//!
//! TWO IMPLEMENTATIONS. [`RedbCatalogOidRegistry`] persists to a
//! [redb](https://docs.rs/redb) file - pure Rust, ACID, copy-on-write B-tree,
//! fsync on commit - so identities survive crashes and restarts.
//! [`InMemoryCatalogOidRegistry`] holds the same semantics in a map for
//! hosts that configure no path; its OIDs are stable only within one process
//! run, which is why choosing it is DEPRECATED and warned about, not silent.
//!
//! WHAT IS NOT REGISTRY MATERIAL. A schema named `public` keeps the built-in
//! identity (OID 2200) and never touches the registry. Live
//! [`crate::lazy_catalog::LazyCatalogSource`]s that supply their own OIDs
//! bypass it entirely - a host's own identifiers pass through verbatim, as
//! they always have. A name that stops being declared keeps its entry, so
//! re-declaring it later returns the same OID rather than `PostgreSQL`'s
//! new-OID-on-recreate; for a catalog emulation, clients caching OIDs across
//! reconnects make the stable answer the safer one.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use datafusion::error::{DataFusionError, Result as DFResult};
use redb::{Database, ReadableTable, TableDefinition};

use crate::lazy_catalog::FIRST_USER_OID;

/// Database name -> OID.
const DATABASES: TableDefinition<&str, i32> = TableDefinition::new("databases");
/// (database, schema name) -> OID.
const SCHEMAS: TableDefinition<(&str, &str), i32> = TableDefinition::new("schemas");
/// (database, schema, relation name) -> (relation OID, rowtype OID).
const RELATIONS: TableDefinition<(&str, &str, &str), (i32, i32)> =
    TableDefinition::new("relations");
/// Single-row bookkeeping: the next OID to hand out.
const ALLOCATOR: TableDefinition<&str, i32> = TableDefinition::new("allocator");

/// The `ALLOCATOR` table's one key.
const NEXT_OID_KEY: &str = "next_oid";

/// Where every declared catalog object's OID comes from.
///
/// Each `resolve_*` call is get-or-allocate: a name seen before returns its
/// recorded OID, a new name is assigned the next one and recorded, atomically.
/// Implementations must be safe to share across threads - riffq resolves from
/// one registry while serving connections.
pub trait CatalogOidRegistry: Send + Sync {
    /// The OID for database `name`, allocating and recording on first sight.
    ///
    /// # Errors
    ///
    /// Returns an error when the backing store cannot be read or written.
    fn resolve_database(&self, name: &str) -> DFResult<i32>;

    /// The OID for schema `name` in `database`, allocating on first sight.
    ///
    /// # Errors
    ///
    /// Returns an error when the backing store cannot be read or written.
    fn resolve_schema(&self, database: &str, name: &str) -> DFResult<i32>;

    /// The `(relation OID, rowtype OID)` for relation `name` in
    /// `database`.`schema`, allocating both on first sight.
    ///
    /// Two OIDs because a relation is two catalog objects sharing one OID
    /// space: its `pg_class` row and its composite rowtype in `pg_type`.
    /// Allocated in one atomic step so no other allocation can land between
    /// them.
    ///
    /// # Errors
    ///
    /// Returns an error when the backing store cannot be read or written.
    fn resolve_relation(&self, database: &str, schema: &str, name: &str) -> DFResult<(i32, i32)>;
}

/// A [`CatalogOidRegistry`] persisted in a [redb] file.
///
/// One file per server. redb holds an exclusive lock on it, so a second
/// process opening the same registry fails loudly rather than corrupting it;
/// commits are fsynced, so an allocation that returned is an allocation that
/// survives a crash.
pub struct RedbCatalogOidRegistry {
    /// The backing store. redb serializes writers internally, so the shared
    /// reference the trait hands out is all that is needed.
    store: Database,
}

impl RedbCatalogOidRegistry {
    /// Open the registry at `path`, creating the file and its tables on first
    /// use.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be created or opened - including
    /// when another process holds it, which is reported rather than waited
    /// out.
    pub fn open(path: &Path) -> DFResult<Self> {
        let store = Database::create(path).map_err(|error| {
            DataFusionError::Execution(format!(
                "cannot open oid registry {}: {error}",
                path.display()
            ))
        })?;
        // Create every table up front so later transactions only ever open
        // existing ones, and a brand-new file is immediately recognisable as
        // a registry.
        let transaction = store.begin_write().map_err(as_registry_error)?;
        {
            transaction
                .open_table(DATABASES)
                .map_err(as_registry_error)?;
            transaction.open_table(SCHEMAS).map_err(as_registry_error)?;
            transaction
                .open_table(RELATIONS)
                .map_err(as_registry_error)?;
            transaction
                .open_table(ALLOCATOR)
                .map_err(as_registry_error)?;
        }
        transaction.commit().map_err(as_registry_error)?;
        Ok(Self { store })
    }

    /// Hand out `count` consecutive OIDs inside the open `transaction`,
    /// bumping the persisted counter.
    ///
    /// The counter starts at [`FIRST_USER_OID`], clear of the built-in range,
    /// and only ever moves forward - a registry never reuses a number, so an
    /// OID a client cached keeps meaning what it meant. Which is exactly why
    /// running out is a refused, named error rather than saturation: a
    /// saturated counter would hand the SAME number to every further name,
    /// the silent-collision failure this module exists to rule out. Two
    /// billion allocations away in practice, but the boundary behaviour is
    /// part of the "never reuses" claim, so it is checked and tested.
    fn allocate(transaction: &redb::WriteTransaction, count: i32) -> DFResult<i32> {
        let mut allocator = transaction
            .open_table(ALLOCATOR)
            .map_err(as_registry_error)?;
        let next = allocator
            .get(NEXT_OID_KEY)
            .map_err(as_registry_error)?
            .map_or(FIRST_USER_OID, |guard| guard.value());
        let bumped = advanced_or_exhausted(next, count)?;
        allocator
            .insert(NEXT_OID_KEY, bumped)
            .map_err(as_registry_error)?;
        Ok(next)
    }
}

/// Render any redb error as the `DataFusionError` the catalog reports.
fn as_registry_error<E: std::fmt::Display>(error: E) -> DataFusionError {
    DataFusionError::Execution(format!("oid registry error: {error}"))
}

/// The counter value after handing out `count` OIDs from `next`, or an
/// exhaustion error when `count` values no longer fit below [`i32::MAX`].
///
/// Shared by both registry implementations so their boundary behaviour cannot
/// drift apart. The check covers the whole span being handed out - a relation
/// takes two consecutive values, and `relation_oid + 1` must be as valid as
/// `relation_oid` itself.
fn advanced_or_exhausted(next: i32, count: i32) -> DFResult<i32> {
    next.checked_add(count).ok_or_else(|| {
        DataFusionError::Execution(format!(
            "oid registry exhausted: allocating {count} oid(s) from {next} would pass \
             {}; no further objects can be registered against this registry",
            i32::MAX
        ))
    })
}

impl CatalogOidRegistry for RedbCatalogOidRegistry {
    fn resolve_database(&self, name: &str) -> DFResult<i32> {
        let transaction = self.store.begin_write().map_err(as_registry_error)?;
        let oid = {
            let mut table = transaction
                .open_table(DATABASES)
                .map_err(as_registry_error)?;
            let recorded = table
                .get(name)
                .map_err(as_registry_error)?
                .map(|guard| guard.value());
            if let Some(oid) = recorded {
                oid
            } else {
                let oid = Self::allocate(&transaction, 1)?;
                table.insert(name, oid).map_err(as_registry_error)?;
                oid
            }
        };
        transaction.commit().map_err(as_registry_error)?;
        Ok(oid)
    }

    fn resolve_schema(&self, database: &str, name: &str) -> DFResult<i32> {
        let transaction = self.store.begin_write().map_err(as_registry_error)?;
        let oid = {
            let mut table = transaction.open_table(SCHEMAS).map_err(as_registry_error)?;
            let recorded = table
                .get((database, name))
                .map_err(as_registry_error)?
                .map(|guard| guard.value());
            if let Some(oid) = recorded {
                oid
            } else {
                let oid = Self::allocate(&transaction, 1)?;
                table
                    .insert((database, name), oid)
                    .map_err(as_registry_error)?;
                oid
            }
        };
        transaction.commit().map_err(as_registry_error)?;
        Ok(oid)
    }

    fn resolve_relation(&self, database: &str, schema: &str, name: &str) -> DFResult<(i32, i32)> {
        let transaction = self.store.begin_write().map_err(as_registry_error)?;
        let oids = {
            let mut table = transaction
                .open_table(RELATIONS)
                .map_err(as_registry_error)?;
            let recorded = table
                .get((database, schema, name))
                .map_err(as_registry_error)?
                .map(|guard| guard.value());
            if let Some(oids) = recorded {
                oids
            } else {
                let relation_oid = Self::allocate(&transaction, 2)?;
                let oids = (relation_oid, relation_oid + 1);
                table
                    .insert((database, schema, name), oids)
                    .map_err(as_registry_error)?;
                oids
            }
        };
        transaction.commit().map_err(as_registry_error)?;
        Ok(oids)
    }
}

/// A [`CatalogOidRegistry`] with no file behind it.
///
/// Same semantics, same allocator, no durability: OIDs are stable within one
/// process run and start over on the next. DEPRECATED as a default - a host
/// that cares about clients caching OIDs across restarts must configure a
/// registry path - and the code that falls back to this says so with a
/// warning rather than silently.
#[derive(Default)]
pub struct InMemoryCatalogOidRegistry {
    /// Every recorded key and the counter, under one lock so get-or-allocate
    /// is atomic exactly as the file-backed transactions are.
    state: Mutex<InMemoryRegistryState>,
}

/// The in-memory registry's tables and counter.
#[derive(Default)]
struct InMemoryRegistryState {
    databases: HashMap<String, i32>,
    schemas: HashMap<(String, String), i32>,
    relations: HashMap<(String, String, String), (i32, i32)>,
    next_oid: Option<i32>,
}

impl InMemoryRegistryState {
    /// Hand out `count` consecutive OIDs from [`FIRST_USER_OID`] up, refusing
    /// with a named exhaustion error once `count` values no longer fit - the
    /// same boundary rule as the file-backed allocator, via the same check.
    fn allocate(&mut self, count: i32) -> DFResult<i32> {
        let next = self.next_oid.unwrap_or(FIRST_USER_OID);
        self.next_oid = Some(advanced_or_exhausted(next, count)?);
        Ok(next)
    }
}

impl InMemoryCatalogOidRegistry {
    /// An empty registry whose first allocation is [`FIRST_USER_OID`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl CatalogOidRegistry for InMemoryCatalogOidRegistry {
    fn resolve_database(&self, name: &str) -> DFResult<i32> {
        let mut state = self.state.lock().expect("registry lock poisoned");
        if let Some(recorded) = state.databases.get(name) {
            return Ok(*recorded);
        }
        let oid = state.allocate(1)?;
        state.databases.insert(name.to_string(), oid);
        Ok(oid)
    }

    fn resolve_schema(&self, database: &str, name: &str) -> DFResult<i32> {
        let mut state = self.state.lock().expect("registry lock poisoned");
        let key = (database.to_string(), name.to_string());
        if let Some(recorded) = state.schemas.get(&key) {
            return Ok(*recorded);
        }
        let oid = state.allocate(1)?;
        state.schemas.insert(key, oid);
        Ok(oid)
    }

    fn resolve_relation(&self, database: &str, schema: &str, name: &str) -> DFResult<(i32, i32)> {
        let mut state = self.state.lock().expect("registry lock poisoned");
        let key = (database.to_string(), schema.to_string(), name.to_string());
        if let Some(recorded) = state.relations.get(&key) {
            return Ok(*recorded);
        }
        let relation_oid = state.allocate(2)?;
        let oids = (relation_oid, relation_oid + 1);
        state.relations.insert(key, oids);
        Ok(oids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both implementations behind one set of assertions, since their
    /// semantics must not drift apart.
    fn registries(dir: &std::path::Path) -> Vec<(&'static str, Box<dyn CatalogOidRegistry>)> {
        vec![
            (
                "redb",
                Box::new(RedbCatalogOidRegistry::open(&dir.join("registry.redb")).unwrap()),
            ),
            ("in-memory", Box::new(InMemoryCatalogOidRegistry::new())),
        ]
    }

    /// A name resolves to the same OID every time, and distinct names to
    /// distinct OIDs from one global counter.
    #[test]
    fn a_name_keeps_its_oid_and_names_never_share_one() {
        let dir = tempfile::tempdir().unwrap();
        for (label, registry) in registries(dir.path()) {
            let auth = registry.resolve_database("auth").unwrap();
            let crm = registry.resolve_database("crm").unwrap();
            assert_eq!(auth, FIRST_USER_OID, "{label}");
            assert_eq!(crm, FIRST_USER_OID + 1, "{label}");
            assert_eq!(
                registry.resolve_database("auth").unwrap(),
                auth,
                "{label}: re-registration returns the recorded oid"
            );

            let schema = registry.resolve_schema("auth", "app").unwrap();
            let (relation, rowtype) = registry.resolve_relation("auth", "app", "users").unwrap();
            let all = [auth, crm, schema, relation, rowtype];
            let distinct: std::collections::HashSet<i32> = all.iter().copied().collect();
            assert_eq!(distinct.len(), all.len(), "{label}: one counter, no reuse");
            assert_eq!(rowtype, relation + 1, "{label}: rowtype is consecutive");
        }
    }

    /// The same name under two databases is two objects - `PostgreSQL`'s
    /// per-database uniqueness keys, not global name uniqueness.
    #[test]
    fn the_same_name_under_two_databases_is_two_objects() {
        let dir = tempfile::tempdir().unwrap();
        for (label, registry) in registries(dir.path()) {
            let in_auth = registry.resolve_schema("auth", "app").unwrap();
            let in_crm = registry.resolve_schema("crm", "app").unwrap();
            assert_ne!(in_auth, in_crm, "{label}");
            let users_in_auth = registry.resolve_relation("auth", "app", "users").unwrap();
            let users_in_crm = registry.resolve_relation("crm", "app", "users").unwrap();
            assert_ne!(users_in_auth.0, users_in_crm.0, "{label}");
        }
    }

    /// OIDs survive closing and reopening the file - the reason the registry
    /// exists.
    #[test]
    fn recorded_oids_survive_reopening_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.redb");

        let (auth, users) = {
            let registry = RedbCatalogOidRegistry::open(&path).unwrap();
            (
                registry.resolve_database("auth").unwrap(),
                registry
                    .resolve_relation("auth", "public", "users")
                    .unwrap(),
            )
        };

        let reopened = RedbCatalogOidRegistry::open(&path).unwrap();
        assert_eq!(reopened.resolve_database("auth").unwrap(), auth);
        assert_eq!(
            reopened
                .resolve_relation("auth", "public", "users")
                .unwrap(),
            users
        );
        // And the counter continued rather than restarting over recorded
        // entries: a NEW name gets a NEW oid.
        let fresh = reopened.resolve_database("crm").unwrap();
        assert!(
            ![auth, users.0, users.1].contains(&fresh),
            "the counter must never reissue a recorded oid"
        );
    }

    /// Declaration order stops mattering once names are recorded.
    ///
    /// The property the in-memory counter never had: register a and b, restart,
    /// register b and a - identical OIDs, because the file remembers.
    #[test]
    fn reordering_declarations_after_a_restart_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.redb");

        let (first_a, first_b) = {
            let registry = RedbCatalogOidRegistry::open(&path).unwrap();
            (
                registry.resolve_database("a").unwrap(),
                registry.resolve_database("b").unwrap(),
            )
        };

        let reopened = RedbCatalogOidRegistry::open(&path).unwrap();
        // Opposite order.
        assert_eq!(reopened.resolve_database("b").unwrap(), first_b);
        assert_eq!(reopened.resolve_database("a").unwrap(), first_a);
    }

    /// A counter at the boundary refuses instead of reissuing a number.
    ///
    /// `saturating_add` here once meant that a counter at `i32::MAX` handed
    /// the SAME oid to every further name - the silent-collision failure this
    /// module exists to rule out, surviving only because no test sat at the
    /// boundary. Both implementations are pinned: the counter is planted next
    /// to the ceiling, the allocation that still fits succeeds, and the one
    /// that would pass the ceiling errors naming the exhaustion - it must
    /// never wrap, saturate, or panic.
    #[test]
    fn an_exhausted_counter_refuses_instead_of_reissuing() {
        // In-memory: plant the counter two below the ceiling. One single
        // allocation fits; the two-value relation allocation that follows
        // would pass the ceiling and must be refused.
        let registry = InMemoryCatalogOidRegistry::new();
        registry
            .state
            .lock()
            .expect("registry lock poisoned")
            .next_oid = Some(i32::MAX - 1);
        assert_eq!(
            registry.resolve_database("fits").unwrap(),
            i32::MAX - 1,
            "the last allocation that fits still succeeds"
        );
        let error = registry
            .resolve_relation("db", "s", "does_not_fit")
            .unwrap_err();
        assert!(
            error.to_string().contains("oid registry exhausted"),
            "{error}"
        );
        // And the refusal recorded nothing: the exhausted registry answers
        // the same way again rather than half-registering the name.
        assert!(registry
            .resolve_relation("db", "s", "does_not_fit")
            .is_err());

        // File-backed: same planting through the allocator table, same rule.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.redb");
        let registry = RedbCatalogOidRegistry::open(&path).unwrap();
        {
            let transaction = registry.store.begin_write().unwrap();
            {
                let mut allocator = transaction.open_table(ALLOCATOR).unwrap();
                allocator.insert(NEXT_OID_KEY, i32::MAX - 1).unwrap();
            }
            transaction.commit().unwrap();
        }
        assert_eq!(registry.resolve_database("fits").unwrap(), i32::MAX - 1);
        let error = registry
            .resolve_relation("db", "s", "does_not_fit")
            .unwrap_err();
        assert!(
            error.to_string().contains("oid registry exhausted"),
            "{error}"
        );
    }

    /// A second process cannot open a locked registry file.
    ///
    /// Two servers allocating from one file independently could hand out the
    /// same oid twice; redb's exclusive lock turns that into a loud error at
    /// open, and this pins that the error is reported rather than waited out.
    #[test]
    fn a_registry_file_is_exclusive_to_one_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.redb");
        let _held = RedbCatalogOidRegistry::open(&path).unwrap();
        let contender = RedbCatalogOidRegistry::open(&path);
        assert!(
            contender.is_err(),
            "a held registry must refuse a second holder"
        );
    }
}
