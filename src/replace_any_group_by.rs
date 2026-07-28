use sqlparser::ast::{
    visit_statements_mut, Expr, GroupByExpr, Ident, SelectItem, SetExpr, Statement,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::collections::HashSet;
use std::ops::ControlFlow;

/// Return the column referenced by the argument of an `ANY(...)` call inside
/// `expr`, if there is one.
///
/// Returns `None` when `expr` contains no `ANY(...)` call, when its argument is
/// unterminated, or when the argument is an array literal - `= ANY(ARRAY['a',
/// 'd'])`, `= ANY('{a,d}')` or a quoted string - because a literal has no
/// column to group on and grouping on it would produce a bogus
/// `GROUP BY ARRAY[...]`.
fn extract_any_column_name(expr: &Expr) -> Option<String> {
    // A naive textual scan is enough for the limited `'lit' = ANY(col)` shapes
    // this rewrite targets.
    let rendered = expr.to_string().replace(' ', "");
    let upper = rendered.to_uppercase();
    let any_call = upper.find("ANY(")?;
    let start = any_call + 4;
    let end = upper[start..].find(')')?;
    let arg = &rendered[start..start + end];
    let arg_upper = arg.to_uppercase();
    if arg_upper.starts_with("ARRAY[") || arg.starts_with('{') || arg.starts_with('\'') {
        return None;
    }
    Some(arg.to_string())
}

/// Add columns referenced inside `= ANY(...)` predicates to the
/// `GROUP BY` clause so queries grouping on such expressions pass
/// semantic analysis.
///
/// The input is returned unchanged when it does not parse as `PostgreSQL` SQL and
/// when no column had to be added, so a query that needs no rewrite keeps its
/// original text instead of being reprinted by the parser.
#[must_use]
pub fn rewrite_group_by_for_any(sql: &str) -> String {
    let dialect = PostgreSqlDialect {};
    let Ok(mut statements) = Parser::parse_sql(&dialect, sql) else {
        return sql.to_string();
    };

    let mut touched = false;

    let _cf = visit_statements_mut(&mut statements, |stmt| {
        if let Statement::Query(q) = stmt {
            if let SetExpr::Select(sel) = q.body.as_mut() {
                // only deal with GROUP BY <exprs>, ignore GROUP BY ALL etc.
                let GroupByExpr::Expressions(exprs, _) = &mut sel.group_by else {
                    return ControlFlow::<()>::Continue(());
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
                    let (SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. }) =
                        item
                    else {
                        continue;
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

    /// A column used only inside `= ANY(...)` is appended to an existing
    /// GROUP BY.
    #[test]
    fn test_adds_column_in_any_to_group_by() {
        let input = "SELECT a, 'x' = ANY(b) FROM t GROUP BY a";
        let output = rewrite_group_by_for_any(input);
        assert_eq!(output, "SELECT a, 'x' = ANY(b) FROM t GROUP BY a, b");
    }

    /// A column already listed in GROUP BY is not duplicated, and the SQL is
    /// returned verbatim rather than reprinted.
    #[test]
    fn test_noop_if_already_in_group_by() {
        let input = "SELECT a, 'x' = ANY(b) FROM t GROUP BY a, b";
        let output = rewrite_group_by_for_any(input);
        assert_eq!(output, input);
    }

    /// An ungrouped query never gains a GROUP BY clause.
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

    /// An array literal argument to `ANY(...)` is never added to GROUP BY.
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

    /// Statements that are not queries pass through untouched.
    #[test]
    fn test_noop_on_non_query() {
        let input = "CREATE TABLE x (a INT)";
        let output = rewrite_group_by_for_any(input);
        assert_eq!(output, input);
    }

    /// A qualified column such as `t.b` is added as a compound identifier, so
    /// the printed GROUP BY keeps the table qualifier.
    #[test]
    fn test_compound_identifier_in_any() {
        let input = "SELECT a, 'x' = ANY(t.b) FROM t GROUP BY a";
        let output = rewrite_group_by_for_any(input);
        assert_eq!(output, "SELECT a, 'x' = ANY(t.b) FROM t GROUP BY a, t.b");
    }
}
