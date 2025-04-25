use sqlparser::ast::*;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::collections::{HashMap};
use std::ops::ControlFlow;


fn alias_projection(select: &mut Select, counter: &mut usize, alias_map: &mut HashMap<String, String>) {
    let mut new_proj = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::UnnamedExpr(expr) => match expr {
                Expr::Wildcard(_) | Expr::QualifiedWildcard(_, _) => {
                    new_proj.push(SelectItem::UnnamedExpr(expr.clone()));
                }
                _ => {
                    let alias = format!("alias_{}", *counter);
                    *counter += 1;

                    let name = match expr {
                        Expr::CompoundIdentifier(segments) => segments.last().map(|id| id.value.clone()).unwrap_or("?column?".to_string()),
                        Expr::Identifier(id) => id.value.clone(),
                        _ => "?column?".to_string(),
                    };

                    alias_map.insert(alias.clone(), name);

                    // alias_map.insert(alias.clone(), expr.to_string());
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

fn walk_set_expr(expr: &mut SetExpr,  counter: &mut usize, alias_map: &mut HashMap<String, String>) {
    match expr {
        SetExpr::Select(select) => {
            alias_projection(select, counter, alias_map );
            for table_with_joins in &mut select.from {
                match &mut table_with_joins.relation {
                    TableFactor::Derived { subquery, .. } => {
                        walk_query(subquery, counter, alias_map);
                    }
                    _ => {}
                }
            }
        }

        SetExpr::SetOperation { left, right, .. } => {
            walk_set_expr(left, counter, alias_map);
            walk_set_expr(right, counter, alias_map);
        }
        SetExpr::Query(subquery) => {
            walk_query(subquery, counter, alias_map);
        }
        _ => {}
    }
}

fn walk_query(query: &mut Query, counter: &mut usize, alias_map: &mut HashMap<String, String>) {
    walk_set_expr(&mut query.body, counter, alias_map);

    if let Some(with) = &mut query.with {
        for cte in &mut with.cte_tables {
            walk_query(&mut cte.query, counter, alias_map);
        }
    }
}

pub fn alias_all_columns(sql: &str) -> (String, HashMap<String, String>){
    let dialect = PostgreSqlDialect {};
    let mut statements = Parser::parse_sql(&dialect, sql).unwrap();

    let mut alias_map = HashMap::new();
    let mut counter = 1;

    let _ = visit_statements_mut(&mut statements, |stmt| {
        if let Statement::Query(query) = stmt {
            walk_query(query, &mut counter, &mut alias_map);
        }
        ControlFlow::<()>::Continue(())
    });

    let res = statements
        .into_iter()
        .map(|stmt| stmt.to_string())
        .collect::<Vec<_>>()
        .join(" ");

    println!("result: {:?} alias_map: {:?}", res, alias_map);

    (res, alias_map)
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
                "select * from (SELECT t.a FROM t)",
                vec!["SELECT t.a AS"],
            ),

            (
                "select * from (SELECT t.a, t.b FROM t) T1",
                vec!["SELECT t.a AS", "t.b AS"],
            ),
        ];

        for (input, expected_substrings) in cases {
            let (transformed, aliases) = alias_all_columns(input);
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
