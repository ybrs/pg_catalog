//! Phase-1: *Visitor utilities*
//
// We add a **read-only** walker that can locate every scalar-subquery
// (`Expr::Subquery`) appearing inside the projection list of a single
// `SELECT` query.
//
// Currently the visitor does **nothing** useful beyond collecting
// those expressions, but it sets up the scaffolding for Phase-2
// (correlation analysis / CTE building).
//
// ## File layout so far
// - `rewrite`               — public façade (Phase-0 stub)
// - `visitor::ScalarFinder` — readonly AST walker (new)
// - unit tests              — smoke-tests for the visitor
//
// Everything still compiles & `cargo test` passes.

#![allow(unused_imports)]

use sqlparser::ast::*;
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

use datafusion::error::{DataFusionError, Result};

/////////////////////////////////////////////////////////////////
/// Phase-0 façade (unchanged)
/////////////////////////////////////////////////////////////////

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteOutcome {
    pub sql:       String,
    pub converted: usize,
}

pub fn rewrite(sql: &str) -> Result<RewriteOutcome> {
    // Phase-1 still returns the input verbatim.
    let _ast = parse_sql(sql)?; // we *can* parse, but ignore the result for now
    Ok(RewriteOutcome {
        sql: sql.to_string(),
        converted: 0,
    })
}

fn parse_sql(sql: &str) -> Result<Statement> {
    let dialect = GenericDialect {};
    let mut statements = Parser::parse_sql(&dialect, sql)
        .map_err(|e| DataFusionError::Plan(format!("SQL parse error: {e}")))?;
    statements
        .pop()
        .ok_or_else(|| DataFusionError::Plan("Empty SQL string".into()))
}

/////////////////////////////////////////////////////////////////
/// Phase-1 visitor
/////////////////////////////////////////////////////////////////
mod visitor {
    use super::*;

    /// Read-only walker that records every scalar sub-query appearing
    /// *directly* inside the projection list.
    #[derive(Default, Debug)]
    pub struct ScalarFinder {
        pub scalars: Vec<Expr>,
    }

    impl ScalarFinder {
        pub fn find(stmt: &Statement) -> Self {
            let mut this = Self::default();
            this.visit_statement(stmt);
            this
        }

        /* ─────── recursive helpers ─────── */

        fn visit_statement(&mut self, stmt: &Statement) {
            if let Statement::Query(q) = stmt {
                self.visit_query(q);
            }
        }

        fn visit_query(&mut self, query: &Box<Query>) {
            if let SetExpr::Select(select) = query.body.as_ref() {
                self.visit_select(select);
            }
            // UNION / INTERSECT 👉 ignored for now
        }

        fn visit_select(&mut self, select: &Select) {
            for item in &select.projection {
                match item {
                    //  SELECT (subq)               …
                    SelectItem::UnnamedExpr(expr)
                    //  SELECT (subq) AS alias      …
                    | SelectItem::ExprWithAlias { expr, .. } => {
                        self.visit_expr(expr)
                    }
                    _ => {} // Column*, Qualified*, Wildcard, etc.
                }
            }
        }

        fn visit_expr(&mut self, expr: &Expr) {
            match expr {
                Expr::Subquery(_) => self.scalars.push(expr.clone()),
                Expr::Exists { .. } => self.scalars.push(expr.clone()), 

                // Binary
                Expr::BinaryOp { left, right, .. } => {
                    self.visit_expr(left);
                    self.visit_expr(right);
                }

                // Nested
                Expr::Nested(e) => self.visit_expr(e),


                Expr::Case {
                    operand,
                    conditions,
                    else_result,
                } => {
                    if let Some(op) = operand {
                        self.visit_expr(op);
                    }
                
                    // walk WHEN … THEN … pairs
                    for CaseWhen { condition, result } in conditions {
                        self.visit_expr(condition);
                        self.visit_expr(result);
                    }
                
                    if let Some(er) = else_result {
                        self.visit_expr(er);
                    }
                }
                
                // CAST (only one variant in this sqlparser version)
                Expr::Cast { expr, .. } => self.visit_expr(expr),

                // Unary
                Expr::UnaryOp { expr, .. } => self.visit_expr(expr),

                // everything else – literals / idents etc.
                _ => {}
            }
        }
    }
}


/////////////////////////////////////////////////////////////////
/// Tests
/////////////////////////////////////////////////////////////////
#[cfg(test)]
mod tests {
    use super::*;
    use visitor::ScalarFinder;

    #[tokio::test]
    async fn rewrite_noop_roundtrip() -> Result<()> {
        let original = "SELECT 1";
        let outcome  = rewrite(original)?;
        assert_eq!(outcome.sql, original);
        assert_eq!(outcome.converted, 0);
        Ok(())
    }

    #[test]
    fn visitor_finds_two_scalars() -> Result<()> {
        let sql = r#"
            SELECT
              a,
              (SELECT max(b) FROM t2) AS s1,
              (SELECT count(*) FROM t3 WHERE t3.x = t1.x) AS s2
            FROM t1"#;

        let stmt   = parse_sql(sql)?;
        let finder = ScalarFinder::find(&stmt);

        assert_eq!(finder.scalars.len(), 2);
        Ok(())
    }
}
