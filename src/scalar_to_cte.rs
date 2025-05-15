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

////////////////////////////////////////////////////////////////
/// Mutating rewriter  – Phase-3 skeleton
////////////////////////////////////////////////////////////////
mod rewriter {
    use super::*;

    #[derive(Debug)]
    struct PendingRewrite<'a> {
        expr_slot : &'a mut Expr,   
        info      : CorrelatedInfo, 
    }

    #[derive(Debug)]
    struct CorrelatedInfo {
        cte_ident : Ident,
        subquery  : Box<Query>,
        on_pairs  : Vec<EqPair>,
    }

    // ------------------------------------------------------------
    // ★ Correlation discovery utilities
    // ------------------------------------------------------------

    /// `t1.id = t2.id`  →  `(outer=id, inner=id)`
    #[derive(Debug, Clone)]
    struct EqPair {
        outer: Vec<Ident>,
        inner: Vec<Ident>,
    }

    /// walk a boolean expression and collect `outer = inner` pairs
    fn collect_eq_pairs(e: &Expr, outer_alias: &Ident, out: &mut Vec<EqPair>) {
        match e {
            Expr::BinaryOp { op: BinaryOperator::And, left, right } => {
                collect_eq_pairs(left, outer_alias, out);
                collect_eq_pairs(right, outer_alias, out);
            }
            Expr::BinaryOp { op: BinaryOperator::Eq, left, right } => {
                let (l, r) = (as_path(left), as_path(right));
                match (l.first(), r.first()) {
                    (Some(a), Some(b)) if a == outer_alias && b != outer_alias => {
                        out.push(EqPair { outer: l, inner: r });
                    }
                    (Some(a), Some(b)) if b == outer_alias && a != outer_alias => {
                        out.push(EqPair { outer: r, inner: l });
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// helper – CompoundIdentifier to Vec<Ident>, otherwise []
    fn as_path(e: &Expr) -> Vec<Ident> {
        match e {
            Expr::CompoundIdentifier(p) => p.clone(),
            _ => vec![],
        }
    }


    #[derive(Default)]
    pub(super) struct ScalarToCte {
        pub converted : usize,
        cte_counter   : usize,
    }

    impl ScalarToCte {
        pub fn new() -> Self { Self::default() }

        /* ---------- helpers ---------- */


        fn make_from_entry(alias: &Ident) -> TableWithJoins {
            let tmpl = super::parse_sql(&format!("SELECT * FROM {alias}")).unwrap();
            if let Statement::Query(q) = tmpl {
                if let SetExpr::Select(sel) = q.body.as_ref() {
                    return sel.from[0].clone();
                }
            }
            unreachable!("template changed")
        }

        fn make_cross_join(alias: &Ident) -> Join {
            // dummy base table so we can grab the Join node
            let tmp = super::parse_sql(&format!("SELECT * FROM x CROSS JOIN {alias}"))
                .expect("parser");
            if let Statement::Query(q) = tmp {
                if let SetExpr::Select(sel) = q.body.as_ref() {
                    return sel.from[0].joins[0].clone();
                }
            }
            unreachable!("template shape changed")
        }
    
        fn make_left_join(alias: &Ident) -> Join {
            let tmp = super::parse_sql(&format!(
                "SELECT * FROM x LEFT JOIN {alias} ON true"
            ))
            .expect("parser");
        
            if let Statement::Query(q) = tmp {
                if let SetExpr::Select(sel) = q.body.as_ref() {
                    return sel.from[0].joins[0].clone();
                }
            }
            unreachable!("template shape changed")
        }

        fn fresh_name(&mut self) -> Ident {
            self.cte_counter += 1;
            Ident::new(format!("__cte{}", self.cte_counter))
        }

        fn blank_with() -> With {
            let stmt = super::parse_sql("WITH x AS (SELECT 1) SELECT 1").unwrap();
            match stmt {
                Statement::Query(q) => {
                    let mut w = q.with.unwrap();
                    w.cte_tables.clear();
                    w
                }
                _ => unreachable!(),
            }
        }

        fn make_cte(alias: &Ident, subq: Box<Query>) -> Cte {
            let s = super::parse_sql(&format!("WITH {alias} AS (SELECT 1) SELECT 1")).unwrap();
            let mut cte = match s {
                Statement::Query(q) => q.with.unwrap().cte_tables.into_iter().next().unwrap(),
                _ => unreachable!(),
            };
            cte.alias.name = alias.clone();
            cte.query      = subq;
            cte
        }

        fn ensure_with<'a>(&mut self, w: &'a mut Option<With>) -> &'a mut With {
            if w.is_none() {
                *w = Some(Self::blank_with());
            }
            w.as_mut().unwrap()
        }
        fn analyse_scalar(&mut self, e: &Expr, outer_alias: &Ident) -> Option<CorrelatedInfo> {
            let Expr::Subquery(sub) = e else { return None };
        
            // we only support plain SELECT sub-queries for now
            if let SetExpr::Select(inner_sel) = sub.body.as_ref() {
                let mut pairs = Vec::<EqPair>::new();
                if let Some(pred) = &inner_sel.selection {
                    collect_eq_pairs(pred, outer_alias, &mut pairs);
                }
                Some(CorrelatedInfo{
                    cte_ident : self.fresh_name(),
                    subquery  : sub.clone(),
                    on_pairs  : pairs,
                })
            } else {
                None
            }
        }

        fn push_cte(&mut self, outer_with: &mut Option<With>, info: &CorrelatedInfo) {
            let w = self.ensure_with(outer_with);
            w.cte_tables.push(Self::make_cte(&info.cte_ident, info.subquery.clone()));
        }
        
        fn add_join(&mut self, sel: &mut Select, info: &CorrelatedInfo) {
            if sel.from.is_empty() {
                sel.from.push(Self::make_from_entry(&info.cte_ident));
            } else {
                sel.from[0]
                    .joins
                    .push(self.build_left_join(&info.cte_ident, &info.on_pairs));
            }
        }
        

        fn make_ref(info: &CorrelatedInfo) -> Expr {
            Expr::CompoundIdentifier(vec![
                info.cte_ident.clone(),
                Ident::new("col")
            ])
        }


        fn build_left_join(&self, alias: &Ident, pairs: &[EqPair]) -> Join {
            let mut join = Self::make_left_join(alias);
        
            if !pairs.is_empty() {
                // helper: replace first identifier in a path with the CTE alias
                let rewrite_inner = |path: &Vec<Ident>| -> Expr {
                    let mut new = path.clone();
                    new[0] = alias.clone();
                    Expr::CompoundIdentifier(new)
                };
        
                // first equality
                let first = &pairs[0];
                let mut on_expr = Expr::BinaryOp {
                    left  : Box::new(Expr::CompoundIdentifier(first.outer.clone())),
                    op    : BinaryOperator::Eq,
                    right : Box::new(rewrite_inner(&first.inner)),
                };
        
                // AND-chain the rest
                for p in &pairs[1..] {
                    let eq = Expr::BinaryOp {
                        left  : Box::new(Expr::CompoundIdentifier(p.outer.clone())),
                        op    : BinaryOperator::Eq,
                        right : Box::new(rewrite_inner(&p.inner)),
                    };
                    on_expr = Expr::BinaryOp {
                        left  : Box::new(on_expr),
                        op    : BinaryOperator::And,
                        right : Box::new(eq),
                    };
                }
        
                join.join_operator = JoinOperator::LeftOuter(JoinConstraint::On(on_expr));
            }
            join
        }
        

        /* ---------- mut-visitor ---------- */

        pub fn visit_statement_mut(&mut self, s: &mut Statement) {
            if let Statement::Query(q) = s {
                self.visit_query_mut(q);
            }
        }

        fn visit_query_mut(&mut self, q: &mut Box<Query>) {
            if let SetExpr::Select(sel) = q.body.as_mut() {
                self.visit_select_mut(&mut q.with, sel);
            }
        }

        fn visit_select_mut(&mut self, w: &mut Option<With>, sel: &mut Select) {
            // ---------- outer alias (very first table name / alias) ----------
            let outer_alias: Ident = sel
            .from
            .get(0)
            .and_then(|twj| match &twj.relation {
                // explicit alias  →  use it
                TableFactor::Table { alias: Some(a), .. }
                | TableFactor::Derived { alias: Some(a), .. } => Some(a.name.clone()),
                // otherwise   first identifier of the table name
                TableFactor::Table { name, .. } => match name.0.first() {
                    Some(ObjectNamePart::Identifier(id)) => Some(id.clone()),
                    _ => None,
                },
                _ => None,
            })
            .unwrap_or_else(|| Ident::new("_outer"));

            // ---------- 1st pass: collect what needs rewriting ----------
            let mut collected = Vec::<(usize, CorrelatedInfo)>::new();
            for (idx, item) in sel.projection.iter().enumerate() {
                if let SelectItem::UnnamedExpr(e)
                | SelectItem::ExprWithAlias { expr: e, .. } = item
                {
                    if let Some(info) = self.analyse_scalar(e, &outer_alias) {
                        collected.push((idx, info));
                    }
                }
            }

            // ---------- 2nd pass: inject CTEs & JOINs ----------
            for (_, info) in &collected {
                self.push_cte(w, info);
                self.add_join(sel, info);
            }

            // ---------- 3rd pass: patch projection expressions ----------
            for (idx, info) in collected {
                if let SelectItem::UnnamedExpr(e)
                | SelectItem::ExprWithAlias { expr: e, .. } =
                    &mut sel.projection[idx]
                {
                    *e = Self::make_ref(&info);
                    self.converted += 1;
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

    #[test]
    fn scalar_becomes_join() -> Result<()> {
        let q = "SELECT (SELECT 1) FROM t";
        let out = rewrite(q)?;
        println!("scalar_becomes_join {:?}", out);
        assert_eq!(out.converted, 1);
        assert!(out.sql.contains("WITH"));
        assert!(out.sql.contains("JOIN"));
        Ok(())
    }

    #[test]
    fn cte_is_injected() -> Result<()> {
        let q = "SELECT (SELECT 42)";
        let out = rewrite(q)?;
        assert_eq!(out.converted, 1);
        assert!(out.sql.starts_with("WITH"));
        assert!(out.sql.contains("__cte1"));
        Ok(())
    }


    #[test]
    fn rewrite_equality_join() -> Result<()> {
        let q = "SELECT (SELECT max(b) FROM t2 WHERE t2.id = t1.id) FROM t1";
        let out = rewrite(q)?;
        println!("rewrite_equality_join {:?}", out);
        assert!(out.sql.contains("LEFT OUTER JOIN __cte1"));
        assert!(out.sql.contains("t1.id = __cte1.id"));
        Ok(())
    }

}