//! A statement the rewriter cannot handle must come back as an error.
//!
//! This project's rule is that a parse failure is reported to the client and
//! never fatal, and the rewrite pipeline broke it in one place: `rewrite_filters`
//! called the scalar-to-CTE rewrite through a wrapper that unwraps, so a
//! statement carrying no statement at all panicked instead of returning.
//!
//! `-- just a comment` is the shape that reaches it. The server filters the
//! strictly empty string but not a comment-only one, so a client sending it
//! took its own connection down. Since the panic unwinds inside the
//! per-connection task, the symptom is "server closed the connection
//! unexpectedly" with nothing useful on either side.
//!
//! These tests run the rewrite pipeline directly rather than over the wire,
//! because that is where the contract lives - the wire behaviour is a
//! consequence of it.

use datafusion_pg_catalog::session::rewrite_filters;

/// A statement holding only a comment is an error, not a panic.
#[test]
fn a_comment_only_statement_is_reported_rather_than_panicking() {
    for sql in ["-- hi", "  -- hi\n", "/* nothing here */"] {
        let outcome = rewrite_filters(sql);
        assert!(
            outcome.is_err(),
            "{sql:?} carries no statement, so it must report an error: {:?}",
            outcome.map(|(rewritten, _)| rewritten)
        );
    }
}

/// So is SQL the parser rejects outright.
#[test]
fn unparsable_sql_is_reported_rather_than_panicking() {
    let outcome = rewrite_filters("SELECT FROM WHERE ((");
    assert!(
        outcome.is_err(),
        "malformed SQL must report an error rather than abort"
    );
}

/// An ordinary statement still comes back rewritten.
///
/// The guard on the guard: returning an error for everything would satisfy the
/// assertions above and break the server completely.
#[test]
fn a_valid_statement_still_rewrites() {
    let (rewritten, _aliases) =
        rewrite_filters("SELECT relname FROM pg_catalog.pg_class").expect("valid SQL rewrites");
    assert!(
        rewritten.contains("pg_class"),
        "the rewritten statement should still name the relation: {rewritten}"
    );
}
