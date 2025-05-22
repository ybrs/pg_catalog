use sqlparser::ast::*;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::collections::HashSet;
use std::ops::ControlFlow;
use arrow::datatypes::DataType as ArrowDataType;
use datafusion::logical_expr::{create_udf, ColumnarValue, ScalarUDF, Volatility};
use datafusion::scalar::ScalarValue;

use sqlparser::ast::{Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, ObjectName, ObjectNamePart, Value};
use datafusion::prelude::SessionContext;
use sqlparser::ast::*;
use sqlparser::tokenizer::Span;
use sqlparser::ast::{
    visit_expressions_mut, visit_statements_mut, ValueWithSpan,
};
use sqlparser::ast::{Statement};
use sqlparser::ast::OneOrManyWithParens;
use datafusion::error::{DataFusionError, Result};


pub fn rewrite_group_by_for_any(sql: &str) -> String {
    use sqlparser::ast::{
        visit_statements_mut, Expr, GroupByExpr, Ident, SelectItem, SetExpr, Statement,
    };

    let dialect = PostgreSqlDialect {};
    let mut statements = match Parser::parse_sql(&dialect, sql) {
        Ok(v) => v,
        Err(_) => return sql.to_string(),
    };

    fn extract_any_arg(e: &Expr) -> Option<String> {
        // naive textual scan is enough for our limited patterns `'lit' = ANY(col)`
        let s = e.to_string().replace(' ', "");
        let up = s.to_uppercase();
        if let Some(p) = up.find("ANY(") {
            let start = p + 4;
            if let Some(end) = up[start..].find(')') {
                return Some(s[start..start + end].to_string());
            }
        }
        None
    }

    let mut touched = false;

    visit_statements_mut(&mut statements, |stmt| {
        if let Statement::Query(q) = stmt {
            if let SetExpr::Select(sel) = q.body.as_mut() {
                // only deal with GROUP BY <exprs>, ignore GROUP BY ALL etc.
                let exprs = match &mut sel.group_by {
                    GroupByExpr::Expressions(vec, _) => vec,
                    _ => return ControlFlow::<()>::Continue(()),
                };

                let mut seen: HashSet<String> =
                    exprs.iter().map(|e| e.to_string().to_lowercase()).collect();

                for item in &sel.projection {
                    let expr = match item {
                        SelectItem::UnnamedExpr(e) => e,
                        SelectItem::ExprWithAlias { expr: e, .. } => e,
                        _ => continue,
                    };
                    if let Some(col_txt) = extract_any_arg(expr) {
                        let key = col_txt.to_lowercase();
                        if !seen.contains(&key) {
                            let new_e = if col_txt.contains('.') {
                                Expr::CompoundIdentifier(
                                    col_txt.split('.').map(Ident::new).collect(),
                                )
                            } else {
                                Expr::Identifier(Ident::new(col_txt))
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