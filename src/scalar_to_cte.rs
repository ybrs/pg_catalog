//! Minimal scaffolding for “scalar-subquery → CTE” rewrite logic.
//!
//! Compile-time *only* — every interesting function is still `todo!()`.
//
// Phase map (see discussion):
//   0. scaffolding  ← **you are here**
//   1. visitor utilities
//   2. correlation analysis
//   3. CTE builder
//   4. top-level rewriter
//   5. unit-tests
//   6. polish / edge cases

// External crates we’ll eventually rely on.
// They’re unused for now but kept here so adding real code later is friction-free.
#![allow(unused_imports)]

use sqlparser::ast::*;
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

use datafusion::error::{DataFusionError, Result};

/// Result of a rewrite: the new SQL string **and** the number of
/// scalar-subqueries that were converted (handy for tests / stats).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteOutcome {
    pub sql: String,
    pub converted: usize,
}

/// Public façade — callers feed raw SQL; we hand back rewritten SQL.
///
/// *Phase-0 implementation:* returns the input verbatim and
/// reports zero rewrites so the crate compiles & tests run.
pub fn rewrite(sql: &str) -> Result<RewriteOutcome> {
    Ok(RewriteOutcome {
        sql: sql.to_string(),
        converted: 0,
    })
}

/// Parse the input SQL into a `Statement`.  
/// Helper kept separate so later phases can reuse / extend it.
fn parse_sql(sql: &str) -> Result<Statement> {
    let dialect = GenericDialect {};
    let mut statements = Parser::parse_sql(&dialect, sql)
        .map_err(|e| DataFusionError::Plan(format!("SQL parse error: {e}")))?;
    // For now we expect exactly one statement.
    statements
        .pop()
        .ok_or_else(|| DataFusionError::Plan("Empty SQL string".into()))
}

///////////////////////////////////////////////////////////////////////////////
/// Tests
///////////////////////////////////////////////////////////////////////////////
#[cfg(test)]
mod tests {
    use super::*;

    // Just a smoke-test: compile & round-trip the string.
    #[tokio::test]
    async fn rewrite_noop_roundtrip() -> Result<()> {
        let original = "SELECT 1";
        let outcome = rewrite(original)?;
        assert_eq!(outcome.sql, original);
        assert_eq!(outcome.converted, 0);
        Ok(())
    }

    // Placeholder for where the “example with scalar sub-query” test will go.
    #[tokio::test]
    #[ignore = "implemented in Phase-1"]
    async fn convert_single_scalar_subquery() -> Result<()> {
        todo!("add real test in the visitor-phase")
    }
}
