use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::collections::HashSet;
use std::ops::ControlFlow;

/// Add columns referenced inside `= ANY(...)` predicates to the
/// `GROUP BY` clause so queries grouping on such expressions pass
/// semantic analysis.
pub fn rewrite_group_by_for_any(sql: &str) -> String {
    use sqlparser::ast::{
        visit_statements_mut, Expr, GroupByExpr, Ident, SelectItem, SetExpr, Statement,
    };

    let dialect = PostgreSqlDialect {};
    let mut statements = match Parser::parse_sql(&dialect, sql) {
        Ok(v) => v,
        Err(_) => return sql.to_string(),
    };

    fn extract_any_column_name(e: &Expr) -> Option<String> {
        // naive textual scan is enough for our limited patterns `'lit' = ANY(col)`
        let s = e.to_string().replace(' ', "");
        let up = s.to_uppercase();
        if let Some(p) = up.find("ANY(") {
            let start = p + 4;
            if let Some(end) = up[start..].find(')') {
                let arg = &s[start..start + end];
                // Only a *column* reference is meaningful to add to GROUP BY. An
                // array literal - `= ANY(ARRAY['a','d'])` or `= ANY('{a,d}')` -
                // has no column to group on; adding it produces a bogus
                // `GROUP BY ARRAY[...]` (this is how the `columns` view tripped).
                let argup = arg.to_uppercase();
                if argup.starts_with("ARRAY[") || arg.starts_with('{') || arg.starts_with('\'') {
                    return None;
                }
                return Some(arg.to_string());
            }
        }
        None
    }

    let mut touched = false;

    let _cf = visit_statements_mut(&mut statements, |stmt| {
        if let Statement::Query(q) = stmt {
            if let SetExpr::Select(sel) = q.body.as_mut() {
                // only deal with GROUP BY <exprs>, ignore GROUP BY ALL etc.
                let exprs = match &mut sel.group_by {
                    GroupByExpr::Expressions(vec, _) => vec,
                    _ => return ControlFlow::<()>::Continue(()),
                };

                // sqlparser models "no GROUP BY" as an *empty* expression list,
                // so an empty `exprs` means the query never grouped at all. Don't
                // fabricate a GROUP BY from `= ANY(...)` predicates in that case -
                // only augment a GROUP BY the query already has.
                if exprs.is_empty() {
                    return ControlFlow::<()>::Continue(());
                }

                let mut seen: HashSet<String> =
                    exprs.iter().map(|e| e.to_string().to_lowercase()).collect();

                for item in &sel.projection {
                    let expr = match item {
                        SelectItem::UnnamedExpr(e) => e,
                        SelectItem::ExprWithAlias { expr: e, .. } => e,
                        _ => continue,
                    };
                    if let Some(any_column_name) = extract_any_column_name(expr) {
                        let key = any_column_name.to_lowercase();
                        if !seen.contains(&key) {
                            let new_e = if any_column_name.contains('.') {
                                Expr::CompoundIdentifier(
                                    any_column_name.split('.').map(Ident::new).collect(),
                                )
                            } else {
                                Expr::Identifier(Ident::new(any_column_name))
                            };
                            exprs.push(new_e);
                            seen.insert(key);
                            touched = true;
                        }
                    }
                }
            }
        }
        std::ops::ControlFlow::Continue(())
    });

    if !touched {
        return sql.to_string();
    }

    statements
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adds_column_in_any_to_group_by() {
        let input = "SELECT a, 'x' = ANY(b) FROM t GROUP BY a";
        let output = rewrite_group_by_for_any(input);
        assert_eq!(output, "SELECT a, 'x' = ANY(b) FROM t GROUP BY a, b");
    }

    #[test]
    fn test_noop_if_already_in_group_by() {
        let input = "SELECT a, 'x' = ANY(b) FROM t GROUP BY a, b";
        let output = rewrite_group_by_for_any(input);
        assert_eq!(output, input);
    }

    #[test]
    fn test_noop_when_no_group_by() {
        // A query with no GROUP BY must not gain one from `= ANY(...)`.
        let input = "SELECT a, 'x' = ANY(b) FROM t";
        let output = rewrite_group_by_for_any(input);
        assert!(
            !output.to_uppercase().contains("GROUP BY"),
            "no GROUP BY fabricated: {output}"
        );
    }

    #[test]
    fn test_noop_on_array_literal_any_arg() {
        // `= ANY(ARRAY[...])` has no column to group on (the `columns` view case).
        let input = "SELECT a, x = ANY(ARRAY['a', 'd']) FROM t GROUP BY a";
        let output = rewrite_group_by_for_any(input);
        assert_eq!(
            output, "SELECT a, x = ANY(ARRAY['a', 'd']) FROM t GROUP BY a",
            "array-literal ANY arg not added to GROUP BY"
        );
    }

    #[test]
    fn test_noop_on_non_query() {
        let input = "CREATE TABLE x (a INT)";
        let output = rewrite_group_by_for_any(input);
        assert_eq!(output, input);
    }

    #[test]
    fn test_compound_identifier_in_any() {
        let input = "SELECT a, 'x' = ANY(t.b) FROM t GROUP BY a";
        let output = rewrite_group_by_for_any(input);
        assert_eq!(output, "SELECT a, 'x' = ANY(t.b) FROM t GROUP BY a, t.b");
    }
}
