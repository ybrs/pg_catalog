//! Every way a client can spell the session-identity functions.
//!
//! `current_user`, `session_user`, `current_role`, `user`, `current_catalog` and
//! `current_schema` are SQL-standard niladic functions, and `PostgreSQL` accepts
//! several spellings of each. `sqlparser` models only some of them as function
//! calls, so before this six spellings a client could legitimately send failed
//! here - four with "No field named ...", two as outright parse errors.
//!
//! THE EXPECTATIONS BELOW WERE MEASURED, not derived from the documentation.
//! Each was run against the `PostgreSQL` 17.4 bundled in `postgres-17/`, and the
//! table records what that server actually did - including the four spellings it
//! REJECTS, which are worth pinning too: a compatibility layer that accepts what
//! the real server refuses is a smaller problem than one that refuses what it
//! accepts, but it is still a difference, and one of them (`current_role()`) is
//! the spelling this crate's own tests were written around.
//!
//! Re-measure with:
//!
//! ```bash
//! ./run-postgres.sh
//! postgres-17/bin/psql -h 127.0.0.1 -p 5434 -U sysuser -d postgres -tAc "SELECT current_role"
//! ```

use std::sync::{Arc, Mutex};

use arrow::datatypes::Schema;
use datafusion::error::Result as DFResult;
use datafusion::prelude::SessionContext;
use datafusion_pg_catalog::session::{execute_sql, set_session_user};
use datafusion_pg_catalog::{dispatch_query, session_identity_spellings};

mod common;
use common::base_ctx;

/// What the value of a spelling should be, for the assertions below.
enum Answer {
    /// The connection's role.
    Role,
    /// The connected database.
    Database,
    /// The first explicit entry of the search path.
    Schema,
}

/// One spelling, what `PostgreSQL` 17.4 does with it, and what this crate must.
struct Spelling {
    /// The statement a client sends.
    sql: &'static str,
    /// `Some(what it means)` when `PostgreSQL` answers it, `None` when
    /// `PostgreSQL` rejects it.
    postgresql: Option<Answer>,
    /// Whether this crate must answer it. Never `false` where `postgresql` is
    /// `Some`: refusing something the real server answers is the defect this
    /// file exists to prevent.
    answered_here: bool,
}

/// The measured matrix. `postgresql: None` rows carry the error `PostgreSQL` gave.
const SPELLINGS: &[Spelling] = &[
    // --- accepted by PostgreSQL, and therefore required here ---------------
    Spelling {
        sql: "SELECT current_user",
        postgresql: Some(Answer::Role),
        answered_here: true,
    },
    Spelling {
        sql: "SELECT session_user",
        postgresql: Some(Answer::Role),
        answered_here: true,
    },
    // "No field named current_role" before this work: not a keyword to
    // sqlparser, so it arrived as a bare column reference.
    Spelling {
        sql: "SELECT current_role",
        postgresql: Some(Answer::Role),
        answered_here: true,
    },
    // "No field named user" before this work.
    Spelling {
        sql: "SELECT user",
        postgresql: Some(Answer::Role),
        answered_here: true,
    },
    // "No field named current_catalog" before this work.
    Spelling {
        sql: "SELECT current_catalog",
        postgresql: Some(Answer::Database),
        answered_here: true,
    },
    // "No field named current_schema" before this work.
    Spelling {
        sql: "SELECT current_schema",
        postgresql: Some(Answer::Schema),
        answered_here: true,
    },
    Spelling {
        sql: "SELECT current_schema()",
        postgresql: Some(Answer::Schema),
        answered_here: true,
    },
    Spelling {
        sql: "SELECT current_database()",
        postgresql: Some(Answer::Database),
        answered_here: true,
    },
    // A parse error before this work: sqlparser reads the keyword as
    // parenthesis-less and then rejects the "(".
    Spelling {
        sql: "SELECT pg_catalog.current_user()",
        postgresql: Some(Answer::Role),
        answered_here: true,
    },
    Spelling {
        sql: "SELECT pg_catalog.session_user()",
        postgresql: Some(Answer::Role),
        answered_here: true,
    },
    Spelling {
        sql: "SELECT pg_catalog.current_schema()",
        postgresql: Some(Answer::Schema),
        answered_here: true,
    },
    Spelling {
        sql: "SELECT pg_catalog.current_database()",
        postgresql: Some(Answer::Database),
        answered_here: true,
    },
    // --- rejected by PostgreSQL --------------------------------------------
    // "syntax error at or near '('": the SQL standard spells these three
    // without parentheses and PostgreSQL allows no other form. Answered here
    // anyway - being more permissive than the real server costs nothing, and
    // this crate's own tests were written using current_role().
    Spelling {
        sql: "SELECT current_user()",
        postgresql: None,
        answered_here: true,
    },
    Spelling {
        sql: "SELECT session_user()",
        postgresql: None,
        answered_here: true,
    },
    Spelling {
        sql: "SELECT current_role()",
        postgresql: None,
        answered_here: true,
    },
    // "missing FROM-clause entry for table pg_catalog": qualified but
    // unparenthesised, which PostgreSQL reads as a column of a table called
    // pg_catalog. This one is NOT answered here either - no such function
    // exists in PostgreSQL, so inventing it would be a difference in the
    // direction that misleads.
    Spelling {
        sql: "SELECT pg_catalog.current_role",
        postgresql: None,
        answered_here: false,
    },
];

/// Run `sql` and return its single text value.
async fn single_text(ctx: &SessionContext, sql: &str) -> DFResult<String> {
    let (batches, _schema) = execute_sql(ctx, sql, None, None).await?;
    let column = arrow::compute::cast(batches[0].column(0), &arrow::datatypes::DataType::Utf8)?;
    let array = column
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .expect("a text column");
    Ok(array.value(0).to_string())
}

/// Every spelling `PostgreSQL` answers is answered here, with the right value.
///
/// The values are checked, not just the absence of an error: `user` and
/// `current_catalog` are rewritten onto DIFFERENT registered functions
/// (`current_user` and `current_database`), so a mapping that pointed one at the
/// wrong one would still run and still return a plausible string.
#[tokio::test(flavor = "multi_thread")]
async fn every_spelling_postgresql_answers_is_answered_here() -> DFResult<()> {
    let ctx = base_ctx().await?;
    set_session_user(&ctx, "alice")?;

    for spelling in SPELLINGS {
        let Some(answer) = &spelling.postgresql else {
            continue;
        };
        assert!(
            spelling.answered_here,
            "{} is answered by PostgreSQL, so it cannot be left failing here",
            spelling.sql
        );
        let expected = match answer {
            Answer::Role => "alice",
            // base_ctx builds the catalog "pgtry"; a context serves the
            // database it is named after.
            Answer::Database => "pgtry",
            Answer::Schema => "public",
        };
        assert_eq!(
            single_text(&ctx, spelling.sql).await?,
            expected,
            "{}",
            spelling.sql
        );
    }
    Ok(())
}

/// A spelling `PostgreSQL` rejects and this crate does not answer keeps failing.
///
/// Pinned so that "we match `PostgreSQL` here" stays a fact rather than an
/// accident: `pg_catalog.current_role` names a function neither server has, and
/// answering it would be a wrong answer rather than a missing one.
#[tokio::test(flavor = "multi_thread")]
async fn a_spelling_neither_server_has_still_fails() -> DFResult<()> {
    let ctx = base_ctx().await?;
    for spelling in SPELLINGS {
        if spelling.postgresql.is_some() || spelling.answered_here {
            continue;
        }
        assert!(
            execute_sql(&ctx, spelling.sql, None, None).await.is_err(),
            "{} does not exist in PostgreSQL either, so it must not be invented",
            spelling.sql
        );
    }
    Ok(())
}

/// Every spelling reports the role of the connection asking.
///
/// The rewrite maps several spellings onto one registered function, and that
/// function reads the executing session. A rewrite that resolved to a literal at
/// planning time would pass the value test above and fail this one.
#[tokio::test(flavor = "multi_thread")]
async fn every_role_spelling_follows_the_connections_own_identity() -> DFResult<()> {
    let ctx = base_ctx().await?;
    for role in ["alice", "bob"] {
        set_session_user(&ctx, role)?;
        for spelling in SPELLINGS {
            if !matches!(spelling.postgresql, Some(Answer::Role)) {
                continue;
            }
            assert_eq!(
                single_text(&ctx, spelling.sql).await?,
                role,
                "{} after set_session_user({role})",
                spelling.sql
            );
        }
    }
    Ok(())
}

/// The router claims every identity spelling for the catalog.
///
/// Half the fix, and the half a unit test on the rewrite alone would miss. The
/// router decides catalog-versus-host by looking for a `Expr::Function` whose
/// name is a registered `pg_catalog.*` entry. `SELECT current_role` arrives as a
/// bare column reference and `SELECT pg_catalog.current_user()` does not parse,
/// so neither was ever recognised - a riffq client asking for its own role got
/// the statement forwarded to the Python host, which has no idea what its
/// `PostgreSQL` role is.
#[tokio::test(flavor = "multi_thread")]
async fn the_router_answers_every_identity_spelling_itself() -> DFResult<()> {
    let ctx = base_ctx().await?;

    for spelling in SPELLINGS {
        if spelling.postgresql.is_none() && !spelling.answered_here {
            continue;
        }

        let handed_to_host = Arc::new(Mutex::new(false));
        let flag = handed_to_host.clone();
        let handler = move |_ctx: &SessionContext, _sql: &str, _params, _types| {
            let flag = flag.clone();
            async move {
                *flag.lock().unwrap() = true;
                Ok((Vec::new(), Arc::new(Schema::empty())))
            }
        };

        dispatch_query(&ctx, spelling.sql, None, None, handler).await?;
        assert!(
            !*handed_to_host.lock().unwrap(),
            "{} is the catalog's to answer, but was forwarded to the host",
            spelling.sql
        );
    }
    Ok(())
}

/// The rewrite is a fixed point, which the pipeline requires.
///
/// Roughly thirty passes run after this one and each re-parses what the last
/// wrote, so a rewrite whose own output it would rewrite again - or worse, one
/// whose output no longer parses - breaks statements far from here.
#[test]
fn normalising_twice_changes_nothing() {
    for spelling in SPELLINGS {
        let once = session_identity_spellings::normalize_session_identity_spellings(spelling.sql)
            .expect("every spelling in the matrix parses after the first pass");
        let twice = session_identity_spellings::normalize_session_identity_spellings(&once)
            .expect("and after the second");
        assert_eq!(once, twice, "{} is not a fixed point", spelling.sql);
    }
}
