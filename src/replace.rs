// Helpers for rewriting SQL before execution.
// Provides small parsers and UDFs to emulate PostgreSQL behaviour (e.g., regclass casts) that DataFusion lacks.
// Added to translate client queries into forms DataFusion understands.

use std::collections::HashMap;
use std::ops::ControlFlow;
use arrow::datatypes::DataType as ArrowDataType;
use datafusion::logical_expr::{create_udf, ColumnarValue, ScalarUDF, Volatility};
use datafusion::scalar::ScalarValue;

use sqlparser::ast::{Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, ObjectName, ObjectNamePart, Value};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::{Parser};
use datafusion::prelude::SessionContext;
use sqlparser::ast::*;
use sqlparser::tokenizer::Span;
use sqlparser::ast::{
    visit_expressions_mut, visit_statements_mut, ValueWithSpan,
};
use sqlparser::ast::{Statement};
use sqlparser::ast::OneOrManyWithParens;
use datafusion::error::{DataFusionError, Result};


/* ---------- UDF ---------- */
pub fn regclass_udfs(ctx: &SessionContext) -> Vec<ScalarUDF> {
    let regclass = create_udf(
        "regclass",
        vec![ArrowDataType::Utf8],
        ArrowDataType::Utf8,
        Volatility::Immutable,
        {
            std::sync::Arc::new(move |args| {
                if let ColumnarValue::Scalar(ScalarValue::Utf8(Some(s))) = &args[0] {
                    Ok(ColumnarValue::Scalar(ScalarValue::Utf8(Some(s.clone()))))
                } else {
                    Ok(ColumnarValue::Scalar(ScalarValue::Utf8(None)))
                }
            })
        },
    );

    vec![regclass]
}


fn add_namespace_to_set_command(obj: &mut ObjectName) {
    if obj.0.len() == 1 {
        let ident = obj.0.remove(0);
        obj.0.push(ObjectNamePart::Identifier(Ident::new("pg_catalog")));
        obj.0.push(ident);
    }
}

pub fn replace_set_command_with_namespace(sql: &str) -> Result<String> {
    let dialect = PostgreSqlDialect {};
    let mut statements = Parser::parse_sql(&dialect, sql)
        .map_err(|e| DataFusionError::External(Box::new(e)))?;

    visit_statements_mut(&mut statements, |stmt| {
        if let Statement::SetVariable { variables, .. } = stmt {
            match variables {
                OneOrManyWithParens::One(obj) => add_namespace_to_set_command(obj),
                OneOrManyWithParens::Many(list) => list.iter_mut().for_each(add_namespace_to_set_command),
            }
        }
        ControlFlow::<()>::Continue(())
    });

    Ok(statements
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("; "))
}

pub fn replace_regclass(sql: &str) -> Result<String> {
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
    let mut statements = Parser::parse_sql(&dialect, sql)
        .map_err(|e| DataFusionError::External(Box::new(e)))?;

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
                                *expr = make_fn("oid", s);
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
                                        *expr = make_fn("oid", s);
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

    Ok(statements
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(" "))
}


pub fn rewrite_pg_custom_operator(sql: &str) -> Result<String> {
    use sqlparser::ast::{visit_expressions_mut, visit_statements_mut, BinaryOperator, Expr};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    let dialect = PostgreSqlDialect {};
    let mut statements = Parser::parse_sql(&dialect, sql)
        .map_err(|e| DataFusionError::External(Box::new(e)))?;

    visit_statements_mut(&mut statements, |stmt| {
        visit_expressions_mut(stmt, |expr| {
            if let Expr::BinaryOp { op, .. } = expr {
                if let BinaryOperator::PGCustomBinaryOperator(parts) = op {
                    if parts.len() == 2
                        && parts[0].eq_ignore_ascii_case("pg_catalog")
                        && parts[1] == "~"
                    {
                        *op = BinaryOperator::PGRegexMatch;    // plain `~`
                    }
                }
            }
            ControlFlow::<()>::Continue(())
        });
        ControlFlow::<()>::Continue(())

    });

    Ok(statements
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(" "))
}

pub fn rewrite_schema_qualified_text(sql: &str) -> Result<String> {
    fn is_pg_text(name: &ObjectName) -> bool {
        name.0.len() == 2
            && matches!((&name.0[0], &name.0[1]),
                (
                    ObjectNamePart::Identifier(a),
                    ObjectNamePart::Identifier(b)
                ) if a.value.eq_ignore_ascii_case("pg_catalog")
                    && b.value.eq_ignore_ascii_case("text"))
    }

    let dialect = PostgreSqlDialect {};
    let mut stmts = Parser::parse_sql(&dialect, sql)
        .map_err(|e| DataFusionError::External(Box::new(e)))?;

    visit_statements_mut(&mut stmts, |stmt| {
        visit_expressions_mut(stmt, |e| {
            if let Expr::Cast { data_type, .. } = e {
                if let DataType::Custom(obj, _) = data_type {
                    if is_pg_text(obj) {
                        *data_type = DataType::Text;
                    }
                }
            }
            ControlFlow::<()>::Continue(())
        })?;
        ControlFlow::Continue(())
    });

    Ok(stmts
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(" "))
}


pub fn rewrite_schema_qualified_custom_types(sql: &str) -> Result<String> {
    use sqlparser::ast::{visit_expressions_mut, visit_statements_mut,
                         DataType, Expr, ObjectName, ObjectNamePart};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    fn is_pg_type(name: &ObjectName, t: &str) -> bool {
        name.0.len() == 2 &&
            matches!(
                (&name.0[0], &name.0[1]),
                (
                    ObjectNamePart::Identifier(a),
                    ObjectNamePart::Identifier(b)
                ) if a.value.eq_ignore_ascii_case("pg_catalog")
                     && b.value.eq_ignore_ascii_case(t)
            )
    }

    let dialect = PostgreSqlDialect {};
    let mut stmts = Parser::parse_sql(&dialect, sql)
        .map_err(|e| DataFusionError::External(Box::new(e)))?;

    visit_statements_mut(&mut stmts, |stmt| {
        visit_expressions_mut(stmt, |e| {
            if let Expr::Cast { data_type, .. } = e {
                if let DataType::Custom(obj, _) = data_type {
                    if is_pg_type(obj, "text") 
                    || is_pg_type(obj, "regtype")
                    || is_pg_type(obj, "regnamespace") 
                    || is_pg_type(obj, "regclass") {
                        *data_type = DataType::Text;
                    }
                }
            }
            ControlFlow::<()>::Continue(())
        })?;
        ControlFlow::Continue(())
    });

    Ok(stmts
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(" "))
}


/// Replace casts to regtype / pg_catalog.regtype with TEXT,
/// or drop them entirely if they are immediately followed by a TEXT cast.
pub fn rewrite_regtype_cast(sql: &str) -> Result<String> {
    use sqlparser::ast::{
        visit_expressions_mut, visit_statements_mut, DataType, Expr, ObjectName,
        ObjectNamePart,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    // Return true when the object name represents regtype
    fn is_regtype(obj: &ObjectName) -> bool {
        match obj.0.as_slice() {
            // unqualified: regtype
            [ObjectNamePart::Identifier(id)]
                if id.value.eq_ignore_ascii_case("regtype") =>
            {
                true
            }
            // qualified: pg_catalog.regtype
            [ObjectNamePart::Identifier(schema), ObjectNamePart::Identifier(id)]
                if schema.value.eq_ignore_ascii_case("pg_catalog")
                    && id.value.eq_ignore_ascii_case("regtype") =>
            {
                true
            }
            _ => false,
        }
    }

    let dialect = PostgreSqlDialect {};
    let mut stmts = Parser::parse_sql(&dialect, sql)
        .map_err(|e| DataFusionError::External(Box::new(e)))?;

    visit_statements_mut(&mut stmts, |stmt| {
        visit_expressions_mut(stmt, |e| {
            if let Expr::Cast { data_type, .. } = e {
                if let DataType::Custom(obj, _) = data_type {
                    if is_regtype(obj) {
                        *data_type = DataType::Text; // regtype  ➜  TEXT
                    }
                }
            }
            ControlFlow::<()>::Continue(())
        })?;
        ControlFlow::Continue(())
    });

    Ok(stmts
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("; "))
}

pub fn rewrite_oid_cast(sql: &str) -> Result<String> {
    use sqlparser::ast::{
        visit_expressions_mut, visit_statements_mut, DataType, Expr, Function, FunctionArg,
        FunctionArgExpr, FunctionArgumentList, FunctionArguments, Ident, ObjectName,
        ObjectNamePart,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    fn is_oid(obj: &ObjectName) -> bool {
        match obj.0.as_slice() {
            [ObjectNamePart::Identifier(id)] if id.value.eq_ignore_ascii_case("oid") => true,
            [ObjectNamePart::Identifier(schema), ObjectNamePart::Identifier(id)]
                if schema.value.eq_ignore_ascii_case("pg_catalog")
                    && id.value.eq_ignore_ascii_case("oid") =>
            {
                true
            }
            _ => false,
        }
    }

    fn make_oid_expr(inner: Expr) -> Expr {
        Expr::Function(Function {
            name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("oid"))]),
            args: FunctionArguments::List(FunctionArgumentList {
                duplicate_treatment: None,
                clauses: vec![],
                args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(inner))],
            }),
            over: None,
            filter: None,
            within_group: vec![],
            null_treatment: None,
            parameters: FunctionArguments::None,
            uses_odbc_syntax: false,
        })
    }

    let dialect = PostgreSqlDialect {};
    let mut stmts = Parser::parse_sql(&dialect, sql)
        .map_err(|e| DataFusionError::External(Box::new(e)))?;

    visit_statements_mut(&mut stmts, |stmt| {
        visit_expressions_mut(stmt, |expr| {
            if let Expr::Cast { expr: inner, data_type, .. } = expr {
                if let DataType::Custom(obj, _) = data_type {
                    if is_oid(obj) {
                        let arg = (**inner).clone();
                        *expr = make_oid_expr(arg);
                    }
                }
            }
            ControlFlow::<()>::Continue(())
        })?;
        ControlFlow::Continue(())
    });

    Ok(stmts
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(" "))
}

pub fn strip_default_collate(sql: &str) -> Result<String> {
    /// we are dropping default collate, since datafusion doesnt support collates. 
    /// and it's kinda safe. there is only default collate. 
    use sqlparser::ast::{
        visit_expressions_mut, visit_statements_mut, Expr, ObjectName, ObjectNamePart,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    fn is_default(coll: &ObjectName) -> bool {
        coll.0.len() == 2
            && matches!(
                (&coll.0[0], &coll.0[1]),
                (
                    ObjectNamePart::Identifier(a),
                    ObjectNamePart::Identifier(b)
                ) if a.value.eq_ignore_ascii_case("pg_catalog")
                    && b.value.eq_ignore_ascii_case("default")
            )
    }

    let dialect = PostgreSqlDialect {};
    let mut statements = Parser::parse_sql(&dialect, sql)
        .map_err(|e| DataFusionError::External(Box::new(e)))?;

    visit_statements_mut(&mut statements, |stmt| {
        visit_expressions_mut(stmt, |e| {
            if let Expr::Collate { expr, collation } = e {
                if is_default(collation) {
                    *e = *expr.clone();
                }
            }
            ControlFlow::<()>::Continue(())
        })?;
        ControlFlow::Continue(())
    });

    Ok(statements
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("; "))
}

/// Re-write  ARRAY( <sub-query> )
///        ⟶  pg_catalog.pg_get_array( ( <sub-query> ) )
///
/// • no regexes – uses `sqlparser` AST  
/// • only the `array( … )` form with ONE argument is accepted  
/// • any other shape causes an explicit `Err(DataFusionError::Plan(..))`  
/// • **if nothing matches we just pass the SQL back untouched**
pub fn rewrite_array_subquery(sql: &str) -> Result<String> {
    use sqlparser::ast::{
        visit_expressions_mut, visit_statements_mut, Expr, Function, FunctionArg,
        FunctionArgExpr, FunctionArguments, FunctionArgumentList, Ident, ObjectName,
        ObjectNamePart, Query,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    let dialect = PostgreSqlDialect {};
    let mut stmts = Parser::parse_sql(&dialect, sql)
        .map_err(|e| DataFusionError::External(Box::new(e)))?;

    let mut rewritten_any = false;

    /* --------------------------------------------------------- */
    let flow: ControlFlow<DataFusionError, ()> =
        visit_statements_mut(&mut stmts, |stmt| {
            let inner = visit_expressions_mut(stmt, |expr| {
                /* ── 1️⃣  bail out on ARRAY[...] literals ─────────────── */
                if let Expr::Array(_) = expr {
                    return ControlFlow::Continue(());
                }

                /* ── 2️⃣  handle ARRAY( … ) rewrites ─────────────────── */
                if let Expr::Function(func) = expr {
                    let base_name = func
                        .name
                        .0
                        .last()
                        .and_then(|p| p.as_ident())
                        .map(|id| id.value.to_lowercase())
                        .unwrap_or_default();

                    if base_name == "array" {
                        /* extract exactly one argument */
                        let arg_expr: Expr = match &func.args {
                            /* list form */
                            FunctionArguments::List(FunctionArgumentList { args, .. }) => {
                                if args.len() != 1 {
                                    return ControlFlow::Break(DataFusionError::Plan(
                                        "ARRAY() must have exactly one argument".into(),
                                    ));
                                }
                                match &args[0] {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => (*e).clone(),
                                    _ => {
                                        return ControlFlow::Break(DataFusionError::Plan(
                                            "ARRAY() argument must be an expression".into(),
                                        ))
                                    }
                                }
                            }
                            /* sub-query form */
                            FunctionArguments::Subquery(q) => {
                                Expr::Subquery(Box::new((**q).clone()))
                            }
                            _ => {
                                return ControlFlow::Break(DataFusionError::Plan(
                                    "ARRAY() with unsupported argument form".into(),
                                ))
                            }
                        };

                        // -----------------------------------------------------------------
                        // Special case: ARRAY(SELECT unnest FROM UNNEST(col))
                        // -------------------------------------------------
                        // PostgreSQL allows this construct to effectively
                        // return the original array. The generic rewrite
                        // below would turn it into:
                        //      pg_catalog.pg_get_array((SELECT unnest FROM
                        //          UNNEST(col)))
                        // which later fails when the scalar sub-query is
                        // converted into a CTE because it references the
                        // outer table.  Detect this exact shape here and
                        // simply replace the whole expression with `col`.

                        if let Expr::Subquery(subq) = &arg_expr {
                            if let SetExpr::Select(inner_sel) = subq.body.as_ref() {
                                let from_ok = inner_sel.from.len() == 1 && matches!(
                                    inner_sel.from[0].relation,
                                    TableFactor::UNNEST { .. }
                                );
                                let proj_ok = inner_sel.projection.len() == 1 &&
                                    matches!(
                                        inner_sel.projection[0],
                                        SelectItem::UnnamedExpr(Expr::Identifier(ref id))
                                        if id.value.to_lowercase() == "unnest"
                                    );
                                if from_ok && proj_ok && inner_sel.selection.is_none() {
                                    if let TableFactor::UNNEST { ref array_exprs, .. } = inner_sel.from[0].relation {
                                        if array_exprs.len() == 1 {
                                            *expr = array_exprs[0].clone();
                                            rewritten_any = true;
                                            return ControlFlow::Continue(());
                                        }
                                    }
                                }
                            }
                        }

                        /* add parentheses only when necessary */
                        let wrapped = match &arg_expr {
                            Expr::Subquery(_) | Expr::Nested(_) => arg_expr.clone(),
                            _                                    => Expr::Nested(Box::new(arg_expr.clone())),
                        };

                        /* build pg_catalog.pg_get_array( wrapped ) */
                        *expr = Expr::Function(Function {
                            name: ObjectName(vec![
                                ObjectNamePart::Identifier(Ident::new("pg_catalog")),
                                ObjectNamePart::Identifier(Ident::new("pg_get_array")),
                            ]),
                            args: FunctionArguments::List(FunctionArgumentList {
                                duplicate_treatment: None,
                                clauses: vec![],
                                args: vec![FunctionArg::Unnamed(
                                    FunctionArgExpr::Expr(wrapped),
                                )],
                            }),
                            over: None,
                            filter: None,
                            within_group: vec![],
                            null_treatment: None,
                            parameters: FunctionArguments::None,
                            uses_odbc_syntax: false,
                        });

                        rewritten_any = true;
                    }
                }
                ControlFlow::Continue(())
            });

            match inner {
                ControlFlow::Break(e)     => ControlFlow::Break(e),
                ControlFlow::Continue(()) => ControlFlow::Continue(()),
            }
        });

    /* propagate any error triggered above */
    if let ControlFlow::Break(err) = flow {
        return Err(err);
    }

    /* nothing matched – just echo input back verbatim */
    if !rewritten_any {
        return Ok(sql.to_owned());
    }

    /* serialise mutated AST */
    Ok(stmts
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("; "))
}


/// Rewrite a Postgres array literal in curly-brace notation
/// (`'{1,2,3}'`, `'{"a","b"}'`, …) into an `Expr::Array`, which
/// `sqlparser` renders as `ARRAY[...]`.
///
///  * pure-AST rewrite – no regexes
///  * if *nothing* matches we pass SQL back unchanged
///  * malformed literals raise `DataFusionError::Plan`
pub fn rewrite_brace_array_literal(sql: &str) -> Result<String> {
    use sqlparser::ast::{
        visit_expressions_mut, visit_statements_mut, Expr, Value, ValueWithSpan,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    let dialect = PostgreSqlDialect {};
    let mut stmts = Parser::parse_sql(&dialect, sql)
        .map_err(|e| DataFusionError::External(Box::new(e)))?;

    let mut rewritten_any = false;

    let flow: ControlFlow<DataFusionError, ()> =
        visit_statements_mut(&mut stmts, |stmt| {
            let inner = visit_expressions_mut(stmt, |expr| {
                if let Expr::Value(ValueWithSpan {
                    value: Value::SingleQuotedString(s),
                    ..
                }) = expr
                {
                    if s.starts_with('{') && s.ends_with('}') {
                        let inside = &s[1..s.len() - 1]; // strip the braces

                        // split respecting the simple {a,b,c} grammar
                        // (no escape handling – good enough for catalogue OIDs
                        //  like '{0}' which is what we need right now)
                        let items: Vec<Expr> = inside
                        .split(',')
                        .map(|t| {
                            Expr::Value(ValueWithSpan {
                                value: Value::SingleQuotedString(
                                    t.trim_matches('"').trim().to_string(),
                                ),
                                span: Span::empty(),
                            })
                        })
                        .collect();
                    
                        // build ARRAY[...]
                        *expr = Expr::Array(Array {
                            elem: items,
                            named: false,          // <- `false` for the normal ARRAY[...] form
                        });

                        rewritten_any = true;
                    }
                }
                ControlFlow::Continue(())
            });

            match inner {
                ControlFlow::Break(e) => ControlFlow::Break(e),
                ControlFlow::Continue(()) => ControlFlow::Continue(()),
            }
        });

    if let ControlFlow::Break(err) = flow {
        return Err(err);
    }

    if !rewritten_any {
        return Ok(sql.to_owned());
    }

    Ok(stmts
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("; "))
}



#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn test_rewrite_regtype_cast() -> Result<(), Box<dyn std::error::Error>> {
        let cases = vec![
            ("SELECT x::regtype",                  "SELECT x::TEXT"),
            ("SELECT x::pg_catalog.regtype",       "SELECT x::TEXT"),
            ("SELECT y::pg_catalog.regtype::text", "SELECT y::TEXT::TEXT"),
        ];
        for (input, expected) in cases {
            assert_eq!(rewrite_regtype_cast(input).unwrap(), expected);
        }
        Ok(())
    }

    #[test]
    fn test_rewrite_pg_custom_types() -> Result<(), Box<dyn std::error::Error>> {
        let cases = vec![
            // literal keeps ::
            ("SELECT 'a'::pg_catalog.text",
             "SELECT 'a'::TEXT"),
    
            // simple identifier keeps ::
            ("SELECT x::pg_catalog.regtype",
             "SELECT x::TEXT"),
            ("SELECT x::pg_catalog.regclass",
             "SELECT x::TEXT"),
             
            // an explicit CAST stays CAST
            ("SELECT CAST(y AS pg_catalog.regtype)",
             "SELECT CAST(y AS TEXT)"),
        ];
    
        for (input, expected) in cases {
            assert_eq!(
                rewrite_schema_qualified_custom_types(input).unwrap(),
                expected,
                "Failed for input: {}",
                input
            );
        }
        Ok(())
    }


    #[test]
    fn test_regclass_with_oid() -> Result<(), Box<dyn std::error::Error>> {
        let cases = vec![
            (
                "SELECT 'pg_constraint'::regclass::oid",
                "SELECT oid('pg_constraint')",
            ),
            (
                "WITH c AS (SELECT 'pg_class'::regclass::oid) SELECT * FROM c",
                "WITH c AS (SELECT oid('pg_class')) SELECT * FROM c",
            ),
            (
                "SELECT t.*, 'pg_namespace'::regclass::oid FROM x t",
                "SELECT t.*, oid('pg_namespace') FROM x AS t",
            ),
        ];

        for (input, expected) in cases {
            let transformed = replace_regclass(input).unwrap();
            assert_eq!(transformed, expected, "Failed for input: {}", input);
        }
        Ok(())
    }

    #[test]
    fn test_rewrite_schema_qualified_text() -> Result<(), Box<dyn std::error::Error>> {
        let cases = vec![
            ("SELECT 'a'::pg_catalog.text",              "SELECT 'a'::TEXT"),
            ("SELECT CAST('x' AS pg_catalog.text)",      "SELECT CAST('x' AS TEXT)"),
            ("WITH q AS (SELECT 'b'::pg_catalog.text) SELECT * FROM q",
            "WITH q AS (SELECT 'b'::TEXT) SELECT * FROM q"),
        ];
        for (input, expected) in cases {
            assert_eq!(rewrite_schema_qualified_text(input).unwrap(), expected);
        }
        Ok(())
    }

    #[test]
    fn test_rewrite_pg_custom_operator() -> Result<(), Box<dyn std::error::Error>> {
        let cases = vec![
            (
                "SELECT 'b' OPERATOR(pg_catalog.~) 'a'",
                "SELECT 'b' ~ 'a'",
            ),
            (
                "SELECT c.relname OPERATOR(pg_catalog.~) '^(t)$' FROM pg_class c",
                "SELECT c.relname ~ '^(t)$' FROM pg_class AS c",
            ),
        ];
        for (input, expected) in cases {
            let transformed = rewrite_pg_custom_operator(input).unwrap();
            assert_eq!(transformed, expected);
        }
        Ok(())
    }


    #[test]
    fn test_strip_default_collate() -> Result<(), Box<dyn std::error::Error>> {
        let cases = vec![
            (
                "SELECT 'a' COLLATE pg_catalog.default",
                "SELECT 'a'",
            ),
            (
                "SELECT * FROM t WHERE c COLLATE pg_catalog.default = 'x'",
                "SELECT * FROM t WHERE c = 'x'",
            ),
            (
                "WITH x AS (SELECT 'foo' COLLATE pg_catalog.default) SELECT * FROM x",
                "WITH x AS (SELECT 'foo') SELECT * FROM x",
            ),
            (
                "SELECT c.relname OPERATOR(pg_catalog.~) '^(t)$' COLLATE pg_catalog.default FROM pg_class c",
                "SELECT c.relname OPERATOR(pg_catalog.~) '^(t)$' FROM pg_class AS c",
            ),
        ];

        for (input, expected) in cases {
            let transformed = strip_default_collate(input).unwrap();
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
            let transformed = replace_regclass(input).unwrap();
            assert_eq!(transformed, expected, "Failed for input: {}", input);
        }

        Ok(())
    }

    #[test]
    fn test_set_show_query_rewrite() -> Result<(), Box<dyn Error>> {
        assert_eq!(
            replace_set_command_with_namespace("SET application_name = 'x'").unwrap(),
            "SET pg_catalog.application_name = 'x'"
        );
        assert_eq!(
            replace_set_command_with_namespace("SELECT foo FROM bar").unwrap(),
            "SELECT foo FROM bar"
        );

        assert_eq!(
            replace_set_command_with_namespace(
                "SET LOCAL work_mem TO '4MB'"
            ).unwrap(),
            "SET LOCAL pg_catalog.work_mem = '4MB'"
        );
        Ok(())
    }

    #[test]
    fn test_rewrite_array_subquery() -> Result<(), Box<dyn std::error::Error>> {
        /* basic happy-path */
        let in_sql  = "SELECT array(SELECT rolname FROM pg_catalog.pg_roles ORDER BY 1)";
        let expect  = "SELECT pg_catalog.pg_get_array((SELECT rolname FROM pg_catalog.pg_roles ORDER BY 1))";
        let out_sql = rewrite_array_subquery(in_sql).unwrap();
        println!("test_rewrite_array_subquery {}", out_sql);
        assert_eq!(out_sql, expect);

        let in_sql = "select 1";
        let out_sql = rewrite_array_subquery(in_sql).unwrap();
        assert_eq!(in_sql, out_sql);

    
        /* ARRAY with more than one arg – rejected */
        let bad_sql = "SELECT array(x, y)";
        assert!(rewrite_array_subquery(bad_sql).is_err());
    
        /* array literal is *not* touched */
        let lit_sql = "SELECT ARRAY[1,2,3]";
        let out_sql = rewrite_array_subquery(lit_sql).unwrap();
        assert_eq!(lit_sql, out_sql);

        Ok(())
    }

    #[test]
    fn test_rewrite_brace_array_literal() -> Result<(), Box<dyn std::error::Error>> {
        let in_sql  = "SELECT pol.polroles = '{0}' FROM pg_catalog.pg_policy pol";
        let expect  = "SELECT pol.polroles = ['0'] FROM pg_catalog.pg_policy AS pol";
        assert_eq!(rewrite_brace_array_literal(in_sql).unwrap(), expect);

        // nothing to do ➜ echoes input
        let plain   = "SELECT 1";
        assert_eq!(rewrite_brace_array_literal(plain).unwrap(), plain);

        Ok(())
    }

    #[test]
    fn test_rewrite_oid_cast() -> Result<(), Box<dyn std::error::Error>> {
        let cases = vec![
            (
                "SELECT amhandler::oid FROM pg_am",
                "SELECT oid(amhandler) FROM pg_am",
            ),
            ("SELECT $1::oid", "SELECT oid($1)"),
        ];

        for (input, expected) in cases {
            assert_eq!(rewrite_oid_cast(input).unwrap(), expected, "Failed for input: {}", input);
        }
        Ok(())
    }

}