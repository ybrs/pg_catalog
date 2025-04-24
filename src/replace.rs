use std::ops::ControlFlow;
use arrow::datatypes::DataType as ArrowDataType;
use datafusion::logical_expr::{create_udf, ColumnarValue, ScalarUDF, Volatility};
use datafusion::scalar::ScalarValue;

use sqlparser::ast::{DataType as SQLDataType, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, ObjectName, ObjectNamePart, Query, Select, SelectItem, SetExpr, Statement, Value};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::{Parser, ParserError};
use std::sync::Arc;
use FunctionArg::Unnamed;
use sqlparser::tokenizer::Tokenizer;


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

use sqlparser::ast::*;


// pub fn parse_sql_and_dump_ast(sql: &str) {
//     let dialect = PostgreSqlDialect {};
//     let mut statements = Parser::parse_sql(&dialect, sql).unwrap();
//
//     visit_statements_mut(&mut statements, |stmt| {
//         if let Statement::Query(query) = stmt {
//             query.limit = None;
//         }
//         ControlFlow::<()>::Continue(())
//     });
//
//     for stmt in statements {
//         println!("{}", stmt);
//     }
// }


pub fn parse_sql_and_dump_ast(sql: &str) {
    let dialect = PostgreSqlDialect {};
    let mut statements = Parser::parse_sql(&dialect, sql).unwrap();


    visit_statements_mut(&mut statements, |stmt| {
        visit_expressions_mut(stmt, |expr| {
            dbg!(&expr); // leave this if you want to debug input
            if let Expr::Cast { data_type, .. } = expr {
                if let DataType::Regclass = data_type {
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
                                    Expr::Value(Value::SingleQuotedString("foo".into()).into())
                                ))
                            ],
                            clauses: vec![],
                        }),
                    });
                }
            }
            ControlFlow::Continue(())
        })?;
        ControlFlow::<()>::Continue(())
    });

    println!("sql after ====");
    for stmt in statements {
        println!("{}", stmt);
    }
    println!("=======");
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;


    #[test]
    fn test_usage() -> Result<(), Box<dyn Error>> {

        parse_sql_and_dump_ast("select 'pg_namespace'::regclass from foo limit 10");
        Ok(())
    }
}