use arrow::datatypes::DataType as ArrowDataType;
use datafusion::logical_expr::{create_udf, ColumnarValue, ScalarUDF, Volatility};
use datafusion::scalar::ScalarValue;
use sqlparser::ast::{
    DataType as SQLDataType, Expr, Function, FunctionArg, FunctionArgExpr, Ident,
    ObjectName, ObjectNamePart, Query, Select, SelectItem, SetExpr, Statement, Value,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::{Parser, ParserError};
use std::sync::Arc;

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

/* ---------- tiny AST helpers ---------- */
fn regclass_call(arg: Expr) -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Ident(Ident::new("regclass"))]),
        args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(arg))],
        parameters: vec![],
        over: None,
        distinct: false,
        null_treatment: None,
        within_group: vec![],
        uses_odbc_syntax: false,
    })
}

fn rewrite_expr(e: &mut Expr) {
    match e {
        Expr::Cast { expr, data_type, .. } if matches!(data_type, SQLDataType::Regclass) => {
            let inner = (*expr).clone();
            *e = regclass_call(inner);
        }
        Expr::TypedString { data_type, value } if matches!(data_type, SQLDataType::Regclass) => {
            let lit = Expr::Value(Value::SingleQuotedString(value.clone()).into());
            *e = regclass_call(lit);
        }
        // recurse into common child-bearing variants
        Expr::BinaryOp { left, right, .. } => {
            rewrite_expr(left);
            rewrite_expr(right);
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::Exists { subquery: expr, .. }
        | Expr::Subquery(expr, ..) => rewrite_expr(expr),
        Expr::Function(Function { args, .. }) => {
            for a in args {
                if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = a {
                    rewrite_expr(e);
                }
            }
        }
        _ => {}
    }
}

fn rewrite_select(sel: &mut Select) {
    for item in &mut sel.projection {
        if let SelectItem::UnnamedExpr(e) = item {
            rewrite_expr(e);
        }
    }
    if let Some(w) = &mut sel.selection {
        rewrite_expr(w);
    }
    if let Some(h) = &mut sel.having {
        rewrite_expr(h);
    }
}

fn rewrite_query(q: &mut Query) {
    if let SetExpr::Select(sel) = &mut q.body {
        rewrite_select(sel);
    }
    if let Some(expr) = &mut q.limit {
        rewrite_expr(expr);
    }
    if let Some(expr) = &mut q.offset {
        rewrite_expr(expr.value.as_mut());
    }
}

fn rewrite_statement(stmt: &mut Statement) {
    if let Statement::Query(q) = stmt {
        rewrite_query(q);
    }
}

/* ---------- public helper ---------- */
pub fn rewrite_regclass(sql: &str) -> Result<String, ParserError> {
    let dialect = PostgreSqlDialect {};
    let mut stmts = Parser::parse_sql(&dialect, sql)?;
    for s in &mut stmts {
        rewrite_statement(s);
    }
    Ok(stmts.into_iter().map(|s| s.to_string()).collect::<Vec<_>>().join("; "))
}
