use std::collections::HashMap;
use std::ops::ControlFlow;
use arrow::datatypes::DataType as ArrowDataType;
use datafusion::logical_expr::{create_udf, ColumnarValue, LogicalPlan, ScalarUDF, Volatility};
use datafusion::scalar::ScalarValue;

use sqlparser::ast::{Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, ObjectName, ObjectNamePart, Value};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::{Parser};
use std::sync::Arc;
use datafusion::prelude::SessionContext;
use sqlparser::ast::*;
use arrow::datatypes::DataType as T;
use async_trait::async_trait;
use sqlparser::tokenizer::Span;

/* ---------- UDF ---------- */
pub fn regclass_udfs(ctx: &SessionContext) -> Vec<ScalarUDF> {
    let mut map: std::collections::HashMap<String, i32> = [
        ("pg_class", 1259),
        ("pg_constraint", 2606),
        ("pg_namespace", 2615),
    ]
        .into_iter()
        .map(|(k, v)| (k.into(), v))
        .collect();
    let mut next = 16384;
    for c in ctx.catalog_names() {
        let cat = ctx.catalog(&c).unwrap();
        for s in cat.schema_names() {
            let sch = cat.schema(&s).unwrap();
            for t in sch.table_names() {
                let k = if s == "public" { t.clone() } else { format!("{}.{}", s, t) };
                if !map.contains_key(&k) {
                    map.insert(k, next);
                    next += 1;
                }
            }
        }
    }
    let map = std::sync::Arc::new(map);

    let regclass = create_udf(
        "regclass",
        vec![T::Utf8],
        T::Utf8,
        Volatility::Immutable,
        {
            let map = map.clone();
            std::sync::Arc::new(move |args| {
                if let ColumnarValue::Scalar(ScalarValue::Utf8(Some(s))) = &args[0] {
                    Ok(ColumnarValue::Scalar(ScalarValue::Utf8(Some(s.clone()))))
                } else {
                    Ok(ColumnarValue::Scalar(ScalarValue::Utf8(None)))
                }
            })
        },
    );

    let regclass_oid = create_udf(
        "regclass_oid",
        vec![T::Utf8],
        T::Int32,
        Volatility::Immutable,
        {
            let map = map.clone();
            std::sync::Arc::new(move |args| {
                let v = if let ColumnarValue::Scalar(ScalarValue::Utf8(Some(s))) = &args[0] {
                    *map.get(s).unwrap_or(&0)
                } else {
                    0
                };
                Ok(ColumnarValue::Scalar(ScalarValue::Int32(Some(v))))
            })
        },
    );

    vec![regclass, regclass_oid]
}



pub fn replace_regclass(sql: &str) -> String {
    fn make_fn(name: &str, lit: &str) -> Expr {
        Expr::Function(Function {
            name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new(name))]),
            over: None,
            filter: None,
            within_group: vec![],
            null_treatment: None,
            uses_odbc_syntax: false,
            parameters: FunctionArguments::None,
            args: FunctionArguments::List(FunctionArgumentList {
                duplicate_treatment: None,
                args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(
                    ValueWithSpan {
                        value: Value::SingleQuotedString(lit.into()),
                        span: Span::empty(),
                    },
                )))],
                clauses: vec![],
            }),
        })
    }

    let dialect = PostgreSqlDialect {};
    let mut statements = Parser::parse_sql(&dialect, sql).unwrap();

    visit_statements_mut(&mut statements, |stmt| {
        visit_expressions_mut(stmt, |expr| {
            match expr {
                /* ---------- 1. 'text'::regclass::oid ---------- */
                Expr::Cast {
                    expr: inner_outer,
                    data_type: DataType::Custom(obj, _),
                    ..
                } if obj.0.len() == 1
                    && matches!(
                        &obj.0[0],
                        ObjectNamePart::Identifier(id) if id.value.eq_ignore_ascii_case("oid")
                    ) =>
                    {
                        // Handle inner Cast('text' AS regclass)
                        if let Expr::Cast { expr: inner, data_type: DataType::Regclass, .. } =
                            &mut **inner_outer
                        {
                            if let Expr::Value(ValueWithSpan { value: Value::SingleQuotedString(s), .. }) =
                                &**inner
                            {
                                *expr = make_fn("regclass_oid", s);
                            }
                        }
                        // Handle inner regclass('text') if it already got rewritten
                        else if let Expr::Function(f) = &mut **inner_outer {
                            if f.name.to_string().eq_ignore_ascii_case("regclass") {
                                if let FunctionArguments::List(list) = &f.args {
                                    if let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(
                                                                         Expr::Value(ValueWithSpan {
                                                                                         value: Value::SingleQuotedString(s),
                                                                                         ..
                                                                                     }),
                                                                     ))) = list.args.get(0)
                                    {
                                        *expr = make_fn("regclass_oid", s);
                                    }
                                }
                            }
                        }
                    }

                /* ---------- 2. plain 'text'::regclass ---------- */
                Expr::Cast { expr: inner, data_type: DataType::Regclass, .. } => {
                    if let Expr::Value(ValueWithSpan { value: Value::SingleQuotedString(s), .. }) =
                        &**inner
                    {
                        *expr = make_fn("regclass", s);
                    }
                }
                _ => {}
            }
            ControlFlow::<()>::Continue(())
        })?;
        ControlFlow::Continue(())
    });

    statements
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;


    #[test]
    fn test_regclass_with_oid() -> Result<(), Box<dyn std::error::Error>> {
        let cases = vec![
            (
                "SELECT 'pg_constraint'::regclass::oid",
                "SELECT regclass_oid('pg_constraint')",
            ),
            (
                "WITH c AS (SELECT 'pg_class'::regclass::oid) SELECT * FROM c",
                "WITH c AS (SELECT regclass_oid('pg_class')) SELECT * FROM c",
            ),
            (
                "SELECT t.*, 'pg_namespace'::regclass::oid FROM x t",
                "SELECT t.*, regclass_oid('pg_namespace') FROM x AS t",
            ),
        ];

        for (input, expected) in cases {
            let transformed = replace_regclass(input);
            assert_eq!(transformed, expected, "Failed for input: {}", input);
        }
        Ok(())
    }


    #[test]
    fn test_various_sql_cases() -> Result<(), Box<dyn Error>> {
        let cases = vec![
            (
                "SELECT 'pg_namespace'::regclass FROM foo LIMIT 10",
                "SELECT regclass('pg_namespace') FROM foo LIMIT 10",
            ),
            (
                "WITH cte AS (SELECT 'pg_class'::regclass) SELECT * FROM cte",
                "WITH cte AS (SELECT regclass('pg_class')) SELECT * FROM cte",
            ),
            (
                "SELECT t.*, 'pg_class'::regclass FROM table1 t JOIN table2 ON true",
                "SELECT t.*, regclass('pg_class') FROM table1 AS t JOIN table2 ON true",
            ),
            (
                "SELECT * FROM (SELECT 'pg_class'::regclass) sub",
                "SELECT * FROM (SELECT regclass('pg_class')) AS sub",
            ),
        ];

        for (input, expected) in cases {
            let transformed = replace_regclass(input);
            assert_eq!(transformed, expected, "Failed for input: {}", input);
        }

        Ok(())
    }



}