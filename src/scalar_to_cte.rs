#![allow(unused_imports)]
/*───────────────────────────────────────────────────────────────────────────────
  Scalar-subquery-to-CTE re-writer
  ──────────────────────────────────────────────────────────────────────────────

  WHY WE BUILT IT
  ═══════════════
  DataFusion’s logical plan *hates* correlated scalar sub-queries:
    • they prevent predicate push-down and join re-ordering,
    • they’re rewritten into a naïve “pull up every row, evaluate per row”
      execution which is disastrously slow on large tables.

  Turning…

      SELECT …,
             (SELECT max(b)
              FROM   t2
              WHERE  t2.id = t1.id)      -- correlated scalar
      FROM t1

  …into…

      WITH __cte1 AS (
          SELECT max(b), t2.id           -- key(s) & scalar value
          FROM   t2
          GROUP BY t2.id
      )
      SELECT …,
             __cte1.col                  -- scalar becomes simple column ref
      FROM t1
      LEFT JOIN __cte1 ON t1.id = __cte1.id

  removes the correlation barrier: the optimiser sees only joins + a WITH
  block, all of which it already handles well.

  PARKING-LOT – IDEAS / TODOS
  ═══════════════════════════
    1. EXISTS / NOT EXISTS   ─ rewrite into semi-/anti-joins.
    2. UNION / INTERSECT     ─ support set-ops inside the scalar sub-query.
    3. Complex projections   ─ sub-queries embedded in wider expressions.
    4. General “outer-only” predicates (t1.x > 10, t1.flag = 1, …).
    5. Multiple scalars in one expression (cte1.col + cte2.col).
    6. Stable synthetic alias numbering across nested rewrites.
    7. Avoid name clashes if inner query already exposes a `col` column.
    8. Cache helper template parses for speed.
    9. Pretty printer for the resulting SQL (line breaks, indent).
   10. Deep-nesting unit tests (scalar within scalar within …).

  ─────────────────────────────────────────────────────────────────────────────*/

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

pub fn rewrite_subquery_as_cte(sql: &str) -> String {
    let out = rewrite(sql);
    out.unwrap().sql
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
    #[allow(dead_code)]
    pub struct ScalarFinder {
        pub scalars: Vec<Expr>,
    }

    #[allow(dead_code)]
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
    #[allow(dead_code)]
    struct PendingRewrite<'a> {
        expr_slot : &'a mut Expr,   
        info      : CorrelatedInfo, 
    }

    #[derive(Debug)]
    #[allow(dead_code)]
    struct CorrelatedInfo {
        cte_ident   : Ident,
        subquery    : Box<Query>,
        on_pairs    : Vec<CorrPred>,  // t1.id = t2.id ...
        outer_only  : Vec<Expr>,      // t1.flag, t1.x > 10, …
        orig_alias  : Option<Ident>,
        outer_alias : Ident,
    }

    // ------------------------------------------------------------
    // ★ Correlation discovery utilities
    // ------------------------------------------------------------


    /// walk the expression and return *all* column paths it contains
    fn collect_paths(e: &Expr, out: &mut Vec<Vec<Ident>>) {
        match e {
            Expr::CompoundIdentifier(p) => out.push(p.clone()),
            Expr::BinaryOp { left, right, .. } => {
                collect_paths(left, out);
                collect_paths(right, out);
            }
            Expr::UnaryOp { expr, .. }
            | Expr::Nested(expr) => collect_paths(expr, out),

            Expr::Cast { expr, .. } => collect_paths(expr, out),
            Expr::Case {
                operand,
                conditions,
                else_result,
            } => {
                if let Some(op) = operand {
                    collect_paths(op, out);
                }
                for CaseWhen { condition, result } in conditions {
                    collect_paths(condition, out);
                    collect_paths(result, out);
                }
                if let Some(er) = else_result {
                    collect_paths(er, out);
                }
            }
            _ => {}
        }
    }

    /// does this conjunct refer **only** to the outer alias?
    fn is_outer_only(e: &Expr, outer: &Ident) -> bool {
        let mut paths = vec![];
        collect_paths(e, &mut paths);

        // at least one reference to the outer alias …
        if !paths.iter().any(|p| p.first() == Some(outer)) {
            return false;
        }
        // … and *no* reference to any other alias
        paths
            .iter()
            .all(|p| p.first() == Some(outer))
    }


    /// `t1.id = t2.id`  →  `(outer=id, inner=id)`
    #[allow(dead_code)]
    #[derive(Debug, Clone)]
    struct EqPair {
        outer: Vec<Ident>,
        inner: Vec<Ident>,
    }

    /// One correlated comparison: `t1.x <> t2.y`
    #[derive(Debug, Clone)]
    struct CorrPred {
        outer: Vec<Ident>,
        inner: Vec<Ident>,
        op   : BinaryOperator,        // =  <>  <  <=  >  >=
    }

    /// walk a boolean expression and collect `outer = inner` pairs
    fn collect_corr_preds(e: &Expr, outer_alias: &Ident, out: &mut Vec<CorrPred>) {
        match e {
            Expr::BinaryOp { op: BinaryOperator::And, left, right } => {
                collect_corr_preds(left,  outer_alias, out);
                collect_corr_preds(right, outer_alias, out);
            }

            Expr::BinaryOp { op, left, right }
                if matches!(op,
                    BinaryOperator::Eq
                  | BinaryOperator::NotEq
                  | BinaryOperator::Lt
                  | BinaryOperator::LtEq
                  | BinaryOperator::Gt
                  | BinaryOperator::GtEq) =>
            {
                let (l, r) = (as_path(left), as_path(right));
                match (l.first(), r.first()) {
                    (Some(a), Some(b)) if a == outer_alias && b != outer_alias => {
                        out.push(CorrPred { outer: l, inner: r, op: op.clone() });
                    }
                    (Some(a), Some(b)) if b == outer_alias && a != outer_alias => {
                            out.push(CorrPred { outer: r, inner: l, op: op.clone() });
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

    /// flatten `a AND b AND c` → `[a, b, c]`
    fn split_and(e: &Expr) -> Vec<Expr> {
        match e {
            Expr::BinaryOp {
                op: BinaryOperator::And,
                left,
                right,
            } => {
                let mut v = split_and(left);
                v.extend(split_and(right));
                v
            }
            other => vec![other.clone()],
        }
    }

    /// rebuild AND-chain; returns `None` if `parts` is empty
    fn build_and(mut parts: Vec<Expr>) -> Option<Expr> {
        match parts.len() {
            0 => None,
            1 => Some(parts.pop().unwrap()),
            _ => {
                let right = parts.pop().unwrap();
                let left  = build_and(parts).unwrap();
                Some(Expr::BinaryOp {
                    left : Box::new(left),
                    op   : BinaryOperator::And,
                    right: Box::new(right),
                })
            }
        }
    }

    /// does this boolean expression represent *exactly* the same
    /// correlated predicate that we lifted into `pairs`?
    fn is_same_pred(e: &Expr, p: &CorrPred) -> bool {
        if let Expr::BinaryOp { op, left, right } = e {
            if op == &p.op {
                return  (as_path(left)  == p.outer && as_path(right) == p.inner)
                    || (as_path(right) == p.outer && as_path(left)  == p.inner);
            }
        }
        false
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

        #[allow(dead_code)]
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
        fn analyse_scalar(&mut self, sel_item : &SelectItem,  outer_alias: &Ident) -> Option<CorrelatedInfo> {
            
            let expr = match sel_item {
                SelectItem::UnnamedExpr(e)
                | SelectItem::ExprWithAlias { expr: e, .. } => e,
                _ => return None,
            };
        
            let Expr::Subquery(sub) = expr else { return None };

        
            // we only support plain SELECT sub-queries for now
            if let SetExpr::Select(inner_sel) = sub.body.as_ref() {
                let mut pairs      = Vec::<CorrPred>::new();
                let mut outer_only = Vec::<Expr>::new();

                if let Some(pred) = &inner_sel.selection {
                    for c in split_and(pred) {
                        if is_outer_only(&c, outer_alias) {
                            outer_only.push(c);
                        }
                    }
                    collect_corr_preds(pred, outer_alias, &mut pairs);
                }

                Some(CorrelatedInfo{
                    cte_ident : self.fresh_name(),
                    subquery  : sub.clone(),
                    on_pairs  : pairs,
                    outer_only,
                    outer_alias: outer_alias.clone(),
                    orig_alias: match sel_item {
                        SelectItem::ExprWithAlias { alias, .. } => Some(alias.clone()),
                        _ => None,
                    },
                })
            } else {
                None
            }
        }

        fn push_cte(&mut self, outer_with: &mut Option<With>, info: &CorrelatedInfo) {
            let w = self.ensure_with(outer_with);
        
            // --- clone & strip correlated filters ------------------
            let mut subq = (*info.subquery).clone();
            if let SetExpr::Select(inner_sel) = subq.body.as_mut() {
                    Self::strip_corr_filters(inner_sel,
                                                &info.on_pairs,
                                                &info.outer_alias);


                    /* --------------------------------------------------------
                     * 1.  Ensure the *scalar value* itself is exposed
                     *     as  col  inside the CTE so the outer query
                     *     can safely reference  __cteN.col
                     * --------------------------------------------------------*/
                    // ---- make the first projection look like  expr AS col --------------
                    if let Some(SelectItem::UnnamedExpr(expr)) = inner_sel.projection.first().cloned() {
                        // overwrite the first entry in-place
                        inner_sel.projection[0] = SelectItem::ExprWithAlias {
                            expr,
                            alias: Ident::new("col"),
                        };
                    }

                    
                    // helper – add "col_path" unless already there
                    let mut ensure_proj = |path: &Vec<Ident>| {
                        let already = inner_sel.projection.iter().any(|item| {
                            matches!(item,
                                SelectItem::UnnamedExpr(
                                    Expr::CompoundIdentifier(p)) if p == path)
                        });
                        if !already {
                            inner_sel.projection.push(
                                SelectItem::UnnamedExpr(
                                    Expr::CompoundIdentifier(path.clone()))
                            );
                        }
                    };
                    
                    // ---- gather every inner-side column that will be used by the JOIN ----
                    let mut need: Vec<Vec<Ident>> = Vec::new();
                    for p in &info.on_pairs {
                        if !need.contains(&p.inner) {
                            need.push(p.inner.clone());
                        }
                    }


                    for p in need { ensure_proj(&p); }

            }
        
            // ★ use *subq* we just cleaned, not the original
            w.cte_tables
                .push(Self::make_cte(&info.cte_ident, Box::new(subq)));  
        }

        fn add_join(&mut self, sel: &mut Select, info: &CorrelatedInfo) {
            if sel.from.is_empty() {
                sel.from.push(Self::make_from_entry(&info.cte_ident));
            } else {
                sel.from[0]
                    .joins
                    .push(
                        self.build_left_join(
                            &info.cte_ident,
                            &info.on_pairs,
                            &info.outer_only,
                        )
                    );
            }
        }

        fn strip_corr_filters(sel: &mut Select,
                                     pairs: &[CorrPred],
                                     outer_alias: &Ident) {
            
            if let Some(pred) = &sel.selection {
                let mut keep: Vec<Expr> = vec![];
                for conjunct in split_and(pred) {          // helper to de-AND
                    let lifted = pairs.iter().any(|p| is_same_pred(&conjunct, p));
                    if !lifted && !is_outer_only(&conjunct, outer_alias) {
                        if !pairs.iter().any(|p| is_same_pred(&conjunct, p)) {
                            keep.push(conjunct);
                        }
                    }
                }
                sel.selection = build_and(keep);           // None if empty
            }

        }
             
        fn make_ref(info: &CorrelatedInfo) -> Expr {
            Expr::CompoundIdentifier(vec![
                info.cte_ident.clone(),
                Ident::new("col"),
            ])
        }


        fn build_left_join(
                &self,
                alias      : &Ident,
                pairs      : &[CorrPred],
                outer_only : &[Expr],
        ) -> Join {
            let mut join = Self::make_left_join(alias);
            let mut on_expr: Option<Expr> = None;

            if !pairs.is_empty() {
                // helper: replace first identifier in a path with the CTE alias
                let rewrite_inner = |path: &Vec<Ident>| -> Expr {
                    let mut new = path.clone();
                    new[0] = alias.clone();
                    Expr::CompoundIdentifier(new)
                };
        
                // first equality
                let first = &pairs[0];
                let mut expr = Expr::BinaryOp {
                    left  : Box::new(Expr::CompoundIdentifier(first.outer.clone())),
                    op    : first.op.clone(),
                    right : Box::new(rewrite_inner(&first.inner)),
                };
        
                // AND-chain the rest
                for p in &pairs[1..] {
                    let eq = Expr::BinaryOp {
                        left  : Box::new(Expr::CompoundIdentifier(p.outer.clone())),
                        op    : p.op.clone(),
                        right : Box::new(rewrite_inner(&p.inner)),
                    };
                    expr = Expr::BinaryOp {
                        left  : Box::new(expr),
                        op    : BinaryOperator::And,
                        right : Box::new(eq),
                    };
                }
        
                on_expr = Some(expr);
            }

            // -------- outer‑only predicates (t1.flag etc.) -----
            /* ---------- outer-only predicates (t1.flag …) ---------- */
            for pred in outer_only {
                on_expr = Some(match on_expr.take() {
                    Some(current) => Expr::BinaryOp {
                        left  : Box::new(current),
                        op    : BinaryOperator::And,
                        right : Box::new(pred.clone()),
                    },
                    None => pred.clone(),
                });

            }
            
            // If we built anything, replace the default `ON true`
            if let Some(expr) = on_expr {
                join.join_operator = JoinOperator::LeftOuter(JoinConstraint::On(expr));
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
                if let SelectItem::UnnamedExpr(_e)
                | SelectItem::ExprWithAlias { expr: _e, .. } = item
                {
                    if let Some(info) = self.analyse_scalar(item, &outer_alias) {
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
                let replacement_expr = Self::make_ref(&info);
            
                sel.projection[idx] = if let Some(alias) = info.orig_alias {
                    SelectItem::ExprWithAlias {
                        expr : replacement_expr,
                        alias,
                    }
                } else {
                    SelectItem::ExprWithAlias {
                        expr : replacement_expr,
                        alias: Ident::new(format!("subq{}", self.converted + 1)),
                    }
                };
                self.converted += 1;
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

    #[test]
    fn rewrite_inequality_join() -> Result<()> {
        let q = "
            SELECT (SELECT min(b.val)
                    FROM   t2
                    WHERE  t2.id  = t1.id
                    AND  t2.val <> t1.val)
            FROM t1";
        let out = rewrite(q)?;
        println!("rewrite_inequality_join {:?}", out);

        assert!(
                out.sql.contains("t1.val <> __cte1.val")
                || out.sql.contains("__cte1.val <> t1.val"),
                "inequality predicate not found in JOIN"
            );
        Ok(())
    }


    #[test]
    fn keeps_explicit_alias() -> Result<()> {
        let q = "SELECT (SELECT 1) AS answer";
        let out = rewrite(q)?;
        println!("keeps_explicit_alias {:?}", out);
        assert!(out.sql.contains("answer"));      // alias survived
        Ok(())
    }
    
    #[test]
    fn synthesises_alias_when_missing() -> Result<()> {
        let q = "SELECT (SELECT 1)";
        let out = rewrite(q)?;
        println!("synthesises_alias_when_missing {:?}", out);
        assert!(out.sql.contains("subq1"));       // our synthetic alias
        Ok(())
    }

    #[test]
    fn cte_strips_correlated_filters() -> Result<()> {
        let q = "SELECT (SELECT 1 FROM t2 WHERE t2.id = t1.id AND t2.flag = 'Y') FROM t1";
        let out = rewrite(q)?;

        println!("cte_strips_correlated_filters {:?} : ", out);

        let sql = out.sql;
    
        // 1) CTE body must *not* reference the outer table
        assert!(
            !sql.contains("t2.id = t1.id"),
            "outer predicate leaked into CTE"
        );
    
        // 2) join ON-clause must contain the lifted predicate
        assert!(
            sql.contains("t1.id = __cte1.id"),
            "lifted predicate missing from JOIN"
        );
    
        // 3) the non-correlated filter must still be inside the CTE
        assert!(
            sql.contains("flag = 'Y'"),
            "local filter should stay inside CTE"
        );
        Ok(())
    }


    #[test]
    fn outer_only_predicate_removed() -> Result<()> {
        let q = "SELECT (SELECT 1 FROM t2 WHERE t1.flag) FROM t1";
        let out = rewrite(q)?;
        println!("outer_only_predicate_removed {:?}", out);

        assert!(
            !out.sql.contains("FROM t2 WHERE t1.flag"),
            "predicate left in CTE"
        );
        assert!(
            out.sql.contains("ON t1.flag"),
            "predicate not copied to JOIN"
        );
        Ok(())
    }

    #[test]
    fn cte_projects_join_key() -> Result<()> {
        let q = "
            SELECT (SELECT count(*)          -- scalar sub-query
                    FROM   t2
                    WHERE  t2.id = t1.id)    -- correlated predicate
            FROM t1";
    
        let out = rewrite(q)?;
        let sql = out.sql;
    
        // 1) the inner column appears in the CTE SELECT-list
        assert!(
            sql.contains("SELECT t2.id") || sql.contains(", t2.id"),
            "join key t2.id not projected by CTE"
        );
    
        // 2) the ON-clause uses the projected column
        assert!(
            sql.contains("t1.id = __cte1.id"),
            "join predicate not rewritten with CTE column"
        );
        Ok(())
    }
    
    #[test]
    fn cte_projects_multiple_keys() -> Result<()> {
        let q = "
            SELECT (SELECT 1
                    FROM   t2
                    WHERE  t2.x = t1.x
                    AND    t2.y <> t1.y)     -- two different columns
            FROM t1";

        let out = rewrite(q)?;
        let sql = out.sql;

        // Both columns must be selected by the CTE
        for col in ["t2.x", "t2.y"] {
            assert!(
                sql.contains(col),
                "{col} not projected by CTE"
            );
        }

        // And appear (rewritten) inside the JOIN
        assert!(sql.contains("t1.x = __cte1.x"),  "x predicate missing");
        assert!(
            sql.contains("t1.y <> __cte1.y") || sql.contains("__cte1.y <> t1.y"),
            "y predicate missing"
        );
        Ok(())
    }

    // ---- full pg_catalog style query --------------------------------------------------
    // Ensures
    //   * scalar value is exposed as __cte1.col
    //   * every join-key column is projected by its CTE
    #[test]
    fn pg_catalog_query_ok() -> Result<()> {
        let q = r#"
            SELECT a.attname,
                   pg_catalog.format_type(a.atttypid, a.atttypmod),
                   (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true)
                    FROM pg_catalog.pg_attrdef d
                    WHERE d.adrelid = a.attrelid
                      AND d.adnum   = a.attnum
                      AND a.atthasdef),
                   a.attnotnull,
                   (SELECT c.collname
                    FROM pg_catalog.pg_collation c,
                         pg_catalog.pg_type      t
                    WHERE c.oid = a.attcollation
                      AND t.oid = a.atttypid
                      AND a.attcollation <> t.typcollation) AS attcollation,
                   a.attidentity,
                   a.attgenerated
            FROM pg_catalog.pg_attribute a
            WHERE a.attrelid = '50010'
              AND a.attnum  > 0
              AND NOT a.attisdropped;
        "#;
    
        let sql = rewrite(q)?.sql;
    
        // scalar exposed
        assert!(sql.contains("__cte1.col"), "scalar alias 'col' missing");
    
        // join-key columns projected by CTEs
        for k in ["d.adrelid", "d.adnum", "c.oid", "t.oid", "t.typcollation"] {
            assert!(
                sql.contains(k),
                "{k} not projected inside CTE"
            );
        }
        Ok(())
    }
}