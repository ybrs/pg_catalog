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
                Expr::Cast { expr: inner_expr, data_type, .. } => match data_type {
                    DataType::Regclass => {
                        let alias = format!("alias_{}", *counter);
                        *counter += 1;
                        let name = "regclass";
                        // here obj is
                        // obj: ObjectName([Identifier(Ident { value: "oid", quote_style: None, span: Span(Location(1,35)..Location(1,38)) })])
                        // find the value "oid" and put it to name
                        alias_map.insert(alias.clone(), name.into());

                        new_proj.push(SelectItem::ExprWithAlias {
                            expr: expr.clone(),
                            alias: Ident::new(alias),
                        });
                    },
                    DataType::Custom(obj, _) if obj.0.len() == 1 => {

                        let alias = format!("alias_{}", *counter);
                        *counter += 1;
                        let name = obj.0
                            .last()
                            .and_then(|part| part.as_ident())
                            .map(|ident| ident.value.clone())
                            .unwrap_or_else(|| "?column?".to_string());

                        // here obj is
                        // obj: ObjectName([Identifier(Ident { value: "oid", quote_style: None, span: Span(Location(1,35)..Location(1,38)) })])
                        // find the value "oid" and put it to name
                        alias_map.insert(alias.clone(), name);

                        new_proj.push(SelectItem::ExprWithAlias {
                            expr: expr.clone(),
                            alias: Ident::new(alias),
                        });
                    },
                    _ => {
                        let alias = format!("alias_{}", *counter);
                        *counter += 1;
                        alias_map.insert(alias.clone(), data_type.to_string().to_lowercase());
                        new_proj.push(SelectItem::ExprWithAlias {
                            expr: expr.clone(),
                            alias: Ident::new(alias),
                        });
                    },
                }
                Expr::Function(f) => {
                    let alias = format!("alias_{}", *counter);
                    *counter += 1;
                    let name = f.clone().name.to_string();
                    alias_map.insert(alias.clone(), name);

                    new_proj.push(SelectItem::ExprWithAlias {
                        expr: expr.clone(),
                        alias: Ident::new(alias),
                    });
                },

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


    fn alias_maps(nums: &[&str]) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for (i, &val) in nums.iter().enumerate() {
            let key = format!("alias_{}", i + 1);
            map.insert(key, val.to_string());
        }
        map
    }

    #[test]
    fn test_alias_all_columns() -> Result<(), Box<dyn Error>> {
        let cases = vec![
            (
                "SELECT t.id FROM foo",
                vec!["SELECT t.id AS alias_1", "FROM foo"],
                alias_maps(&["id"]),
            ),
            (
                "SELECT t.id AS f FROM foo",
                vec!["SELECT t.id AS f FROM foo"], // Should stay the same
                alias_maps(&[]), // empty alias
            ),
            (
                "SELECT t.* FROM foo",
                vec!["SELECT t.* FROM foo"], // No aliasing needed
                alias_maps(&[]), // empty alias
            ),
            (
                "SELECT t.id, t.* FROM foo",
                vec!["SELECT t.id AS alias_1, t.* FROM foo"], // Only t.id gets alias
                alias_maps(&["id"]),
            ),
            (
                "SELECT 1 FROM foo",
                vec!["SELECT 1 AS alias_1", "FROM foo"], // literal should also get alias
                alias_maps(&["?column?"]), // postgresql also returns ?column? in this case
            ),

            (
                "SELECT 1, 1 FROM foo",
                vec!["SELECT 1 AS alias_1, 1 AS alias_2 FROM foo"], // literal should also get alias
                alias_maps(&["?column?", "?column?"]), // postgresql also returns ?column? in this case
            ),


            (
                "SELECT t.id + 1 FROM foo",
                vec!["SELECT t.id + 1 AS", "FROM foo"], // expressions get alias
                alias_maps(&["?column?"]),
            ),

            (
                "WITH cte AS (SELECT t.a FROM t) SELECT * FROM cte",
                vec!["SELECT t.a AS alias_1", "SELECT * FROM cte"], // t.a gets alias, SELECT * stays
                alias_maps(&["a"]),
            ),

            (
                "select * from (SELECT t.a FROM t)",
                vec!["SELECT t.a AS"],
                alias_maps(&["a"]),
            ),

            (
                "select * from (SELECT t.a, t.b FROM t) T1",
                vec!["SELECT t.a AS alias_1", "t.b AS alias_2"],
                alias_maps(&["a", "b"]),
            ),

            (
                "select 'pg_constraint'::regclass::oid",
                vec!["SELECT 'pg_constraint'::REGCLASS::oid AS alias_1"],
                alias_maps(&["oid"]),
            ),

            (
                "select '1'::int4;",
                vec!["SELECT '1'::INT4 AS alias_1"],
                alias_maps(&["int4"]),
            ),

            (
                "select '1'::int8;",
                vec!["SELECT '1'::INT8 AS alias_1"],
                alias_maps(&["int8"]),
            ),

            (
                "select '1'::varchar;",
                vec!["SELECT '1'::VARCHAR AS alias_1"],
                alias_maps(&["varchar"]),
            ),

            (
                "select '1'::varchar(120);",
                vec!["SELECT '1'::VARCHAR(120) AS alias_1"],
                alias_maps(&["varchar(120)"]),
            ),


            (
                "select 'pg_constraint'::regclass",
                vec!["SELECT 'pg_constraint'::REGCLASS AS alias_1"],
                alias_maps(&["regclass"]),
            ),

            (
                "select substr('foo', 1, 2)",
                vec!["SELECT substr('foo', 1, 2) AS "],
                alias_maps(&["substr"]),
            )
        ];

        for (input, expected_substrings, expected_alias_map) in cases {
            let (transformed, aliases) = alias_all_columns(input);
            for expected in expected_substrings {
                assert!(
                    transformed.contains(expected),
                    "Expected substring not found:\ninput: {}\nexpected: {}\nactual: {}",
                    input,
                    expected,
                    transformed
                );
                assert_eq!(aliases, expected_alias_map, "alias maps failed input: {} expected {:?} actual {:?}", input, expected_alias_map, aliases);
            }
        }

        Ok(())
    }
}
