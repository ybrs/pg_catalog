use std::sync::Arc;
use arrow::datatypes::DataType;
use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
use datafusion::prelude::SessionContext;
use sqlparser::ast::*;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::collections::HashMap;
use std::ops::ControlFlow;

pub async fn rewrite_exists_subquery(sql: &str, ctx: &SessionContext) -> datafusion::error::Result<String> {
    let dialect = PostgreSqlDialect {};
    let mut statements = Parser::parse_sql(&dialect, sql)
        .map_err(|e| datafusion::error::DataFusionError::External(Box::new(e)))?;
    let mut counter = 0usize;

    let _ = visit_statements_mut(&mut statements, |stmt| {
        if let Statement::Query(q) = stmt {
            let _ = transform_setexpr(&mut q.body, ctx, &mut counter);
        }
        ControlFlow::<()>::Continue(())
    });

    Ok(statements
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("; "))
}

fn replace_with_fn_call(expr: &mut Expr, fn_name: String, cols: &[(Ident, DataType)]) {
    let args: Vec<FunctionArg> = cols
        .iter()
        .map(|(id, _)| FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Identifier(id.clone()))))
        .collect();

    *expr = Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new(fn_name))]),
        args: FunctionArguments::List(FunctionArgumentList { args, clauses: vec![], duplicate_treatment: None }),
        over: None,
        filter: None,
        within_group: vec![],
        null_treatment: None,
        parameters: FunctionArguments::None,
        uses_odbc_syntax: false,
    });
}

fn find_correlated_columns(q: &mut Box<Query>) -> Vec<(Ident, DataType)> {
    let mut cols: Vec<(Ident, DataType)> = Vec::new();
    let mut map: HashMap<String, usize> = HashMap::new();
    let _ = visit_expressions_mut(q.as_mut(), |expr| {
        if let Expr::Identifier(id) = expr {
            let key = id.value.clone();
            let idx = *map.entry(key.clone()).or_insert_with(|| {
                cols.push((id.clone(), DataType::Utf8));
                cols.len() - 1
            });
            *expr = Expr::Identifier(Ident::new(format!("${}", idx + 1)));
        } else if let Expr::CompoundIdentifier(parts) = expr {
            if parts.len() == 1 {
                let id0 = &parts[0];
                let key = id0.value.clone();
                let idx = *map.entry(key.clone()).or_insert_with(|| {
                    cols.push((id0.clone(), DataType::Utf8));
                    cols.len() - 1
                });
                *expr = Expr::Identifier(Ident::new(format!("${}", idx + 1)));
            }
        }
        ControlFlow::<()>::Continue(())
    });
    cols
}

async fn transform_setexpr(sexpr: &mut SetExpr, ctx: &SessionContext, counter: &mut usize) -> datafusion::error::Result<()> {
    if let SetExpr::Select(s) = sexpr {
        for item in &mut s.projection {
            if let SelectItem::UnnamedExpr(e) = item {
                transform_expr(e, ctx, counter).await?;
            }
        }
        if let Some(e) = &mut s.selection {
            transform_expr(e, ctx, counter).await?;
        }
    }
    Ok(())
}

async fn transform_expr(expr: &mut Expr, ctx: &SessionContext, counter: &mut usize) -> datafusion::error::Result<()> {
    match expr {
        Expr::Subquery(q) => {
            let cols = find_correlated_columns(q);
            if !cols.is_empty() {
                let fn_name = format!("__subq{}", *counter);
                *counter += 1;
                register_udf(ctx, &fn_name, q.to_string(), &cols).await?;
                replace_with_fn_call(expr, fn_name, &cols);
            }
        }
        Expr::Exists { subquery, .. } => {
            let cols = find_correlated_columns(subquery);
            if !cols.is_empty() {
                let fn_name = format!("__subq{}", *counter);
                *counter += 1;
                let exist_sql = format!("SELECT EXISTS ({})", subquery.to_string());
                register_udf(ctx, &fn_name, exist_sql, &cols).await?;
                replace_with_fn_call(expr, fn_name, &cols);
            }
        }
        _ => {}
    }
    Ok(())
}

async fn register_udf(
    ctx: &SessionContext,
    name: &str,
    sub_sql: String,
    cols: &[(Ident, DataType)],
) -> datafusion::error::Result<()> {
    let arg_types: Vec<DataType> = cols.iter().map(|(_, t)| t.clone()).collect();
    let plan = ctx.state().create_logical_plan(&sub_sql).await?;
    let ret_type = plan.schema().field(0).data_type().clone();
    let ret_type_cloned = ret_type.clone();
    let ctx_ref = Arc::new(ctx.clone());
    let template = sub_sql.clone();
    let fun = move |args: &[ColumnarValue]| {
        let mut q = template.clone();
        for (i, arg) in args.iter().enumerate() {
            let v = match arg {
                ColumnarValue::Scalar(s) => s.to_string(),
                _ => return Err(datafusion::error::DataFusionError::Internal(String::from("array arg"))),
            };
            q = q.replace(&format!("${}", i + 1), &v);
        }
        futures::executor::block_on(async {
            let batch = ctx_ref.sql(&q).await?.collect().await?;
            if ret_type_cloned == DataType::Boolean {
                let rows = batch.iter().map(|b| b.num_rows()).sum::<usize>() > 0;
                Ok(ColumnarValue::Scalar(datafusion::scalar::ScalarValue::Boolean(Some(rows))))
            } else {
                let val = datafusion::scalar::ScalarValue::try_from_array(batch[0].column(0).as_ref(), 0)?;
                Ok(ColumnarValue::Scalar(val))
            }
        })
    };
    let udf = create_udf(name, arg_types, ret_type, Volatility::Volatile, Arc::new(fun));
    ctx.register_udf(udf);
    Ok(())
}
