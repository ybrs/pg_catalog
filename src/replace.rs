use std::ops::ControlFlow;
use arrow::datatypes::DataType as ArrowDataType;
use datafusion::logical_expr::{create_udf, ColumnarValue, ScalarUDF, Volatility};
use datafusion::scalar::ScalarValue;

use sqlparser::ast::{Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, ObjectName, ObjectNamePart, Value};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::{Parser};
use std::sync::Arc;
use sqlparser::ast::*;


/* ---------- UDF ---------- */
pub fn create_regclass_udf() -> ScalarUDF {
    create_udf(
        "regclass",
        vec![ArrowDataType::Utf8],
        ArrowDataType::Int32,
        Volatility::Immutable,
        Arc::new(|args: &[ColumnarValue]| {
            let oid = match &args[0] {
                ColumnarValue::Scalar(ScalarValue::Utf8(Some(s))) => match s.as_str() {
                    "pg_namespace" => 2615,
                    "pg_class"     => 1259,
                    _              => 0,
                },
                _ => 0,
            };
            Ok(ColumnarValue::Scalar(ScalarValue::Int32(Some(oid))))
        }),
    )
}



pub fn replace_regclass(sql: &str) -> String {
    let dialect = PostgreSqlDialect {};
    let mut statements = Parser::parse_sql(&dialect, sql).unwrap();


    visit_statements_mut(&mut statements, |stmt| {
        visit_expressions_mut(stmt, |expr| {
            dbg!(&expr); // leave this if you want to debug input
            if let Expr::Cast { expr: inner_expr, data_type, .. } = expr {
                if let DataType::Regclass = data_type {
                    if let Expr::Value(inner_val) = &**inner_expr {
                        if let Value::SingleQuotedString(s) = &inner_val.value {
                            let original_string = s.clone();
                            println!("Regclass: {}", original_string);

                            *expr = Expr::Function(Function {
                                name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("regclass"))]),
                                over: None,
                                filter: None,
                                within_group: vec![],
                                null_treatment: None,
                                uses_odbc_syntax: false,
                                parameters: FunctionArguments::None,
                                args: FunctionArguments::List(FunctionArgumentList {
                                    duplicate_treatment: None,
                                    args: vec![
                                        FunctionArg::Unnamed(FunctionArgExpr::Expr(
                                            Expr::Value(Value::SingleQuotedString(original_string.into()).into())
                                        ))
                                    ],
                                    clauses: vec![],
                                }),
                            });
                        }
                    }
                }
            }
            ControlFlow::Continue(())
        })?;
        ControlFlow::<()>::Continue(())
    });

    statements
        .into_iter()
        .map(|stmt| stmt.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;


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