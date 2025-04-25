use sqlparser::ast::*;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::collections::HashSet;
use std::ops::ControlFlow;
use uuid::Uuid;


fn alias_projection(select: &mut Select) {
    let mut new_proj = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::UnnamedExpr(expr) => match expr {
                Expr::Wildcard(_) | Expr::QualifiedWildcard(_, _) => {
                    new_proj.push(SelectItem::UnnamedExpr(expr.clone()));
                }
                _ => {
                    let alias = format!("\"{}\"", Uuid::new_v4().simple());
                    new_proj.push(SelectItem::ExprWithAlias {
                        expr: expr.clone(),
                        alias: Ident::new(alias),
                    });
                }
            },
            _ => new_proj.push(item.clone()),
        }
    }
    select.projection = new_proj;
}

fn walk_set_expr(expr: &mut SetExpr) {
    match expr {
        SetExpr::Select(select) => {
            alias_projection(select);
        }
        SetExpr::SetOperation { left, right, .. } => {
            walk_set_expr(left);
            walk_set_expr(right);
        }
        SetExpr::Query(subquery) => {
            walk_query(subquery);
        }
        _ => {}
    }
}

fn walk_query(query: &mut Query) {
    walk_set_expr(&mut query.body);

    if let Some(with) = &mut query.with {
        for cte in &mut with.cte_tables {
            walk_query(&mut cte.query);
        }
    }
}

pub fn alias_all_columns(sql: &str) -> String {
    let dialect = PostgreSqlDialect {};
    let mut statements = Parser::parse_sql(&dialect, sql).unwrap();

    let _ = visit_statements_mut(&mut statements, |stmt| {
        if let Statement::Query(query) = stmt {
            walk_query(query);
        }
        ControlFlow::<()>::Continue(())
    });

    let res = statements
        .into_iter()
        .map(|stmt| stmt.to_string())
        .collect::<Vec<_>>()
        .join(" ");

    println!("result: {:?}", res);

    res
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn test_alias_all_columns() -> Result<(), Box<dyn Error>> {
        let cases = vec![
            (
                "SELECT t.id FROM foo",
                vec!["SELECT t.id AS", "FROM foo"],
            ),
            (
                "SELECT t.id AS f FROM foo",
                vec!["SELECT t.id AS f FROM foo"], // Should stay the same
            ),
            (
                "SELECT t.* FROM foo",
                vec!["SELECT t.* FROM foo"], // No aliasing needed
            ),
            (
                "SELECT t.id, t.* FROM foo",
                vec!["SELECT t.id AS", "t.*", "FROM foo"], // Only t.id gets alias
            ),
            (
                "SELECT 1 FROM foo",
                vec!["SELECT 1 AS", "FROM foo"], // literal should also get alias
            ),
            (
                "SELECT t.id + 1 FROM foo",
                vec!["SELECT t.id + 1 AS", "FROM foo"], // expressions get alias
            ),

            (
                "WITH cte AS (SELECT t.a FROM t) SELECT * FROM cte",
                vec!["SELECT t.a AS", "SELECT * FROM cte"], // t.a gets alias, SELECT * stays
            ),

            (
                "select * from (SELECT t.a FROM t) T1",
                vec!["SELECT t.a AS", ""], // TODO: fix the test case
            ),
        ];

        for (input, expected_substrings) in cases {
            let transformed = alias_all_columns(input);
            for expected in expected_substrings {
                assert!(
                    transformed.contains(expected),
                    "Expected substring not found:\ninput: {}\nexpected: {}\nactual: {}",
                    input,
                    expected,
                    transformed
                );
            }
        }

        Ok(())
    }
}
