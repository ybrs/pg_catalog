#![allow(unused_imports)]

use sqlparser::ast::*;
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

use datafusion::error::{DataFusionError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteOutcome {
    pub sql:       String,
    pub converted: usize,
}

pub fn rewrite(sql: &str) -> Result<RewriteOutcome> {
    let mut stmt = parse_sql(sql)?;

    // ------------------------------------------------------------------
    // run the *mutating* rewriter
    // ------------------------------------------------------------------
    let mut rewriter = rewriter::ScalarToCte::new();
    rewriter.visit_statement_mut(&mut stmt);
    let converted = rewriter.converted;

    // Serialise back to SQL
    let sql_rewritten = stmt.to_string();

    Ok(RewriteOutcome {
        sql: sql_rewritten,
        converted,
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

mod rewriter {
    use super::*;

    #[derive(Default)]
    pub struct ScalarToCte {
        pub converted: usize,
        cte_counter:   usize,
    }

    impl ScalarToCte {
        pub fn new() -> Self {
            Self::default()
        }

        /* ─────── helpers ─────── */
        fn fresh_cte_name(&mut self) -> Ident {
            self.cte_counter += 1;
            Ident::new(format!("__cte{}", self.cte_counter))
        }

        /// Parse a dummy `WITH x AS (SELECT 1) SELECT 1`
        /// and return a *blank* `With` node whose tokens are intact.
        fn blank_with() -> With {
            let stmt = super::parse_sql("WITH x AS (SELECT 1) SELECT 1")
                .expect("parser must succeed");
            if let Statement::Query(q) = stmt {
                let mut w = q.with.expect("template has WITH");
                w.cte_tables.clear();
                w
            } else {
                unreachable!()
            }
        }

        /// Same trick, but returns a ready-made `Cte` which we then patch.
        fn make_cte(alias: &Ident, subq: Box<Query>) -> Cte {
            let tmpl = format!("WITH {alias} AS (SELECT 1) SELECT 1");
            let stmt = super::parse_sql(&tmpl).expect("parser");
            let mut cte = if let Statement::Query(q) = stmt {
                q.with.expect("WITH").cte_tables.into_iter().next().unwrap()
            } else {
                unreachable!()
            };
            cte.alias.name = alias.clone();
            cte.query      = subq;
            cte
        }

        /// ensure outer `WITH` exists and return mut-ref
        fn ensure_with<'a>(&mut self, with_opt: &'a mut Option<With>) -> &'a mut With {
            if with_opt.is_none() {
                *with_opt = Some(Self::blank_with());
            }
            with_opt.as_mut().unwrap()
        }

        /// rewrite a scalar subquery → CTE and return replacement expr
        fn rewrite_scalar_expr(
            &mut self,
            outer_with: &mut Option<With>,
            scalar: &Expr,
        ) -> Expr {
            let subq = match scalar {
                Expr::Subquery(q) => q.clone(),
                _ => return scalar.clone(),
            };

            let cte_ident = self.fresh_cte_name();

            // push into WITH
            let with_clause = self.ensure_with(outer_with);
            with_clause
                .cte_tables
                .push(Self::make_cte(&cte_ident, subq));

            self.converted += 1;

            // replace reference in SELECT list
            Expr::CompoundIdentifier(vec![cte_ident, Ident::new("col")])
        }

        /* ─────── mut-visitors ─────── */

        pub fn visit_statement_mut(&mut self, stmt: &mut Statement) {
            if let Statement::Query(q) = stmt {
                self.visit_query_mut(q);
            }
        }

        fn visit_query_mut(&mut self, query: &mut Box<Query>) {
            if let SetExpr::Select(sel) = query.body.as_mut() {
                self.visit_select_mut(&mut query.with, sel);
            }
        }

        fn visit_select_mut(
            &mut self,
            outer_with: &mut Option<With>,
            select:     &mut Select,
        ) {
            for item in &mut select.projection {
                match item {
                    SelectItem::UnnamedExpr(expr)
                    | SelectItem::ExprWithAlias { expr, .. } => {
                        if matches!(expr, Expr::Subquery(_)) {
                            *expr = self.rewrite_scalar_expr(outer_with, expr);
                        }
                    }
                    _ => {}
                }
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

    /// Placeholder: when real rewrite lands this should assert the
    /// presence of a WITH-clause and replaced expressions.
    #[test]
    fn rewrite_does_not_panic() -> Result<()> {
        let sql = r#"
            SELECT
              a,
              (SELECT max(b) FROM t2) AS s1
            FROM t1"#;
        let out = rewrite(sql)?;
        assert!(!out.sql.is_empty());
        Ok(())
    }

    #[test]
    fn rewrite_single_scalar_to_cte() -> Result<()> {
        let sql = "SELECT (SELECT 1) AS x";
        let out = rewrite(sql)?;
        println!("out ---> {:?}", out);
        assert_eq!(out.converted, 1);
        assert!(out.sql.contains("WITH"));
        assert!(out.sql.contains("__cte1"));
        assert!(out.sql.contains("SELECT 1"));
        Ok(())
    }

    #[test]
    fn rewrite_preserves_other_columns() -> Result<()> {
        let sql = "SELECT a, (SELECT 2) AS two FROM t";
        let out = rewrite(sql)?;
        println!("out 2---> {:?}", out);
        // make sure original top-level columns still there
        assert!(out.sql.starts_with("WITH"));
        assert!(out.sql.contains("SELECT a, __cte1.col")); // rough check
        Ok(())
    }

}