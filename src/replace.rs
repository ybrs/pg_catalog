// Helpers for rewriting SQL before execution.
// Provides small parsers and UDFs to emulate PostgreSQL behaviour (e.g., regclass casts) that DataFusion lacks.
// Added to translate client queries into forms DataFusion understands.

use arrow::datatypes::DataType as ArrowDataType;
use datafusion::logical_expr::{create_udf, ColumnarValue, ScalarUDF, Volatility};
use datafusion::scalar::ScalarValue;
use std::ops::ControlFlow;

use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::SessionContext;
use sqlparser::ast::Statement;
use sqlparser::ast::{visit_expressions_mut, visit_statements_mut, ValueWithSpan};
use sqlparser::ast::{
    Array, BinaryOperator, CastKind, DataType, FunctionArgumentList, Query, SelectItem, SetExpr,
    TableAlias, TableFactor, TableWithJoins,
};
use sqlparser::ast::{
    Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, ObjectName,
    ObjectNamePart, Value,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::Span;

/// Returns `true` when `obj` names the type `type_name`, either bare
/// (`type_name`) or qualified as `pg_catalog.type_name`. Comparison is
/// case-insensitive, matching how `PostgreSQL` resolves unquoted identifiers.
fn object_name_matches(obj: &ObjectName, type_name: &str) -> bool {
    match obj.0.as_slice() {
        [ObjectNamePart::Identifier(id)] => id.value.eq_ignore_ascii_case(type_name),
        [ObjectNamePart::Identifier(schema), ObjectNamePart::Identifier(id)] => {
            schema.value.eq_ignore_ascii_case("pg_catalog")
                && id.value.eq_ignore_ascii_case(type_name)
        }
        _ => false,
    }
}

/// Parse `sql` as `PostgreSQL`, apply `rewrite` in place to every expression in
/// every statement (depth-first), and render the statements back to a SQL
/// string. Returns a parse error if `sql` is not valid `PostgreSQL`.
///
/// This is the shared skeleton for the many single-purpose expression rewriters
/// below; each supplies only the per-expression transformation.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
fn rewrite_each_expression<F>(sql: &str, mut rewrite: F) -> Result<String>
where
    F: FnMut(&mut Expr),
{
    let dialect = PostgreSqlDialect {};
    let mut statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;

    let _ = visit_statements_mut(&mut statements, |stmt| {
        let _ = visit_expressions_mut(stmt, |expr| {
            rewrite(expr);
            ControlFlow::<()>::Continue(())
        });
        ControlFlow::<()>::Continue(())
    });

    Ok(statements
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("; "))
}

/// Rewrite every `expr::type_name` cast (where `type_name` is a custom/unknown
/// type that `DataFusion` does not model) into a cast to `target`, leaving all
/// other casts untouched. `type_name` matches bare or `pg_catalog`-qualified.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
fn rewrite_custom_type_cast_target(
    sql: &str,
    type_name: &str,
    target: &DataType,
) -> Result<String> {
    rewrite_each_expression(sql, |expr| {
        if let Expr::Cast { data_type, .. } = expr {
            if let DataType::Custom(obj, _) = data_type {
                if object_name_matches(obj, type_name) {
                    *data_type = target.clone();
                }
            }
        }
    })
}

/// Force every FROM-clause table alias to render with an explicit `AS` keyword.
///
/// sqlparser 0.62 added `TableAlias::explicit`, so an alias parsed without `AS`
/// (e.g. `FROM pg_class c`) now round-trips back *without* `AS`. Earlier
/// versions always emitted `AS`, and our rewrites canonicalise to that form, so
/// we normalise aliases back to explicit `AS` after parsing.
///
/// CTE name aliases (`WITH c AS (...)`) are intentionally left untouched: their
/// `AS` is rendered by the CTE itself, so forcing it here would duplicate it.
fn force_explicit_aliases(stmts: &mut [Statement]) {
    /// Mark one optional table alias as spelled with `AS`.
    fn force_explicit_alias(alias: &mut Option<TableAlias>) {
        if let Some(a) = alias {
            a.explicit = true;
        }
    }

    /// Normalise the alias of one FROM item, descending into derived tables and
    /// nested joins so aliases at every depth are reached.
    fn walk_factor(tf: &mut TableFactor) {
        match tf {
            TableFactor::Table { alias, .. } => force_explicit_alias(alias),
            TableFactor::Derived {
                alias, subquery, ..
            } => {
                force_explicit_alias(alias);
                walk_query(subquery);
            }
            TableFactor::NestedJoin {
                table_with_joins,
                alias,
                ..
            } => {
                force_explicit_alias(alias);
                walk_table_with_joins(table_with_joins);
            }
            _ => {}
        }
    }

    /// Normalise the aliases of a FROM item and everything joined to it.
    fn walk_table_with_joins(twj: &mut TableWithJoins) {
        walk_factor(&mut twj.relation);
        for join in &mut twj.joins {
            walk_factor(&mut join.relation);
        }
    }

    /// Normalise the aliases in one query body, including both sides of a set
    /// operation.
    fn walk_setexpr(se: &mut SetExpr) {
        match se {
            SetExpr::Select(select) => {
                for twj in &mut select.from {
                    walk_table_with_joins(twj);
                }
            }
            SetExpr::Query(q) => walk_query(q),
            SetExpr::SetOperation { left, right, .. } => {
                walk_setexpr(left);
                walk_setexpr(right);
            }
            _ => {}
        }
    }

    /// Normalise the aliases in one query, including the bodies of its CTEs.
    fn walk_query(q: &mut Query) {
        if let Some(with) = &mut q.with {
            for cte in &mut with.cte_tables {
                walk_query(&mut cte.query); // body only, never the CTE name alias
            }
        }
        walk_setexpr(&mut q.body);
    }

    for stmt in stmts {
        if let Statement::Query(q) = stmt {
            walk_query(q);
        }
    }
}

/* ---------- UDF ---------- */
/// Register the minimal `regclass` UDF used by some rewrites.
///
/// The function simply returns the passed string value so that
/// casts such as `'foo'::regclass` can be emulated.
#[must_use]
pub fn regclass_udfs(_ctx: &SessionContext) -> Vec<ScalarUDF> {
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

/// Qualify an unqualified `SET` variable name with the `pg_catalog` schema,
/// leaving an already qualified name (`pg_catalog.work_mem`) as it is.
fn add_namespace_to_set_command(obj: &mut ObjectName) {
    if obj.0.len() == 1 {
        let ident = obj.0.remove(0);
        obj.0
            .push(ObjectNamePart::Identifier(Ident::new("pg_catalog")));
        obj.0.push(ident);
    }
}

/// Prefix `SET` command variables with `pg_catalog` when they are
/// unqualified so that clients using bare names still work.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn replace_set_command_with_namespace(sql: &str) -> Result<String> {
    let dialect = PostgreSqlDialect {};
    let mut statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;

    let _ = visit_statements_mut(&mut statements, |stmt| {
        if let Statement::Set(set) = stmt {
            use sqlparser::ast::Set;
            match set {
                Set::SingleAssignment { variable, .. } => add_namespace_to_set_command(variable),
                Set::ParenthesizedAssignments { variables, .. } => {
                    variables.iter_mut().for_each(add_namespace_to_set_command);
                }
                Set::MultipleAssignments { assignments } => {
                    for assignment in assignments {
                        add_namespace_to_set_command(&mut assignment.name);
                    }
                }
                _ => {}
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

/// Rewrite casts from text to `regclass` (and optionally `oid`) into
/// explicit function calls so they can be executed by `DataFusion`.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn replace_regclass(sql: &str) -> Result<String> {
    /// Build the call `name('lit')` that resolves a catalog name at run time.
    fn build_function_call_expr(name: &str, lit: &str) -> Expr {
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
    let mut statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;

    let _ = visit_statements_mut(&mut statements, |stmt| {
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
                    if let Expr::Cast {
                        expr: inner,
                        data_type: DataType::Regclass,
                        ..
                    } = &mut **inner_outer
                    {
                        if let Expr::Value(ValueWithSpan {
                            value: Value::SingleQuotedString(s),
                            ..
                        }) = &**inner
                        {
                            *expr = build_function_call_expr("oid", s);
                        }
                    }
                    // Handle an inner regclass('text') or oid('text') that a
                    // deeper visit already produced (expressions are visited
                    // child-first, so 'text'::regclass becomes oid('text') before
                    // this outer ::oid cast is reached). Either way the redundant
                    // ::oid collapses to oid('text').
                    else if let Expr::Function(f) = &mut **inner_outer {
                        if f.name.to_string().eq_ignore_ascii_case("regclass")
                            || f.name.to_string().eq_ignore_ascii_case("oid")
                        {
                            if let FunctionArguments::List(list) = &f.args {
                                if let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(
                                    Expr::Value(ValueWithSpan {
                                        value: Value::SingleQuotedString(s),
                                        ..
                                    }),
                                ))) = list.args.first()
                                {
                                    *expr = build_function_call_expr("oid", s);
                                }
                            }
                        }
                    }
                }

                /* ---------- 2. plain 'text'::regclass ---------- */
                Expr::Cast {
                    expr: inner,
                    data_type: DataType::Regclass,
                    ..
                } => {
                    if let Expr::Value(ValueWithSpan {
                        value: Value::SingleQuotedString(s),
                        ..
                    }) = &**inner
                    {
                        // Resolve a bare 'name'::regclass to the relation's OID
                        // integer, not the name string. Clients cast a literal to
                        // regclass to compare it against an oid column (e.g.
                        // classoid = 'pg_class'::regclass); returning the string
                        // makes the planner try to cast 'pg_class' to the column's
                        // Int type and fail. The oid() UDF resolves the name.
                        *expr = build_function_call_expr("oid", s);
                    }
                }
                _ => {}
            }
            ControlFlow::<()>::Continue(())
        })?;
        ControlFlow::Continue(())
    });

    force_explicit_aliases(&mut statements);
    Ok(statements
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(" "))
}

/// Rewrite casts like `array_agg(...)::varchar` into calls to
/// `pg_catalog.array_to_string(array_agg(...), ',')` so that `DataFusion`
/// doesn't have to cast an Arrow list array to Utf8 directly.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_array_agg_varchar_cast(sql: &str) -> Result<String> {
    let dialect = PostgreSqlDialect {};
    let mut statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;

    let _ = visit_statements_mut(&mut statements, |stmt| {
        let _ = visit_expressions_mut(stmt, |expr| {
            if let Expr::Cast {
                expr: inner,
                data_type,
                ..
            } = expr
            {
                let is_varchar = matches!(data_type, DataType::Varchar(_));
                if is_varchar {
                    if let Expr::Function(fun) = &**inner {
                        if let Some(last) = fun.name.0.last() {
                            if last
                                .as_ident()
                                .is_some_and(|ident| ident.value.eq_ignore_ascii_case("array_agg"))
                            {
                                let agg_expr = inner.as_ref().clone();
                                let args = vec![
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(agg_expr)),
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(
                                        ValueWithSpan {
                                            value: Value::SingleQuotedString(",".into()),
                                            span: Span::empty(),
                                        },
                                    ))),
                                ];

                                *expr = Expr::Function(Function {
                                    name: ObjectName(vec![
                                        ObjectNamePart::Identifier(Ident::new("pg_catalog")),
                                        ObjectNamePart::Identifier(Ident::new("array_to_string")),
                                    ]),
                                    over: None,
                                    filter: None,
                                    within_group: vec![],
                                    null_treatment: None,
                                    uses_odbc_syntax: false,
                                    parameters: FunctionArguments::None,
                                    args: FunctionArguments::List(FunctionArgumentList {
                                        duplicate_treatment: None,
                                        args,
                                        clauses: vec![],
                                    }),
                                });
                            }
                        }
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

/// Replace custom operator syntax like `OPERATOR(pg_catalog.~)` with the
/// plain operator so regex comparisons can be parsed.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_pg_custom_operator(sql: &str) -> Result<String> {
    use sqlparser::ast::{visit_expressions_mut, visit_statements_mut, BinaryOperator, Expr};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    let dialect = PostgreSqlDialect {};
    let mut statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;

    let _ = visit_statements_mut(&mut statements, |stmt| {
        let _ = visit_expressions_mut(stmt, |expr| {
            if let Expr::BinaryOp { op, .. } = expr {
                if let BinaryOperator::PGCustomBinaryOperator(parts) = op {
                    if parts.len() == 2
                        && parts[0].eq_ignore_ascii_case("pg_catalog")
                        && parts[1] == "~"
                    {
                        *op = BinaryOperator::PGRegexMatch; // plain `~`
                    }
                }
            }
            ControlFlow::<()>::Continue(())
        });
        ControlFlow::<()>::Continue(())
    });

    force_explicit_aliases(&mut statements);
    Ok(statements
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(" "))
}

/// Drop the `pg_catalog.` prefix from text casts such as
/// `pg_catalog.text` so they become plain `TEXT` casts.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_schema_qualified_text(sql: &str) -> Result<String> {
    /// Does `name` spell the built-in text type as `pg_catalog.text`?
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
    let mut stmts =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;

    let _ = visit_statements_mut(&mut stmts, |stmt| {
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

/// Treat schema qualified casts to built-in types (regclass, regtype,
/// regnamespace, ...) as plain `TEXT` casts so `DataFusion` can parse them.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_schema_qualified_custom_types(sql: &str) -> Result<String> {
    use sqlparser::ast::{
        visit_expressions_mut, visit_statements_mut, DataType, Expr, ObjectName, ObjectNamePart,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    /// Does `name` spell the built-in type `t` as `pg_catalog.<t>`?
    fn is_pg_type(name: &ObjectName, t: &str) -> bool {
        name.0.len() == 2
            && matches!(
                (&name.0[0], &name.0[1]),
                (
                    ObjectNamePart::Identifier(a),
                    ObjectNamePart::Identifier(b)
                ) if a.value.eq_ignore_ascii_case("pg_catalog")
                     && b.value.eq_ignore_ascii_case(t)
            )
    }

    let dialect = PostgreSqlDialect {};
    let mut stmts =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;

    let _ = visit_statements_mut(&mut stmts, |stmt| {
        let () = visit_expressions_mut(stmt, |e| {
            if let Expr::Cast { data_type, .. } = e {
                if let DataType::Custom(obj, _) = data_type {
                    if is_pg_type(obj, "text")
                        || is_pg_type(obj, "regtype")
                        || is_pg_type(obj, "regnamespace")
                        || is_pg_type(obj, "regclass")
                    {
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

/// Rename `array_upper(<arr>, <dim>)` calls to `DataFusion`'s
/// `array_length(<arr>, <dim>)`.
///
/// `PostgreSQL`'s `array_upper` returns the upper bound of the given array
/// dimension; `DataFusion` has no such function (getTypeInfo uses
/// `array_upper(current_schemas(false), 1)`). For the 1-based arrays these
/// catalog queries use, the upper bound of a dimension equals that dimension's
/// length, which `array_length` computes.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_array_upper_to_array_length(sql: &str) -> Result<String> {
    use sqlparser::ast::{
        visit_expressions_mut, visit_statements_mut, Expr, Function, Ident, ObjectName,
        ObjectNamePart,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    /// Wrap `inner` in `CAST(<inner> AS BIGINT)`, built from a parsed
    /// `CAST(NULL AS BIGINT)` template.
    ///
    /// `DataFusion`'s `array_length` returns an unsigned integer, but
    /// `generate_series` (which consumes this value in getTypeInfo) requires a
    /// signed integer, so the result is cast to BIGINT.
    fn bigint_cast(inner: Expr) -> Expr {
        let tmpl = Parser::parse_sql(&PostgreSqlDialect {}, "SELECT CAST(NULL AS BIGINT)").unwrap();
        let mut cast = match tmpl.into_iter().next().unwrap() {
            sqlparser::ast::Statement::Query(q) => match *q.body {
                sqlparser::ast::SetExpr::Select(s) => {
                    match s.projection.into_iter().next().unwrap() {
                        sqlparser::ast::SelectItem::UnnamedExpr(e) => e,
                        _ => unreachable!(),
                    }
                }
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };
        if let Expr::Cast { expr, .. } = &mut cast {
            **expr = inner;
        }
        cast
    }

    let dialect = PostgreSqlDialect {};
    let mut stmts =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;

    let _ = visit_statements_mut(&mut stmts, |stmt| {
        let _ = visit_expressions_mut(stmt, |e| {
            let is_array_upper = matches!(
                e,
                Expr::Function(Function { name, .. })
                    if matches!(name.0.last(), Some(ObjectNamePart::Identifier(i))
                        if i.value.eq_ignore_ascii_case("array_upper"))
            );
            if is_array_upper {
                if let Expr::Function(f) = e {
                    // array_length is a DataFusion built-in; call it unqualified.
                    f.name =
                        ObjectName(vec![ObjectNamePart::Identifier(Ident::new("array_length"))]);
                    let renamed = e.clone();
                    *e = bigint_cast(renamed);
                }
            }
            ControlFlow::<()>::Continue(())
        });
        ControlFlow::<()>::Continue(())
    });

    Ok(stmts
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(" "))
}

/// Rewrite a correlated boolean scalar subquery used as a predicate --
/// `(SELECT <a> <cmp> <b> FROM t WHERE <corr>)` -- into
/// `EXISTS (SELECT 1 FROM t WHERE (<corr>) AND (<a> <cmp> <b>))`.
///
/// pgjdbc's getTypeInfo filters with
/// `typrelid = 0 OR (SELECT c.relkind = 'c' FROM pg_class c WHERE c.oid = t.typrelid)`.
/// That non-aggregated correlated scalar subquery is rejected by `DataFusion`
/// ("Correlated scalar subquery must be aggregated to return at most one row").
/// Because the subquery's value is a boolean consumed as a predicate, it is
/// equivalent to an `EXISTS` whose body carries that boolean as an extra filter
/// (the correlation key is unique, so at most one row can match); the following
/// [`rewrite_exists_to_count`] pass then turns the `EXISTS` into a
/// `(SELECT count(*) ...) > 0` scalar `DataFusion` can plan.
///
/// Only a simple `SELECT` whose single projection is a comparison, with no
/// `GROUP BY` / `HAVING` / `DISTINCT` / set operation, is transformed; anything
/// else is left untouched. Must run before [`rewrite_exists_to_count`].
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
///
/// # Panics
///
/// Panics if the built-in `SELECT 1` template this borrows its replacement
/// projection from stops parsing, which would mean the bundled `sqlparser` no
/// longer accepts trivial `PostgreSQL`.
pub fn rewrite_boolean_scalar_subquery_to_exists(sql: &str) -> Result<String> {
    use sqlparser::ast::{
        visit_expressions_mut, visit_statements_mut, BinaryOperator, Expr, GroupByExpr, SelectItem,
        SetExpr, Statement, Value,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    /// Is `expr` a comparison whose value is boolean (so the scalar subquery
    /// projecting it is a predicate, not a data value)?
    fn is_boolean_comparison(expr: &Expr) -> bool {
        matches!(
            expr,
            Expr::BinaryOp {
                op: BinaryOperator::Eq
                    | BinaryOperator::NotEq
                    | BinaryOperator::Lt
                    | BinaryOperator::LtEq
                    | BinaryOperator::Gt
                    | BinaryOperator::GtEq,
                ..
            }
        )
    }

    /// If `select` is a simple single-comparison-projection SELECT, return a clone
    /// of that comparison expression (the predicate to fold into the EXISTS body).
    fn projection_predicate(select: &sqlparser::ast::Select) -> Option<Expr> {
        if select.projection.len() != 1 || select.having.is_some() || select.distinct.is_some() {
            return None;
        }
        if !matches!(&select.group_by, GroupByExpr::Expressions(g, _) if g.is_empty()) {
            return None;
        }
        let (SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. }) =
            &select.projection[0]
        else {
            return None;
        };
        if is_boolean_comparison(expr) {
            Some(expr.clone())
        } else {
            None
        }
    }

    let dialect = PostgreSqlDialect {};
    let mut stmts =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;

    // `SELECT 1` projection item, borrowed from a parsed template.
    let one_item: SelectItem = {
        let tmpl = Parser::parse_sql(&dialect, "SELECT 1").unwrap();
        match tmpl.into_iter().next().unwrap() {
            Statement::Query(q) => match *q.body {
                SetExpr::Select(s) => s.projection.into_iter().next().unwrap(),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        }
    };

    let _ = visit_statements_mut(&mut stmts, |stmt| {
        let _ = visit_expressions_mut(stmt, |e| {
            let predicate = match e {
                Expr::Subquery(subquery) => match subquery.body.as_ref() {
                    SetExpr::Select(select) => projection_predicate(select),
                    _ => None,
                },
                _ => None,
            };
            if let Some(pred) = predicate {
                let placeholder = Expr::Value(Value::Null.into());
                if let Expr::Subquery(mut subquery) = std::mem::replace(e, placeholder) {
                    if let SetExpr::Select(select) = subquery.body.as_mut() {
                        select.projection = vec![one_item.clone()];
                        select.selection = Some(match select.selection.take() {
                            Some(existing) => Expr::BinaryOp {
                                left: Box::new(Expr::Nested(Box::new(existing))),
                                op: BinaryOperator::And,
                                right: Box::new(Expr::Nested(Box::new(pred))),
                            },
                            None => pred,
                        });
                    }
                    *e = Expr::Exists {
                        subquery,
                        negated: false,
                    };
                }
            }
            ControlFlow::<()>::Continue(())
        });
        ControlFlow::<()>::Continue(())
    });

    Ok(stmts
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(" "))
}

/// Rewrite `EXISTS (<subquery>)` predicates into `(<subquery-with-count>) > 0`
/// (and `NOT EXISTS` into `... = 0`).
///
/// `DataFusion` 54 decorrelates correlated `EXISTS` only when it sits as a
/// top-level WHERE filter; it cannot physically plan the `EXISTS` operator when
/// it appears as a scalar value (inside `CASE WHEN EXISTS(...)`, a `SELECT`
/// list, etc.) - exactly how the `information_schema` views use it. It *can*,
/// however, decorrelate an equivalent correlated **scalar** subquery. So we turn
/// the EXISTS subquery's projection into `count(*)` and compare it against zero,
/// which `DataFusion` handles natively in any expression position. This replaces
/// the old `df_subquery_udf` rewrite for the one pattern `DataFusion` still can't
/// do on its own.
///
/// Only simple `SELECT` subqueries are transformed (no `GROUP BY`, `HAVING`, or
/// set operation), since `count(*)` is existence-equivalent only there; other
/// shapes - which the catalog views don't use - are left untouched.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_exists_to_count(sql: &str) -> Result<String> {
    use sqlparser::ast::{
        visit_expressions_mut, visit_statements_mut, BinaryOperator, Expr, GroupByExpr, SelectItem,
        SetExpr, Statement,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    let dialect = PostgreSqlDialect {};

    // Borrow a `count(*)` projection item and a `0` literal from a parsed
    // template, so we don't hand-build version-specific AST literal nodes.
    let (count_item, zero_expr): (SelectItem, Expr) = {
        let tmpl = Parser::parse_sql(&dialect, "SELECT count(*), 0")
            .map_err(|e| DataFusionError::External(Box::new(e)))?;
        match &tmpl[0] {
            Statement::Query(q) => match q.body.as_ref() {
                SetExpr::Select(s) => {
                    let count = s.projection[0].clone();
                    let zero = match &s.projection[1] {
                        SelectItem::UnnamedExpr(e) => e.clone(),
                        _ => unreachable!("template projection[1] is `0`"),
                    };
                    (count, zero)
                }
                _ => unreachable!("template body is a SELECT"),
            },
            _ => unreachable!("template is a query"),
        }
    };

    let mut stmts =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;

    let _ = visit_statements_mut(&mut stmts, |stmt| {
        visit_expressions_mut(stmt, |e| {
            if let Expr::Exists { subquery, negated } = e {
                // Only transform a plain SELECT with no grouping/having; count(*)
                // is existence-equivalent only there.
                let simple = match subquery.body.as_ref() {
                    SetExpr::Select(s) => {
                        s.having.is_none()
                            && matches!(&s.group_by, GroupByExpr::Expressions(g, _) if g.is_empty())
                    }
                    _ => false,
                };
                if simple {
                    let negated = *negated;
                    if let SetExpr::Select(select) = subquery.body.as_mut() {
                        select.projection = vec![count_item.clone()];
                        select.distinct = None;
                    }
                    let op = if negated {
                        BinaryOperator::Eq
                    } else {
                        BinaryOperator::Gt
                    };
                    *e = Expr::BinaryOp {
                        left: Box::new(Expr::Subquery(subquery.clone())),
                        op,
                        right: Box::new(zero_expr.clone()),
                    };
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

/// Rewrite a correlated `(SELECT c FROM t WHERE p LIMIT 1)` into
/// `(SELECT max(c) FROM t WHERE p)`, which decorrelates correctly.
///
/// `DataFusion` decorrelates a correlated scalar subquery by pulling its
/// correlation predicate up into a join, but for a scalar subquery it leaves an
/// inner `LIMIT` where it is (`PullUpCorrelatedExpr` strips the limit only for
/// `EXISTS` subqueries). The limit then applies to the subquery relation as a
/// whole instead of per outer row, so the join keeps one row overall and every
/// other outer row gets NULL. That is a wrong answer rather than an error,
/// which is why this is rewritten rather than left to fail loudly.
///
/// `max(c)` is equivalent here: with at most one matching row it returns that
/// row's value, and with none it returns NULL, exactly as `LIMIT 1` does. Where
/// several rows match, `LIMIT 1` without `ORDER BY` already picks an arbitrary
/// one, so choosing the largest is a narrowing of unspecified behaviour rather
/// than a change of defined behaviour.
///
/// Deliberately narrow, because each condition is load-bearing:
/// - Only correlated subqueries. An uncorrelated `LIMIT 1` subquery plans and
///   answers correctly today, and `max` would change which row it returns.
/// - Only `LIMIT 1`, no `ORDER BY`. `ORDER BY ... LIMIT 1` means "the first by
///   that ordering", which `max` does not express unless the ordering and the
///   projection are the same column. Those are left alone; they hit the same
///   `DataFusion` bug, so they are a known remaining gap rather than something
///   this silently mistranslates.
/// - Only a single non-aggregate projection with no `GROUP BY`, `HAVING`, or
///   `DISTINCT`, since `max` is equivalent only for that shape.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_correlated_limit_one_subquery_to_max(sql: &str) -> Result<String> {
    use sqlparser::ast::{visit_expressions_mut, visit_statements_mut, Expr, SelectItem, SetExpr};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    let dialect = PostgreSqlDialect {};
    let mut stmts =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;

    let _ = visit_statements_mut(&mut stmts, |stmt| {
        let _ = visit_expressions_mut(stmt, |e| {
            if let Expr::Subquery(query) = e {
                if let Some(projection) = correlated_limit_one_projection(query) {
                    let wrapped = wrap_in_max(&projection);
                    if let SetExpr::Select(select) = query.body.as_mut() {
                        select.projection = vec![SelectItem::UnnamedExpr(wrapped)];
                    }
                    // The limit has to go: keeping it would leave the same
                    // wrong-answer plan the aggregate is here to avoid.
                    query.limit_clause = None;
                }
            }
            ControlFlow::<()>::Continue(())
        });
        ControlFlow::<()>::Continue(())
    });

    Ok(stmts
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(" "))
}

/// Return the projected expression of a correlated `LIMIT 1` scalar subquery.
///
/// Returns None unless every condition in
/// [`rewrite_correlated_limit_one_subquery_to_max`] holds, so the caller can
/// treat Some as "safe to rewrite".
fn correlated_limit_one_projection(query: &sqlparser::ast::Query) -> Option<sqlparser::ast::Expr> {
    use sqlparser::ast::{GroupByExpr, SelectItem, SetExpr};

    if !limits_to_one_row_unordered(query) {
        return None;
    }

    let SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };

    if select.having.is_some() || select.distinct.is_some() || select.projection.len() != 1 {
        return None;
    }
    if !matches!(&select.group_by, GroupByExpr::Expressions(g, _) if g.is_empty()) {
        return None;
    }

    let (SelectItem::UnnamedExpr(projected)
    | SelectItem::ExprWithAlias {
        expr: projected, ..
    }) = &select.projection[0]
    else {
        return None;
    };
    if contains_aggregate_call(projected) {
        return None;
    }
    if !references_outer_column(select) {
        return None;
    }
    Some(projected.clone())
}

/// Report whether `query` is exactly `LIMIT 1` with no `ORDER BY` or `OFFSET`.
fn limits_to_one_row_unordered(query: &sqlparser::ast::Query) -> bool {
    use sqlparser::ast::{Expr, LimitClause, Value};

    if query.order_by.is_some() {
        return false;
    }
    match &query.limit_clause {
        Some(LimitClause::LimitOffset {
            limit: Some(Expr::Value(value)),
            offset,
            limit_by,
        }) => {
            if offset.is_some() || !limit_by.is_empty() {
                return false;
            }
            matches!(&value.value, Value::Number(n, _) if n == "1")
        }
        _ => false,
    }
}

/// Report whether `expr` contains an aggregate function call.
///
/// A projection that already aggregates decorrelates correctly on its own, and
/// wrapping it in another `max` would change what it computes.
fn contains_aggregate_call(expr: &sqlparser::ast::Expr) -> bool {
    use sqlparser::ast::{visit_expressions, Expr, ObjectNamePart};
    use std::ops::ControlFlow;

    /// The aggregate function names a projection can be built from.
    const AGGREGATES: &[&str] = &[
        "min",
        "max",
        "sum",
        "count",
        "avg",
        "array_agg",
        "string_agg",
        "bool_and",
        "bool_or",
    ];

    let mut found = false;
    let _ = visit_expressions(expr, |e| {
        if let Expr::Function(f) = e {
            let name = f
                .name
                .0
                .last()
                .and_then(|part| match part {
                    ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                    ObjectNamePart::Function(_) => None,
                })
                .unwrap_or_default();
            if AGGREGATES.contains(&name.as_str()) {
                found = true;
                return ControlFlow::Break(());
            }
        }
        ControlFlow::<()>::Continue(())
    });
    found
}

/// Report whether `select` references a column qualified by a name its own
/// `FROM` does not define, which is what makes a subquery correlated.
///
/// Unqualified columns are treated as local: without full name resolution they
/// cannot be attributed to an outer query, and assuming they are local keeps
/// this rewrite from firing on uncorrelated subqueries it must leave alone.
fn references_outer_column(select: &sqlparser::ast::Select) -> bool {
    use sqlparser::ast::{visit_expressions, Expr, ObjectNamePart, TableFactor};
    use std::collections::HashSet;
    use std::ops::ControlFlow;

    let mut local: HashSet<String> = HashSet::new();
    for table in &select.from {
        let mut relations = vec![&table.relation];
        for join in &table.joins {
            relations.push(&join.relation);
        }
        for relation in relations {
            match relation {
                TableFactor::Table { name, alias, .. } => {
                    if let Some(alias) = alias {
                        local.insert(alias.name.value.to_lowercase());
                    } else if let Some(ObjectNamePart::Identifier(i)) = name.0.last() {
                        local.insert(i.value.to_lowercase());
                    }
                }
                TableFactor::Derived { alias, .. } | TableFactor::NestedJoin { alias, .. } => {
                    if let Some(alias) = alias {
                        local.insert(alias.name.value.to_lowercase());
                    }
                }
                _ => {}
            }
        }
    }

    let mut correlated = false;
    let mut inspect = |expr: &Expr| {
        let _ = visit_expressions(expr, |e| {
            if let Expr::CompoundIdentifier(parts) = e {
                if let Some(first) = parts.first() {
                    if !local.contains(&first.value.to_lowercase()) {
                        correlated = true;
                        return ControlFlow::Break(());
                    }
                }
            }
            ControlFlow::<()>::Continue(())
        });
    };

    if let Some(selection) = &select.selection {
        inspect(selection);
    }
    for item in &select.projection {
        if let sqlparser::ast::SelectItem::UnnamedExpr(e)
        | sqlparser::ast::SelectItem::ExprWithAlias { expr: e, .. } = item
        {
            inspect(e);
        }
    }
    correlated
}

/// Wrap `expr` in a `max(...)` call, built by parsing a template so no
/// version-specific function AST has to be hand-constructed.
fn wrap_in_max(expr: &sqlparser::ast::Expr) -> sqlparser::ast::Expr {
    use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, FunctionArguments, SetExpr};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    let template =
        Parser::parse_sql(&PostgreSqlDialect {}, "SELECT max(x)").expect("max() template parses");
    let mut call = match template.into_iter().next() {
        Some(sqlparser::ast::Statement::Query(q)) => match *q.body {
            SetExpr::Select(mut s) => match s.projection.remove(0) {
                sqlparser::ast::SelectItem::UnnamedExpr(e) => e,
                _ => unreachable!("template projection is an expression"),
            },
            _ => unreachable!("template body is a SELECT"),
        },
        _ => unreachable!("template is a query"),
    };

    if let Expr::Function(function) = &mut call {
        if let FunctionArguments::List(list) = &mut function.args {
            list.args = vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(expr.clone()))];
        }
    }
    call
}

/// Rewrite a multi-column (row-constructor) `IN`-subquery into a correlated
/// `EXISTS`, which `DataFusion` can plan (it rejects multi-column `IN` subqueries
/// with "the subquery should only return one column").
///
/// `(a, b, c) IN (SELECT p, q, r FROM t WHERE w)` becomes
/// `EXISTS (SELECT 1 FROM t WHERE w AND p = a AND q = b AND r = c)` (and `NOT IN`
/// -> `NOT EXISTS`). Single-column `IN` is left alone - `DataFusion` handles it.
/// Used by `information_schema.element_types`, whose visibility filter is a
/// 4-column `IN (SELECT ... FROM data_type_privileges)`.
///
/// MUST run after [`rewrite_exists_to_count`] so the `EXISTS` it produces reaches
/// `DataFusion` as a native `WHERE` predicate rather than being turned into a
/// `(SELECT count(*) ...) > 0` scalar subquery.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
///
/// # Panics
///
/// The per-column rewrite unwraps each projected expression, which is sound
/// because the subquery is left untouched unless every projection item was
/// already checked to be a plain expression.
pub fn rewrite_tuple_in_subquery_to_exists(sql: &str) -> Result<String> {
    use sqlparser::ast::{
        visit_expressions_mut, visit_statements_mut, BinaryOperator, Expr, SelectItem, SetExpr,
        Value,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    /// The bare expression a projection item selects, or `None` for `*`/wildcards.
    fn projected_expr(item: &SelectItem) -> Option<&Expr> {
        match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => Some(e),
            _ => None,
        }
    }

    let mut stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .map_err(|e| DataFusionError::External(Box::new(e)))?;
    let _ = visit_statements_mut(&mut stmts, |stmt| {
        let () = visit_expressions_mut(stmt, |e| {
            if let Expr::InSubquery {
                expr,
                subquery,
                negated,
            } = e
            {
                // Only the multi-column row-constructor form.
                let elems = match expr.as_ref() {
                    Expr::Tuple(elems) if elems.len() > 1 => elems.clone(),
                    _ => return ControlFlow::<()>::Continue(()),
                };
                let negated = *negated;
                let mut new_subquery = subquery.clone();
                if let SetExpr::Select(select) = new_subquery.body.as_mut() {
                    // Arities must match and every projection must be a plain expression.
                    if select.projection.len() != elems.len()
                        || select
                            .projection
                            .iter()
                            .any(|p| projected_expr(p).is_none())
                    {
                        return ControlFlow::Continue(());
                    }
                    // Append `projected_col = tuple_elem` for each column to the WHERE.
                    let mut condition = select.selection.clone();
                    for (item, tuple_elem) in select.projection.iter().zip(elems.iter()) {
                        let equality = Expr::BinaryOp {
                            left: Box::new(projected_expr(item).unwrap().clone()),
                            op: BinaryOperator::Eq,
                            right: Box::new(tuple_elem.clone()),
                        };
                        condition = Some(match condition {
                            Some(existing) => Expr::BinaryOp {
                                left: Box::new(existing),
                                op: BinaryOperator::And,
                                right: Box::new(equality),
                            },
                            None => equality,
                        });
                    }
                    select.selection = condition;
                    select.projection = vec![SelectItem::UnnamedExpr(Expr::Value(
                        Value::Number("1".to_string(), false).into(),
                    ))];
                    *e = Expr::Exists {
                        subquery: new_subquery,
                        negated,
                    };
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

/// The set-returning functions we model as scalar `List<Struct>` functions, so
/// `(srf(x)).field` in a projection can be unnested. See [`rewrite_srf_to_unnest`].
const PROJECTION_SRFS: &[&str] = &["aclexplode", "_pg_expandarray", "pg_options_to_table"];

/// Rewrite `(srf(x)).field` projections (set-returning function used as a scalar
/// value, which `DataFusion` can't plan) into an `unnest`-of-`List<Struct>` form
/// that it can.
///
/// `PostgreSQL`'s `information_schema` views call SRFs like `aclexplode` directly in
/// the SELECT list: `(aclexplode(acl)).grantee`. We register those SRFs as scalar
/// functions returning `List<Struct{...}>` (see `register_pg_options_to_table`
/// etc.), then transform
///
/// ```sql
/// SELECT (srf(x)).a, (srf(x)).b, other FROM t WHERE w
/// ```
/// into
/// ```sql
/// SELECT __srf_unnest['a'], __srf_unnest['b'], other
/// FROM (SELECT *, unnest(srf(x)) AS __srf_unnest FROM t WHERE w) AS __srf_src
/// ```
///
/// which `DataFusion` executes (unnest in projection + `struct['field']` access).
/// Only the simple shape the catalog views use is handled: a single distinct
/// `srf(x)` per `SELECT`, accessed in the projection. Other shapes are left
/// unchanged. `UNION` branches are each rewritten independently.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`, or if the rendered
/// result does not parse again for the closing dot-to-subscript pass.
pub fn rewrite_srf_to_unnest(sql: &str) -> Result<String> {
    use sqlparser::ast::{Query, SetExpr, Statement, TableFactor};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    /// Rewrite the SRF projections of one query and of every CTE it defines.
    fn walk_query(q: &mut Query) {
        if let Some(with) = q.with.as_mut() {
            for cte in &mut with.cte_tables {
                walk_query(&mut cte.query);
            }
        }
        walk_setexpr(q.body.as_mut());
    }

    /// Rewrite the SRF projections inside a FROM item that is a subquery.
    fn recurse_into_derived_subquery(tf: &mut TableFactor) {
        if let TableFactor::Derived { subquery, .. } = tf {
            walk_query(subquery);
        }
    }

    /// Rewrite the SRF projections of one query body, both sides of a set
    /// operation included.
    fn walk_setexpr(body: &mut SetExpr) {
        match body {
            SetExpr::Select(select) => {
                // Recurse into FROM-clause subqueries (and joined ones) first, so
                // an SRF aliased inside a derived table is handled before the
                // outer SELECT's `(alias).field` access is rewritten to a bracket.
                for twj in &mut select.from {
                    recurse_into_derived_subquery(&mut twj.relation);
                    for join in &mut twj.joins {
                        recurse_into_derived_subquery(&mut join.relation);
                    }
                }
                rewrite_select(select);
            }
            SetExpr::Query(q) => walk_query(q),
            SetExpr::SetOperation { left, right, .. } => {
                walk_setexpr(left);
                walk_setexpr(right);
            }
            _ => {}
        }
    }

    let dialect = PostgreSqlDialect {};
    let mut stmts =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;
    for stmt in &mut stmts {
        if let Statement::Query(q) = stmt {
            walk_query(q);
        }
    }
    // Finally, convert any remaining composite field access `(expr).field` into
    // DataFusion's struct-subscript form `expr['field']` (e.g. the outer query's
    // `(ss.x).n` reference to an unnested SRF column).
    let out = stmts
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    convert_dot_field_to_subscript(&out)
}

/// Convert composite field access `(<expr>).<field>` into the struct-subscript
/// form `(<expr>)['<field>']` that `DataFusion` supports. Inline-SRF accesses have
/// already been replaced with `__srf_unnest['field']` by the time this runs, so
/// the remaining `.field` Dot accesses are column/struct references.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
fn convert_dot_field_to_subscript(sql: &str) -> Result<String> {
    use sqlparser::ast::{visit_expressions_mut, visit_statements_mut, AccessExpr, Expr, Value};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    let mut stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .map_err(|e| DataFusionError::External(Box::new(e)))?;
    let _ = visit_statements_mut(&mut stmts, |stmt| {
        let () = visit_expressions_mut(stmt, |e| {
            if let Expr::CompoundFieldAccess { root, access_chain } = e {
                // Only treat a `.field` access as a *struct field* (-> `['field']`)
                // when the root is a genuine composite-typed value: a parenthesized
                // expression `(ss.x).n` or a direct function call `(srf(a)).f`. A
                // bare `tbl.arraycol[1]` parses as root `tbl` with chain
                // `[Dot(arraycol), Subscript(1)]` - that leading Dot is a normal
                // qualified column reference and must stay `tbl.arraycol`, not
                // become `tbl['arraycol']` (which DataFusion reads as subscripting
                // a column literally named `tbl`).
                if matches!(root.as_ref(), Expr::Nested(_) | Expr::Function(_)) {
                    for acc in access_chain.iter_mut() {
                        if let AccessExpr::Dot(Expr::Identifier(field)) = acc {
                            let name = field.value.clone();
                            *acc = AccessExpr::Subscript(sqlparser::ast::Subscript::Index {
                                index: Expr::Value(Value::SingleQuotedString(name).into()),
                            });
                        }
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

/// Is `e` an `(srf(args)).field` access on one of [`PROJECTION_SRFS`]? If so,
/// return the inner `srf(args)` function expression.
fn srf_field_access(e: &sqlparser::ast::Expr) -> Option<sqlparser::ast::Expr> {
    use sqlparser::ast::{AccessExpr, Expr, FunctionArguments, ObjectNamePart};
    if let Expr::CompoundFieldAccess { root, access_chain } = e {
        if access_chain.len() == 1 {
            if let AccessExpr::Dot(_) = &access_chain[0] {
                if let Expr::Nested(inner) = root.as_ref() {
                    if let Expr::Function(f) = inner.as_ref() {
                        let name = f
                            .name
                            .0
                            .last()
                            .and_then(|p| match p {
                                ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                                ObjectNamePart::Function(_) => None,
                            })
                            .unwrap_or_default();
                        if PROJECTION_SRFS.contains(&name.as_str())
                            && matches!(f.args, FunctionArguments::List(_))
                        {
                            return Some((**inner).clone());
                        }
                    }
                }
            }
        }
    }
    None
}

/// The expression a `SELECT` list item projects, mutably, or `None` for a
/// wildcard item that projects no single expression.
fn projection_item_expr_mut(item: &mut SelectItem) -> Option<&mut Expr> {
    match item {
        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => Some(e),
        _ => None,
    }
}

/// Is `e` a bare call to a projection SRF (e.g. `_pg_expandarray(c.conkey)`),
/// not field-accessed? Such aliased SRFs are wrapped in `unnest(...)` in place.
fn is_bare_projection_srf_call(e: &Expr) -> bool {
    if let Expr::Function(f) = e {
        let name = f
            .name
            .0
            .last()
            .and_then(|p| match p {
                ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                ObjectNamePart::Function(_) => None,
            })
            .unwrap_or_default();
        return PROJECTION_SRFS.contains(&name.as_str())
            && matches!(f.args, FunctionArguments::List(_));
    }
    false
}

/// Which inline `(srf(x)).field` accesses a single `SELECT` projection makes.
///
/// Only a projection that accesses exactly one distinct call can be served from
/// one unnested column, so the three cases have to be told apart.
enum ProjectionSrfUsage {
    /// The projection field-accesses no set-returning function.
    Absent,
    /// Every field access is on this one set-returning function call. The call
    /// is boxed because an `Expr` dwarfs the empty variants.
    Single(Box<Expr>),
    /// The projection field-accesses two or more different calls.
    Several,
}

/// Classify the inline `(srf(x)).field` accesses in `select`'s projection.
fn projection_srf_usage(select: &mut sqlparser::ast::Select) -> ProjectionSrfUsage {
    let mut found: Option<Expr> = None;
    let mut has_multiple_srfs = false;
    for item in &mut select.projection {
        if let Some(expr) = projection_item_expr_mut(item) {
            let _ = visit_expressions_mut(expr, |e| {
                if let Some(call) = srf_field_access(e) {
                    match &found {
                        None => found = Some(call),
                        Some(prev) if prev.to_string() != call.to_string() => {
                            has_multiple_srfs = true;
                        }
                        _ => {}
                    }
                }
                ControlFlow::<()>::Continue(())
            });
        }
    }
    match found {
        Some(_) if has_multiple_srfs => ProjectionSrfUsage::Several,
        Some(call) => ProjectionSrfUsage::Single(Box::new(call)),
        None => ProjectionSrfUsage::Absent,
    }
}

/// Build the projection item `unnest(<srf>) AS __srf_unnest` from a parsed
/// template, so no version-specific function AST has to be hand-built.
fn unnest_projection_item(srf: &Expr) -> SelectItem {
    let template = Parser::parse_sql(&PostgreSqlDialect {}, "SELECT unnest(NULL) AS __srf_unnest")
        .expect("unnest template parses");
    let mut item = match template.into_iter().next().unwrap() {
        Statement::Query(q) => match *q.body {
            SetExpr::Select(s) => s.projection.into_iter().next().unwrap(),
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    // Substitute the real srf call for the NULL placeholder argument.
    if let SelectItem::ExprWithAlias {
        expr: Expr::Function(f),
        ..
    } = &mut item
    {
        if let FunctionArguments::List(list) = &mut f.args {
            if let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(arg))) = list.args.first_mut() {
                *arg = srf.clone();
            }
        }
    }
    item
}

/// Point every projection reference to the SRF call `srf_text` at the unnested
/// `__srf_unnest` column: an inline `(srf(x)).field` becomes
/// `__srf_unnest['field']`, and a bare `srf(x)` projection (e.g. `srf(x) AS
/// keys`) becomes `__srf_unnest` itself.
///
/// Routing both forms through the one unnested value is what lets a `SELECT`
/// that aliases the SRF and also accesses its fields inline be planned; the
/// alias would otherwise stay a raw `List(Struct)`.
fn read_srf_projections_from_unnested_column(select: &mut sqlparser::ast::Select, srf_text: &str) {
    use sqlparser::ast::AccessExpr;

    for item in &mut select.projection {
        if let Some(expr) = projection_item_expr_mut(item) {
            if is_bare_projection_srf_call(expr) && expr.to_string() == srf_text {
                *expr = Expr::Identifier(Ident::new("__srf_unnest"));
                continue;
            }
            let _ = visit_expressions_mut(expr, |e| {
                if let Some(call) = srf_field_access(e) {
                    if call.to_string() == srf_text {
                        if let Expr::CompoundFieldAccess { access_chain, .. } = e {
                            if let AccessExpr::Dot(Expr::Identifier(field)) = &access_chain[0] {
                                *e = bracket_access("__srf_unnest", &field.value);
                            }
                        }
                    }
                }
                ControlFlow::<()>::Continue(())
            });
        }
    }
}

/// The table qualifiers `select`'s FROM clause defines: each plain table's
/// alias, or its bare name when it has none, lowercased.
fn from_clause_qualifiers(select: &sqlparser::ast::Select) -> std::collections::HashSet<String> {
    let mut qualifiers = std::collections::HashSet::new();
    for twj in &select.from {
        let mut record_table_qualifier = |tf: &TableFactor| {
            if let TableFactor::Table { name, alias, .. } = tf {
                if let Some(a) = alias {
                    qualifiers.insert(a.name.value.to_lowercase());
                } else if let Some(ObjectNamePart::Identifier(i)) = name.0.last() {
                    qualifiers.insert(i.value.to_lowercase());
                }
            }
        };
        record_table_qualifier(&twj.relation);
        for j in &twj.joins {
            record_table_qualifier(&j.relation);
        }
    }
    qualifiers
}

/// Move `select`'s FROM and WHERE into a derived table aliased `__srf_src` that
/// also projects `unnest_item`, leaving the outer `SELECT` to read the unnested
/// column from it.
///
/// The inner query carries only the source and the unnest; grouping, ordering
/// and distinctness stay on the outer `SELECT`, which still owns them.
fn move_source_into_unnesting_derived_table(
    select: &mut sqlparser::ast::Select,
    unnest_item: SelectItem,
) {
    let mut inner = select.clone();
    inner.projection = vec![
        SelectItem::Wildcard(sqlparser::ast::WildcardAdditionalOptions::default()),
        unnest_item,
    ];
    inner.selection.clone_from(&select.selection);
    inner.group_by = sqlparser::ast::GroupByExpr::Expressions(vec![], vec![]);
    inner.having = None;
    inner.distinct = None;
    inner.sort_by = vec![];
    inner.qualify = None;

    let inner_query = Query {
        with: None,
        body: Box::new(SetExpr::Select(Box::new(inner))),
        order_by: None,
        limit_clause: None,
        fetch: None,
        locks: vec![],
        for_clause: None,
        settings: None,
        format_clause: None,
        pipe_operators: vec![],
    };

    // Build a `FROM (SELECT 1) AS __srf_src` template, then swap the placeholder
    // subquery for our inner query (avoids enumerating sqlparser's struct fields).
    let mut derived: TableWithJoins = {
        let template = Parser::parse_sql(
            &PostgreSqlDialect {},
            "SELECT 1 FROM (SELECT 1) AS __srf_src",
        )
        .expect("derived table template parses");
        match template.into_iter().next().unwrap() {
            Statement::Query(q) => match *q.body {
                SetExpr::Select(s) => s.from.into_iter().next().unwrap(),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        }
    };
    if let TableFactor::Derived { subquery, .. } = &mut derived.relation {
        **subquery = inner_query;
    }

    // The outer SELECT keeps its (rewritten) projection and post-FROM clauses,
    // but its FROM is now the derived subquery and its WHERE moved inward.
    select.from = vec![derived];
    select.selection = None;
}

/// Re-qualify projection references from the tables the original FROM named to
/// the derived table `__srf_src` that now hides them (e.g. `pg_class.oid` ->
/// `__srf_src.oid`).
fn requalify_projection_to_srf_source(
    select: &mut sqlparser::ast::Select,
    original_from_qualifiers: &std::collections::HashSet<String>,
) {
    if original_from_qualifiers.is_empty() {
        return;
    }
    for item in &mut select.projection {
        if let Some(expr) = projection_item_expr_mut(item) {
            let _ = visit_expressions_mut(expr, |e| {
                if let Expr::CompoundIdentifier(parts) = e {
                    if parts.len() == 2
                        && original_from_qualifiers.contains(&parts[0].value.to_lowercase())
                    {
                        parts[0] = Ident::new("__srf_src");
                    }
                }
                ControlFlow::<()>::Continue(())
            });
        }
    }
}

/// Rewrite one `SELECT` that uses `(srf(x)).field` in its projection. See
/// [`rewrite_srf_to_unnest`].
fn rewrite_select(select: &mut sqlparser::ast::Select) {
    let srf = match projection_srf_usage(select) {
        // With no inline field access, handle the simple bare-aliased form in place
        // (`srf(x) AS alias` -> `unnest(srf(x)) AS alias`), the shape the internal
        // _pg_expandarray views use; the outer query's `(alias).field` then becomes
        // `alias['field']` via the final dot->subscript pass. When an inline access
        // IS present (as in the driver's getPrimaryKeys query, which both aliases the
        // SRF and accesses its fields inline), fall through to the derived-table
        // rewrite below, which routes both through one unnested column -- the in-place
        // wrap cannot, because a bare `unnest(...) AS alias` leaves an inline
        // `(srf(x)).field` in the same SELECT unrewritten on a List(Struct) value.
        ProjectionSrfUsage::Absent => {
            for item in &mut select.projection {
                if let Some(expr) = projection_item_expr_mut(item) {
                    if is_bare_projection_srf_call(expr) {
                        let inner = expr.clone();
                        *expr = wrap_in_unnest(inner);
                    }
                }
            }
            return;
        }
        // More than one distinct SRF cannot share a single unnested column, so
        // such a SELECT is left unchanged.
        ProjectionSrfUsage::Several => return,
        ProjectionSrfUsage::Single(call) => *call,
    };

    let srf_text_key = srf.to_string();
    let unnest_item = unnest_projection_item(&srf);
    read_srf_projections_from_unnested_column(select, &srf_text_key);
    // The qualifiers have to be read before the FROM is replaced, since the
    // tables they name disappear behind the derived table.
    let original_from_qualifiers = from_clause_qualifiers(select);
    move_source_into_unnesting_derived_table(select, unnest_item);
    requalify_projection_to_srf_source(select, &original_from_qualifiers);
}

/// Build the expression `<root>['<field>']` (struct field access) via a parsed
/// template, substituting the field name.
fn bracket_access(root: &str, field: &str) -> sqlparser::ast::Expr {
    use sqlparser::ast::{AccessExpr, Expr, Subscript, Value};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    let sql = format!("SELECT {root}['__field__']");
    let q = Parser::parse_sql(&PostgreSqlDialect {}, &sql).unwrap();
    let mut expr = match q.into_iter().next().unwrap() {
        sqlparser::ast::Statement::Query(q) => match *q.body {
            sqlparser::ast::SetExpr::Select(s) => match s.projection.into_iter().next().unwrap() {
                sqlparser::ast::SelectItem::UnnamedExpr(e) => e,
                _ => unreachable!(),
            },
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    if let Expr::CompoundFieldAccess { access_chain, .. } = &mut expr {
        if let Some(AccessExpr::Subscript(Subscript::Index { index })) = access_chain.first_mut() {
            *index = Expr::Value(Value::SingleQuotedString(field.to_string()).into());
        }
    }
    expr
}

/// Wrap `inner` in a call `unnest(<inner>)` via a parsed template.
fn wrap_in_unnest(inner: sqlparser::ast::Expr) -> sqlparser::ast::Expr {
    use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, FunctionArguments};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    let q = Parser::parse_sql(&PostgreSqlDialect {}, "SELECT unnest(NULL)").unwrap();
    let mut expr = match q.into_iter().next().unwrap() {
        sqlparser::ast::Statement::Query(q) => match *q.body {
            sqlparser::ast::SetExpr::Select(s) => match s.projection.into_iter().next().unwrap() {
                sqlparser::ast::SelectItem::UnnamedExpr(e) => e,
                _ => unreachable!(),
            },
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    if let Expr::Function(f) = &mut expr {
        if let FunctionArguments::List(list) = &mut f.args {
            if let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(arg))) = list.args.first_mut() {
                *arg = inner;
            }
        }
    }
    expr
}

/// Rewrite casts to the `information_schema` standard domain types
/// (`sql_identifier`, `character_data`, `cardinal_number`, `yes_or_no`,
/// `time_stamp`) into their underlying base types. Those domains are thin,
/// value-preserving wrappers over base types that exist only for SQL-standard
/// conformance, but `DataFusion` doesn't know them - so every
/// `expr::information_schema.<domain>` cast in the `information_schema` views
/// fails to plan. Mapping them to the base type makes the views executable.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_information_schema_casts(sql: &str) -> Result<String> {
    use sqlparser::ast::{
        visit_expressions_mut, visit_statements_mut, DataType, Expr, ObjectName, ObjectNamePart,
        TimezoneInfo,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    /// Map an `information_schema.<domain>` cast target to its base `DataType`.
    fn base_type(name: &ObjectName) -> Option<DataType> {
        if name.0.len() != 2 {
            return None;
        }
        let (ObjectNamePart::Identifier(schema), ObjectNamePart::Identifier(ty)) =
            (&name.0[0], &name.0[1])
        else {
            return None;
        };
        if !schema.value.eq_ignore_ascii_case("information_schema") {
            return None;
        }
        match ty.value.to_ascii_lowercase().as_str() {
            "sql_identifier" | "character_data" | "yes_or_no" => Some(DataType::Text),
            "cardinal_number" => Some(DataType::Integer(None)),
            "time_stamp" => Some(DataType::Timestamp(None, TimezoneInfo::None)),
            _ => None,
        }
    }

    let dialect = PostgreSqlDialect {};
    let mut stmts =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;

    let _ = visit_statements_mut(&mut stmts, |stmt| {
        let () = visit_expressions_mut(stmt, |e| {
            if let Expr::Cast { data_type, .. } = e {
                match data_type {
                    DataType::Custom(obj, _) => {
                        if let Some(base) = base_type(obj) {
                            *data_type = base;
                        }
                    }
                    // `x::character varying` / `x::varchar` with no length is how
                    // the information_schema views spell their text casts;
                    // DataFusion can't plan a cast to unbounded varchar, so map it
                    // to TEXT. Length-qualified varchar(n) is left as-is.
                    DataType::CharacterVarying(None) | DataType::Varchar(None) => {
                        *data_type = DataType::Text;
                    }
                    _ => {}
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

/// Replace casts to regtype / `pg_catalog.regtype` with TEXT,
/// or drop them entirely if they are immediately followed by a TEXT cast.
/// Neutralize the remaining `<expr>::regclass` and `<expr>::oid` casts that the
/// literal-oriented `replace_regclass` / `rewrite_oid_cast` passes don't cover
/// (e.g. `c.oid::regclass`, `proargtypes::oid`).
///
/// `regclass`/`oid` are display types over an integer OID, so for a non-literal
/// argument the cast is value-preserving and `DataFusion` can't plan it; we simply
/// drop the cast, keeping the inner expression. String-literal casts
/// (`'pg_class'::regclass`) are left untouched - those need the OID lookup
/// `replace_regclass` performs - as are numeric `::oid` casts (already mapped to
/// BIGINT by `rewrite_oid_cast`).
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn drop_redundant_oid_and_regclass_casts(sql: &str) -> Result<String> {
    use sqlparser::ast::{
        visit_expressions_mut, visit_statements_mut, DataType, Expr, ObjectNamePart, Value,
        ValueWithSpan,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    /// Is `e` a single-quoted string literal, the operand shape that needs the
    /// name-to-OID lookup rather than a dropped cast?
    fn is_string_literal(e: &Expr) -> bool {
        matches!(
            e,
            Expr::Value(ValueWithSpan {
                value: Value::SingleQuotedString(_),
                ..
            })
        )
    }
    /// Is `e` a numeric literal, i.e. already the integer an OID is stored as?
    fn is_number(e: &Expr) -> bool {
        matches!(
            e,
            Expr::Value(ValueWithSpan {
                value: Value::Number(_, _),
                ..
            })
        )
    }
    /// Is `dt` the unqualified custom type name `oid`?
    fn is_oid_custom(dt: &DataType) -> bool {
        matches!(dt, DataType::Custom(obj, _)
            if obj.0.len() == 1
            && matches!(&obj.0[0], ObjectNamePart::Identifier(i) if i.value.eq_ignore_ascii_case("oid")))
    }

    let mut stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .map_err(|e| DataFusionError::External(Box::new(e)))?;
    let _ = visit_statements_mut(&mut stmts, |stmt| {
        let () = visit_expressions_mut(stmt, |e| {
            if let Expr::Cast {
                expr, data_type, ..
            } = e
            {
                let drop = match data_type {
                    DataType::Regclass => !is_string_literal(expr),
                    dt if is_oid_custom(dt) => !is_string_literal(expr) && !is_number(expr),
                    _ => false,
                };
                if drop {
                    *e = (**expr).clone();
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

/// Drop `<expr>::oid[]` casts, keeping the inner expression.
///
/// `oid[]` is an integer array, and this catalog now loads the columns these
/// casts apply to as integer arrays already: `proargtypes` (`oidvector`) and
/// `proallargtypes` (`_oid`) both map to `List<Int64>` (see
/// [`crate::db_table::map_pg_type`]). So in `COALESCE(proallargtypes,
/// proargtypes::oid[])` both arms are already `List<Int64>` and the `::oid[]`
/// cast is a value-preserving no-op that `DataFusion` still can't plan (it doesn't
/// know the `oid` element type). Dropping the cast leaves a clean
/// `COALESCE(List<Int64>, List<Int64>)`. Used by the `element_types` and
/// `parameters` `information_schema` views.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn drop_oid_array_cast(sql: &str) -> Result<String> {
    use sqlparser::ast::{
        visit_expressions_mut, visit_statements_mut, ArrayElemTypeDef, DataType, Expr,
        ObjectNamePart,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    /// True for an array-of-oid cast like `proargtypes::oid[]`, which parses as
    /// `Array(SquareBracket(Custom(oid)))` / `Array(AngleBracket(Custom(oid)))`.
    fn is_oid_array(dt: &DataType) -> bool {
        let DataType::Array(elem) = dt else {
            return false;
        };
        let inner = match elem {
            ArrayElemTypeDef::SquareBracket(inner, _) | ArrayElemTypeDef::AngleBracket(inner) => {
                Some(inner.as_ref())
            }
            _ => None,
        };
        matches!(inner, Some(DataType::Custom(obj, _))
            if obj.0.len() == 1
            && matches!(&obj.0[0], ObjectNamePart::Identifier(i) if i.value.eq_ignore_ascii_case("oid")))
    }

    let mut stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .map_err(|e| DataFusionError::External(Box::new(e)))?;
    let _ = visit_statements_mut(&mut stmts, |stmt| {
        let () = visit_expressions_mut(stmt, |e| {
            if let Expr::Cast {
                data_type, expr, ..
            } = e
            {
                if is_oid_array(data_type) {
                    *e = (**expr).clone();
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

/// Rewrite casts to catalog types that `DataFusion`'s planner rejects by name into the
/// concrete types this catalog stores those columns as.
///
/// Two catalog type names reach the planner only inside view bodies and are rejected
/// outright ("Unsupported SQL type"):
///
/// * `anyarray` - the `pg_statistic.stavalues*` columns (and the `pg_stats` view's
///   `NULL::anyarray` branches over them) are stored as `text`, so `::anyarray` becomes
///   `::text`. This lets the `pg_stats` CASE branches share a common type with the
///   text-typed `stavalues*` columns.
/// * `name[]` - name arrays (e.g. the `pg_policies` `::name[]` role lists) are stored as
///   `text[]`, so `name[]` becomes `text[]`.
///
/// `oid[]` is handled separately by [`drop_oid_array_cast`], which removes it where the
/// underlying column is already an integer array; this rewrite leaves it untouched.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_text_backed_type_casts(sql: &str) -> Result<String> {
    use sqlparser::ast::{
        visit_expressions_mut, visit_statements_mut, ArrayElemTypeDef, DataType, Expr,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    /// The element type of an array cast (`name[]` parses as `Array(SquareBracket(..))`).
    fn array_elem(dt: &DataType) -> Option<&DataType> {
        let DataType::Array(elem) = dt else {
            return None;
        };
        match elem {
            ArrayElemTypeDef::SquareBracket(inner, _)
            | ArrayElemTypeDef::AngleBracket(inner)
            | ArrayElemTypeDef::Parenthesis(inner) => Some(inner.as_ref()),
            ArrayElemTypeDef::None => None,
        }
    }

    let mut stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .map_err(|e| DataFusionError::External(Box::new(e)))?;

    let _ = visit_statements_mut(&mut stmts, |stmt| {
        visit_expressions_mut(stmt, |e| {
            if let Expr::Cast { data_type, .. } = e {
                let is_anyarray = matches!(data_type, DataType::Custom(obj, _) if object_name_matches(obj, "anyarray"));
                let is_name_array = array_elem(data_type)
                    .is_some_and(|inner| matches!(inner, DataType::Custom(obj, _) if object_name_matches(obj, "name")));
                if is_anyarray {
                    *data_type = DataType::Text;
                } else if is_name_array {
                    *data_type = DataType::Array(ArrayElemTypeDef::SquareBracket(
                        Box::new(DataType::Text),
                        None,
                    ));
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

/// Expand the whole-row composite arguments of `information_schema._pg_truetypid`
/// and `_pg_truetypmod` into the individual columns their bodies read.
///
/// `PostgreSQL` declares these as `_pg_truetypid(pg_attribute, pg_type)` and the
/// catalog views call them as `_pg_truetypid(a.*, t.*)`. `DataFusion` cannot pass a
/// whole row (`a.*`) as a single scalar argument, so we rewrite each call into
/// the three columns the function actually touches:
///
/// * `_pg_truetypid(a.*, t.*)`  -> `_pg_truetypid(a.atttypid,  t.typtype, t.typbasetype)`
/// * `_pg_truetypmod(a.*, t.*)` -> `_pg_truetypmod(a.atttypmod, t.typtype, t.typtypmod)`
///
/// The first wildcard's qualifier (`a`) is the `pg_attribute` alias and the
/// second (`t`) the `pg_type` alias; both are taken from the call site so the
/// rewrite works regardless of the aliases a view chose. Calls that aren't in the
/// `(x.*, y.*)` shape are left untouched. Pairs with
/// [`crate::user_functions::register_pg_truetypid_helpers`].
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_pg_truetypid_composite_args(sql: &str) -> Result<String> {
    use sqlparser::ast::{
        visit_expressions_mut, visit_statements_mut, Expr, FunctionArg, FunctionArgExpr,
        FunctionArguments, Ident, ObjectNamePart,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    /// The `(source-row argument index, column)` triples each function expands
    /// into, keyed by the function's bare (last-segment) name. Returns `None`
    /// for any other function.
    fn fields_for(name: &str) -> Option<[(usize, &'static str); 3]> {
        if name.eq_ignore_ascii_case("_pg_truetypid") {
            Some([(0, "atttypid"), (1, "typtype"), (1, "typbasetype")])
        } else if name.eq_ignore_ascii_case("_pg_truetypmod") {
            Some([(0, "atttypmod"), (1, "typtype"), (1, "typtypmod")])
        } else {
            None
        }
    }

    /// The qualifier of a `qualifier.*` function argument (e.g. `a` for `a.*`),
    /// or `None` if the argument isn't a qualified wildcard.
    fn wildcard_qualifier(arg: &FunctionArg) -> Option<String> {
        if let FunctionArg::Unnamed(FunctionArgExpr::QualifiedWildcard(obj)) = arg {
            obj.0.last().and_then(|p| match p {
                ObjectNamePart::Identifier(id) => Some(id.value.clone()),
                ObjectNamePart::Function(_) => None,
            })
        } else {
            None
        }
    }

    let mut stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .map_err(|e| DataFusionError::External(Box::new(e)))?;
    let _ = visit_statements_mut(&mut stmts, |stmt| {
        let () = visit_expressions_mut(stmt, |e| {
            if let Expr::Function(f) = e {
                let bare = f.name.0.last().and_then(|p| match p {
                    ObjectNamePart::Identifier(id) => Some(id.value.clone()),
                    ObjectNamePart::Function(_) => None,
                });
                if let Some(fields) = bare.as_deref().and_then(fields_for) {
                    if let FunctionArguments::List(list) = &mut f.args {
                        if list.args.len() == 2 {
                            if let (Some(att), Some(typ)) = (
                                wildcard_qualifier(&list.args[0]),
                                wildcard_qualifier(&list.args[1]),
                            ) {
                                let quals = [att, typ];
                                list.args = fields
                                    .iter()
                                    .map(|(src, col)| {
                                        FunctionArg::Unnamed(FunctionArgExpr::Expr(
                                            Expr::CompoundIdentifier(vec![
                                                Ident::new(quals[*src].clone()),
                                                Ident::new(*col),
                                            ]),
                                        ))
                                    })
                                    .collect();
                            }
                        }
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

/// Map casts to `regtype` (bare or `pg_catalog`-qualified) onto TEXT, since this
/// catalog stores type references by name and `DataFusion` has no `regtype`.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_regtype_cast(sql: &str) -> Result<String> {
    rewrite_custom_type_cast_target(sql, "regtype", &DataType::Text)
}

/// Normalize casts to `pg_catalog.char` by converting them to the
/// standard `CHAR` type understood by `DataFusion`.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_char_cast(sql: &str) -> Result<String> {
    rewrite_custom_type_cast_target(sql, "char", &DataType::Char(None))
}

/// Rename the `pg_available_extension_versions(...)` table-function call to the internal
/// name `available_extension_versions` it is registered under.
///
/// The catalog declares both a *view* `pg_available_extension_versions` and (to back that
/// view) a table function of the same name. `DataFusion` resolves a relation and a table
/// function of the same name ambiguously - the function shadows the view in FROM clauses -
/// so the function is registered as `available_extension_versions` and this rewrite points
/// the view body's call there, leaving the view to own its name. Only the call form (a
/// table factor WITH arguments) is renamed; a bare `FROM pg_available_extension_versions`
/// (querying the view itself) is left untouched.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_available_extension_versions_source(sql: &str) -> Result<String> {
    use sqlparser::ast::{Ident, ObjectName, ObjectNamePart, TableFactor, VisitMut, VisitorMut};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    /// Does `name` refer to `pg_available_extension_versions`, bare or
    /// `pg_catalog`-qualified?
    fn is_target(name: &ObjectName) -> bool {
        match name.0.as_slice() {
            [ObjectNamePart::Identifier(id)] => id
                .value
                .eq_ignore_ascii_case("pg_available_extension_versions"),
            [ObjectNamePart::Identifier(schema), ObjectNamePart::Identifier(id)] => {
                schema.value.eq_ignore_ascii_case("pg_catalog")
                    && id
                        .value
                        .eq_ignore_ascii_case("pg_available_extension_versions")
            }
            _ => false,
        }
    }

    /// Visitor that points every `pg_available_extension_versions(...)` table
    /// function call at the internal name the function is registered under.
    struct Renamer;
    impl VisitorMut for Renamer {
        type Break = ();

        /// Rename one FROM item when it calls the view's backing table function.
        fn pre_visit_table_factor(&mut self, tf: &mut TableFactor) -> ControlFlow<()> {
            if let TableFactor::Table {
                name,
                args: Some(_),
                ..
            } = tf
            {
                if is_target(name) {
                    *name = ObjectName(vec![ObjectNamePart::Identifier(Ident::new(
                        "available_extension_versions",
                    ))]);
                }
            }
            ControlFlow::Continue(())
        }
    }

    let mut stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .map_err(|e| DataFusionError::External(Box::new(e)))?;
    for stmt in &mut stmts {
        let _ = stmt.visit(&mut Renamer);
    }
    Ok(stmts
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("; "))
}

/// The mutable `ON` constraint of an inner/left equi-join, if the operator has one.
fn join_on_constraint(
    op: &mut sqlparser::ast::JoinOperator,
) -> Option<&mut sqlparser::ast::JoinConstraint> {
    use sqlparser::ast::JoinOperator;

    match op {
        JoinOperator::Left(c)
        | JoinOperator::LeftOuter(c)
        | JoinOperator::Inner(c)
        | JoinOperator::Join(c) => Some(c),
        _ => None,
    }
}

/// The name a single plain table is referred to by (its alias, else its bare name).
fn plain_table_qualifier(tf: &TableFactor) -> Option<String> {
    if let TableFactor::Table {
        name,
        alias,
        args: None,
        ..
    } = tf
    {
        if let Some(a) = alias {
            return Some(a.name.value.clone());
        }
        if let Some(ObjectNamePart::Identifier(id)) = name.0.last() {
            return Some(id.value.clone());
        }
    }
    None
}

/// The table qualifier of a two-part `qual.col` reference.
fn compound_reference_qualifier(e: &Expr) -> Option<&str> {
    match e {
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => Some(parts[0].value.as_str()),
        _ => None,
    }
}

/// True if `e` computes an aggregate (so grouping the rows feeding it is
/// meaningful), looking through casts, parentheses and arithmetic.
///
/// Deliberately narrower than [`contains_aggregate_call`]: this decides whether
/// a LATERAL subquery may be turned into a grouped join, so it only recognises
/// the aggregate spellings the catalog views actually join on.
fn expression_computes_aggregate(e: &Expr) -> bool {
    match e {
        Expr::Function(f) => {
            let name = f
                .name
                .0
                .last()
                .and_then(|p| p.as_ident())
                .map(|i| i.value.to_lowercase())
                .unwrap_or_default();
            ["sum", "count", "avg", "min", "max", "array_agg"].contains(&name.as_str())
        }
        Expr::Cast { expr, .. } | Expr::Nested(expr) | Expr::UnaryOp { expr, .. } => {
            expression_computes_aggregate(expr)
        }
        Expr::BinaryOp { left, right, .. } => {
            expression_computes_aggregate(left) || expression_computes_aggregate(right)
        }
        _ => false,
    }
}

/// True if the join operator is an inner/left join constrained by a literal `ON true`.
fn is_joined_on_literal_true(op: &sqlparser::ast::JoinOperator) -> bool {
    use sqlparser::ast::{JoinConstraint, JoinOperator};

    let is_true = |c: &JoinConstraint| {
        matches!(
            c,
            JoinConstraint::On(Expr::Value(ValueWithSpan {
                value: Value::Boolean(true),
                ..
            }))
        )
    };
    match op {
        JoinOperator::Left(c)
        | JoinOperator::LeftOuter(c)
        | JoinOperator::Inner(c)
        | JoinOperator::Join(c) => is_true(c),
        _ => false,
    }
}

/// Rewrite one join in place if it matches the correlated-LATERAL-aggregate shape,
/// reporting whether it did. See [`decorrelate_lateral_aggregate`] for the shape and
/// why the equivalence holds. `counter` names each decorrelated key uniquely.
fn decorrelate_lateral_aggregate_join(
    join: &mut sqlparser::ast::Join,
    counter: &mut usize,
) -> bool {
    use sqlparser::ast::{GroupByExpr, JoinConstraint};

    // Validate the shape under an immutable borrow, cloning out the two expressions
    // and the alias the mutation needs; bail (leaving the join untouched) otherwise.
    let (inner_key, outer_expr, derived_name) = {
        let TableFactor::Derived {
            lateral: true,
            subquery,
            alias: Some(derived_alias),
            ..
        } = &join.relation
        else {
            return false;
        };
        if !is_joined_on_literal_true(&join.join_operator) {
            return false;
        }
        let SetExpr::Select(sel) = subquery.body.as_ref() else {
            return false;
        };
        if subquery.with.is_some()
            || sel.from.len() != 1
            || !sel.from[0].joins.is_empty()
            || !matches!(sel.group_by, GroupByExpr::Expressions(ref e, _) if e.is_empty())
        {
            return false;
        }
        let Some(inner_qual) = plain_table_qualifier(&sel.from[0].relation) else {
            return false;
        };
        let Some(Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        }) = sel.selection.as_ref()
        else {
            return false;
        };
        let (inner_key, outer_expr) = match (
            compound_reference_qualifier(left) == Some(&inner_qual),
            compound_reference_qualifier(right) == Some(&inner_qual),
        ) {
            (true, false) => ((**left).clone(), (**right).clone()),
            (false, true) => ((**right).clone(), (**left).clone()),
            _ => return false,
        };
        let has_aggr = sel.projection.iter().any(|item| match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                expression_computes_aggregate(e)
            }
            _ => false,
        });
        if !has_aggr {
            return false;
        }
        (inner_key, outer_expr, derived_alias.name.value.clone())
    };

    let key_alias = format!("__decorr_key_{counter}");
    *counter += 1;

    // Apply: turn the LATERAL aggregate subquery into a grouped one and replace the
    // `ON true` with `<alias>.<key_alias> = <outer>`.
    if let TableFactor::Derived {
        lateral, subquery, ..
    } = &mut join.relation
    {
        *lateral = false;
        if let SetExpr::Select(sel) = subquery.body.as_mut() {
            sel.projection.insert(
                0,
                SelectItem::ExprWithAlias {
                    expr: inner_key.clone(),
                    alias: Ident::new(&key_alias),
                },
            );
            sel.group_by = GroupByExpr::Expressions(vec![inner_key], Vec::new());
            sel.selection = None;
        }
    }
    let on = Expr::BinaryOp {
        left: Box::new(Expr::CompoundIdentifier(vec![
            Ident::new(derived_name),
            Ident::new(key_alias),
        ])),
        op: BinaryOperator::Eq,
        right: Box::new(outer_expr),
    };
    if let Some(c) = join_on_constraint(&mut join.join_operator) {
        *c = JoinConstraint::On(on);
    }
    true
}

/// Decorrelate a correlated `LEFT JOIN LATERAL (SELECT <aggregates> FROM <tbl> WHERE
/// <tbl>.<key> = <outer>) <alias> ON true` into a grouped equi-join:
/// `LEFT JOIN (SELECT <tbl>.<key> AS <k>, <aggregates> FROM <tbl> GROUP BY <tbl>.<key>)
/// <alias> ON <alias>.<k> = <outer>`.
///
/// `DataFusion` has no physical plan for the correlated `OuterReferenceColumn` such a
/// LATERAL aggregate leaves in the logical plan (it does not decorrelate aggregate
/// LATERAL subqueries itself), so `pg_statio_all_tables` - which sums per-table index
/// block counts this way - cannot otherwise be served as a view. The grouped equi-join is
/// an exact equivalent the planner handles. Only the precise shape above is rewritten: a
/// LATERAL derived table joined `ON true` whose subquery is a single aggregating SELECT
/// over one table with one correlated equality predicate. Anything else is left untouched.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn decorrelate_lateral_aggregate(sql: &str) -> Result<String> {
    use sqlparser::ast::{Query, Select, SetExpr, TableFactor};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    /// Decorrelate the LATERAL aggregates in a FROM item that is a subquery.
    fn walk_table_factor(tf: &mut TableFactor, counter: &mut usize) {
        if let TableFactor::Derived { subquery, .. } = tf {
            walk_query(subquery, counter);
        }
    }

    /// Decorrelate the LATERAL aggregates joined into one `SELECT`, and those
    /// nested in the subqueries it selects from.
    fn walk_select(sel: &mut Select, counter: &mut usize) {
        for twj in &mut sel.from {
            walk_table_factor(&mut twj.relation, counter);
            for join in &mut twj.joins {
                walk_table_factor(&mut join.relation, counter);
                decorrelate_lateral_aggregate_join(join, counter);
            }
        }
    }

    /// Decorrelate the LATERAL aggregates of one query and of its CTEs.
    fn walk_query(q: &mut Query, counter: &mut usize) {
        if let Some(with) = &mut q.with {
            for cte in &mut with.cte_tables {
                walk_query(&mut cte.query, counter);
            }
        }
        walk_set_expr(&mut q.body, counter);
    }

    /// Decorrelate the LATERAL aggregates in one query body, both sides of a
    /// set operation included.
    fn walk_set_expr(se: &mut SetExpr, counter: &mut usize) {
        match se {
            SetExpr::Select(sel) => walk_select(sel, counter),
            SetExpr::Query(q) => walk_query(q, counter),
            SetExpr::SetOperation { left, right, .. } => {
                walk_set_expr(left, counter);
                walk_set_expr(right, counter);
            }
            _ => {}
        }
    }

    let mut stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .map_err(|e| DataFusionError::External(Box::new(e)))?;
    let mut counter = 0usize;
    for stmt in &mut stmts {
        if let sqlparser::ast::Statement::Query(q) = stmt {
            walk_query(q, &mut counter);
        }
    }
    Ok(stmts
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("; "))
}

/// Remove the `pg_catalog.` prefix from known table functions such as
/// `pg_get_keywords` so unqualified calls work inside user queries.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_schema_qualified_udtfs(sql: &str) -> Result<String> {
    use sqlparser::ast::{
        visit_expressions_mut, visit_relations_mut, visit_statements_mut, Expr, Function,
        ObjectName, ObjectNamePart,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    /// Drop the `pg_catalog.` prefix from one of the known table functions,
    /// reporting whether the name was changed.
    fn strip_name(name: &mut ObjectName) -> bool {
        match name.0.as_slice() {
            [ObjectNamePart::Identifier(schema), ObjectNamePart::Identifier(func)]
                if schema.value.eq_ignore_ascii_case("pg_catalog")
                    && [
                        "pg_get_keywords",
                        "pg_postmaster_start_time",
                        "generate_series",
                    ]
                    .iter()
                    .any(|f| func.value.eq_ignore_ascii_case(f)) =>
            {
                let ident = name.0.pop().unwrap();
                name.0.clear();
                name.0.push(ident);
                true
            }
            _ => false,
        }
    }

    let dialect = PostgreSqlDialect {};
    let mut stmts =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;
    let mut rewritten = false;

    let _ = visit_statements_mut(&mut stmts, |stmt| {
        visit_expressions_mut(stmt, |e| {
            if let Expr::Function(Function { name, .. }) = e {
                if strip_name(name) {
                    rewritten = true;
                }
            }
            ControlFlow::<()>::Continue(())
        })?;
        visit_relations_mut(stmt, |obj| {
            if strip_name(obj) {
                rewritten = true;
            }
            ControlFlow::<()>::Continue(())
        })?;
        ControlFlow::Continue(())
    });

    if rewritten {
        Ok(stmts
            .into_iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join("; "))
    } else {
        Ok(sql.to_owned())
    }
}

/// Convert casts to `xid` into plain BIGINT casts since transaction IDs
/// are represented as 64 bit integers in the catalog snapshots.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_xid_cast(sql: &str) -> Result<String> {
    rewrite_custom_type_cast_target(sql, "xid", &DataType::BigInt(None))
}

/// Map casts to the pseudo-type `name` onto plain TEXT since the
/// planner does not know about `PostgreSQL`'s internal name type.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_name_cast(sql: &str) -> Result<String> {
    rewrite_custom_type_cast_target(sql, "name", &DataType::Text)
}

/// Build the call expression `name(argument)`.
fn function_call(name: &str, argument: Expr) -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new(name))]),
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            clauses: vec![],
            args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(argument))],
        }),
        over: None,
        filter: None,
        within_group: vec![],
        null_treatment: None,
        parameters: FunctionArguments::None,
        uses_odbc_syntax: false,
    })
}

/// Convert casts to the OID type into BIGINT since our catalog
/// represents object identifiers as plain integers.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_oid_cast(sql: &str) -> Result<String> {
    rewrite_each_expression(sql, |e| {
        let Expr::Cast {
            expr, data_type, ..
        } = e
        else {
            return;
        };
        let DataType::Custom(obj, _) = data_type else {
            return;
        };
        if !object_name_matches(obj, "oid") {
            return;
        }

        // A numeric or placeholder operand is a plain integer OID, so cast it to
        // BIGINT; anything else (e.g. a relation name) goes through a function
        // that resolves a name to its OID - `pg_proc_oid()` for the columns that
        // hold a function name, `oid()` (a `pg_class` lookup) for the rest.
        let operand_is_integer = matches!(
            expr.as_ref(),
            Expr::Value(
                ValueWithSpan {
                    value: Value::Number(_, _),
                    ..
                } | ValueWithSpan {
                    value: Value::Placeholder(_),
                    ..
                }
            )
        );

        if operand_is_integer {
            *e = Expr::Cast {
                kind: CastKind::DoubleColon,
                expr: expr.clone(),
                data_type: DataType::BigInt(None),
                array: false,
                format: None,
            };
        } else if is_regproc_column(expr) {
            *e = function_call(PG_PROC_OID_FUNCTION, *expr.clone());
        } else {
            *e = function_call("oid", *expr.clone());
        }
    })
}

/// Name of the UDF that resolves a function name to its `pg_catalog.pg_proc` OID.
const PG_PROC_OID_FUNCTION: &str = "pg_proc_oid";

/// The catalog columns `PostgreSQL` declares as `regproc`.
///
/// This catalog stores them the way `PostgreSQL` *renders* a `regproc` - as the
/// function's name, so `pg_type.typreceive` reads `boolrecv` rather than 2436 - which
/// means a client comparing one of them against an OID (`pg_proc.oid = a.typreceive`)
/// is comparing text against an integer. [`resolve_regproc_columns_to_oids_in_comparisons`]
/// and [`rewrite_oid_cast`] send those columns through `pg_proc_oid()` to get the
/// number back.
///
/// Mirrors every column marked `pg_types: regproc` in `pg_catalog_data/pg_schema`;
/// `tests/test_regproc_columns.py` fails if the two drift apart.
pub const REGPROC_COLUMN_NAMES: &[&str] = &[
    "aggcombinefn",
    "aggdeserialfn",
    "aggfinalfn",
    "aggfnoid",
    "aggmfinalfn",
    "aggminvtransfn",
    "aggmtransfn",
    "aggserialfn",
    "aggtransfn",
    "amhandler",
    "amproc",
    "conproc",
    "oprcode",
    "oprjoin",
    "oprrest",
    "prosupport",
    "prsend",
    "prsheadline",
    "prslextype",
    "prsstart",
    "prstoken",
    "rngcanonical",
    "rngsubdiff",
    "tmplinit",
    "tmpllexize",
    "trffromsql",
    "trftosql",
    "typanalyze",
    "typinput",
    "typmodin",
    "typmodout",
    "typoutput",
    "typreceive",
    "typsend",
    "typsubscript",
];

/// Returns `true` when `expr` reads one of the [`REGPROC_COLUMN_NAMES`], either bare
/// (`typreceive`) or qualified (`a.typreceive`, `pg_catalog.pg_type.typreceive`).
fn is_regproc_column(expr: &Expr) -> bool {
    let column = match expr {
        Expr::Identifier(ident) => ident,
        Expr::CompoundIdentifier(parts) => match parts.last() {
            Some(ident) => ident,
            None => return false,
        },
        _ => return false,
    };
    REGPROC_COLUMN_NAMES
        .iter()
        .any(|known| known.eq_ignore_ascii_case(&column.value))
}

/// Returns `true` when `expr` is a text literal, including one wrapped in a cast such
/// as `'array_in'::regproc`.
fn is_text_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(_) | Value::DoubleQuotedString(_),
            ..
        }) => true,
        Expr::Cast { expr, .. } => is_text_literal(expr),
        _ => false,
    }
}

/// Compare `regproc` columns as OIDs by sending them through `pg_proc_oid()`.
///
/// A client joining `pg_proc.oid = a.typreceive` means "the function this type receives
/// with"; because this catalog holds the function's name in that column, the comparison
/// would otherwise ask `DataFusion` to read `'boolrecv'` as an integer and fail the whole
/// query. A function name written as a literal (`typinput = 'pg_catalog.array_in'`) is
/// resolved the same way, so that it matches whether or not the two sides agree on
/// spelling the schema out - which is how `PostgreSQL` compares a `regproc` against text.
/// Comparing two `regproc` columns needs no lookup: their names already line up.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn resolve_regproc_columns_to_oids_in_comparisons(sql: &str) -> Result<String> {
    rewrite_each_expression(sql, |e| {
        let Expr::BinaryOp { left, op, right } = e else {
            return;
        };
        if !matches!(op, BinaryOperator::Eq | BinaryOperator::NotEq) {
            return;
        }
        let (regproc_side, opposite_side) = if is_regproc_column(left) {
            (left, right)
        } else if is_regproc_column(right) {
            (right, left)
        } else {
            return;
        };
        if is_regproc_column(opposite_side) {
            return;
        }
        if is_text_literal(opposite_side) {
            **opposite_side = function_call(PG_PROC_OID_FUNCTION, *opposite_side.clone());
        }
        **regproc_side = function_call(PG_PROC_OID_FUNCTION, *regproc_side.clone());
    })
}

/// Catalog columns of `PostgreSQL` type `boolean`. Clients (and the ODBC driver's
/// `SQLPrimaryKeys` query) compare these to a character boolean literal such as
/// `indisprimary = 't'`; `DataFusion` will not coerce `Boolean = Utf8`, so
/// [`rewrite_boolean_column_char_comparisons`] turns the literal into a boolean.
///
/// Only unambiguously boolean columns are listed. Character columns that clients
/// also compare to single-letter literals (`relkind = 'r'`, `contype = 't'`,
/// `relpersistence = 't'`) are deliberately excluded.
pub const BOOLEAN_COLUMN_NAMES: &[&str] = &[
    // pg_index
    "indisunique",
    "indisprimary",
    "indisexclusion",
    "indimmediate",
    "indisclustered",
    "indisvalid",
    "indcheckxmin",
    "indisready",
    "indislive",
    "indisreplident",
    "indnullsnotdistinct",
    // pg_class
    "relhasindex",
    "relhasrules",
    "relhastriggers",
    "relhassubclass",
    "relrowsecurity",
    "relforcerowsecurity",
    "relispopulated",
    "relisshared",
    "relispartition",
    // pg_attribute
    "attnotnull",
    "atthasdef",
    "atthasmissing",
    "attisdropped",
    "attislocal",
    "attbyval",
    // pg_type
    "typbyval",
    "typisdefined",
    "typnotnull",
    "typispreferred",
    // pg_constraint
    "condeferrable",
    "condeferred",
    "convalidated",
    "conislocal",
    "connoinherit",
    // pg_proc
    "proisstrict",
    "proretset",
    "prosecdef",
    "proleakproof",
    // pg_database
    "datistemplate",
    "datallowconn",
    // pg_authid / pg_roles
    "rolsuper",
    "rolinherit",
    "rolcreaterole",
    "rolcreatedb",
    "rolcanlogin",
    "rolreplication",
    "rolbypassrls",
];

/// Return true when `expr` names one of [`BOOLEAN_COLUMN_NAMES`] (bare or
/// qualified, e.g. `i.indisprimary`).
fn is_boolean_column(expr: &Expr) -> bool {
    let column = match expr {
        Expr::Identifier(ident) => ident,
        Expr::CompoundIdentifier(parts) => match parts.last() {
            Some(ident) => ident,
            None => return false,
        },
        _ => return false,
    };
    BOOLEAN_COLUMN_NAMES
        .iter()
        .any(|known| known.eq_ignore_ascii_case(&column.value))
}

/// Interpret a `PostgreSQL` character boolean literal.
fn char_boolean_literal(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "t" | "true" | "y" | "yes" | "1" | "on" => Some(true),
        "f" | "false" | "n" | "no" | "0" | "off" => Some(false),
        _ => None,
    }
}

/// Rewrite `<boolean-column> = '<char-bool>'` (and `<>`) so the string literal
/// becomes a boolean literal, e.g. `indisprimary = 't'` -> `indisprimary = true`.
/// `PostgreSQL` coerces the character form; `DataFusion` rejects `Boolean = Utf8`.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_boolean_column_char_comparisons(sql: &str) -> Result<String> {
    rewrite_each_expression(sql, |e| {
        let Expr::BinaryOp { left, op, right } = e else {
            return;
        };
        if !matches!(op, BinaryOperator::Eq | BinaryOperator::NotEq) {
            return;
        }
        let literal_side = if is_boolean_column(left) {
            right
        } else if is_boolean_column(right) {
            left
        } else {
            return;
        };
        let replacement = match literal_side.as_ref() {
            Expr::Value(ValueWithSpan {
                value: Value::SingleQuotedString(s) | Value::DoubleQuotedString(s),
                ..
            }) => char_boolean_literal(s),
            _ => None,
        };
        if let Some(boolean) = replacement {
            **literal_side = Expr::Value(Value::Boolean(boolean).into());
        }
    })
}

/// Replace casts to regoper with NULL. Queries sometimes cast the
/// `conexclop` column (stored as `_text`) to `regoper` and then to
/// another type like TEXT. Since the column is always NULL we can
/// short-circuit this pattern by returning NULL directly.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_regoper_cast(sql: &str) -> Result<String> {
    rewrite_each_expression(sql, |expr| {
        if let Expr::Cast {
            data_type: DataType::Custom(obj, _),
            ..
        } = expr
        {
            if object_name_matches(obj, "regoper") {
                *expr = Expr::Value(ValueWithSpan {
                    value: Value::Null,
                    span: Span::empty(),
                });
            }
        }
    })
}

/// Replace casts to regoperator with TEXT.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_regoperator_cast(sql: &str) -> Result<String> {
    rewrite_custom_type_cast_target(sql, "regoperator", &DataType::Text)
}

/// Replace casts to regprocedure with TEXT.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_regprocedure_cast(sql: &str) -> Result<String> {
    rewrite_custom_type_cast_target(sql, "regprocedure", &DataType::Text)
}

/// Replace casts to regproc with TEXT.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_regproc_cast(sql: &str) -> Result<String> {
    rewrite_custom_type_cast_target(sql, "regproc", &DataType::Text)
}

/// The name a SELECT list item is output under, when it has one.
///
/// A bare column keeps its own name (`pg_type.oid` is output as `oid`), an aliased
/// expression takes the alias, and anything else - a function call, a CASE - has no name
/// a client could sort by.
fn output_column_name(item: &SelectItem) -> Option<String> {
    match item {
        SelectItem::ExprWithAlias { alias, .. } => Some(alias.value.clone()),
        SelectItem::UnnamedExpr(Expr::Identifier(ident)) => Some(ident.value.clone()),
        SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts)) => {
            parts.last().map(|ident| ident.value.clone())
        }
        _ => None,
    }
}

/// Sort by the SELECT list column an `ORDER BY` names, by replacing the name with its
/// position in that list.
///
/// `PostgreSQL` resolves a bare name in ORDER BY against the query's OUTPUT columns before
/// its input columns, so `SELECT pg_type.oid ... JOIN pg_enum ... ORDER BY oid` sorts by
/// the selected `pg_type.oid`. `DataFusion` resolves it against the input instead and fails
/// the query with "column 'oid' is ambiguous" whenever the FROM clause carries the name
/// more than once - which every catalog join does, since every catalog table has an `oid`.
/// A position means the same thing to both.
///
/// Only a bare name is resolved this way: a qualified one (`pg_enum.oid`) always means the
/// input column in `PostgreSQL` too. Queries whose SELECT list holds a wildcard are left
/// alone, because the position of anything after `*` is not known before expansion.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn resolve_order_by_names_to_output_positions(sql: &str) -> Result<String> {
    use sqlparser::ast::{OrderByKind, Query, SetExpr, VisitMut, VisitorMut};

    /// Visitor that replaces each `ORDER BY <name>` that names an output column
    /// with that column's position.
    struct SortByPosition;
    impl VisitorMut for SortByPosition {
        type Break = ();

        /// Resolve the `ORDER BY` names of one query against its SELECT list.
        fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<()> {
            let Some(order_by) = query.order_by.as_mut() else {
                return ControlFlow::Continue(());
            };
            let OrderByKind::Expressions(order_by_exprs) = &mut order_by.kind else {
                return ControlFlow::Continue(());
            };
            let SetExpr::Select(select) = query.body.as_ref() else {
                return ControlFlow::Continue(());
            };
            if select.projection.iter().any(|item| {
                matches!(
                    item,
                    SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..)
                )
            }) {
                return ControlFlow::Continue(());
            }

            let output_names: Vec<Option<String>> =
                select.projection.iter().map(output_column_name).collect();

            for order_by_expr in order_by_exprs {
                let Expr::Identifier(ident) = &order_by_expr.expr else {
                    continue;
                };
                let position = output_names.iter().position(|name| {
                    name.as_deref()
                        .is_some_and(|name| name.eq_ignore_ascii_case(&ident.value))
                });
                if let Some(position) = position {
                    order_by_expr.expr = Expr::Value(ValueWithSpan {
                        value: Value::Number((position + 1).to_string(), false),
                        span: Span::empty(),
                    });
                }
            }
            ControlFlow::Continue(())
        }
    }

    let dialect = PostgreSqlDialect {};
    let mut statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;
    for statement in &mut statements {
        let _ = statement.visit(&mut SortByPosition);
    }
    Ok(statements
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("; "))
}

/// Replace the `available_updates` sub-query in `pg_extension` queries with NULL.
/// `IntelliJ` issues a correlated ARRAY sub-query over `available_versions`
/// which our planner cannot resolve. Returning NULL keeps the column shape
/// without failing the query.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_available_updates(sql: &str) -> Result<String> {
    use sqlparser::ast::{
        visit_expressions_mut, visit_statements_mut, BinaryOperator, Expr, Function,
        FunctionArguments, SelectItem, SetExpr, TableFactor, Value, ValueWithSpan,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    let dialect = PostgreSqlDialect {};
    let mut statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;

    let mut rewritten = false;

    let _ = visit_statements_mut(&mut statements, |stmt| {
        let inner = visit_expressions_mut(stmt, |expr| {
            if let Expr::Function(Function { name, args, .. }) = expr {
                let base = name
                    .0
                    .last()
                    .and_then(|p| p.as_ident())
                    .map(|id| id.value.to_lowercase())
                    .unwrap_or_default();

                if base == "array" {
                    if let FunctionArguments::Subquery(subq) = args {
                        if let SetExpr::Select(sel) = subq.body.as_ref() {
                            let from_ok = sel.from.len() == 1
                                && matches!(sel.from[0].relation, TableFactor::UNNEST { .. });
                            let proj_ok = sel.projection.len() == 1
                                && matches!(
                                    sel.projection[0],
                                    SelectItem::UnnamedExpr(Expr::Identifier(ref id)) if id.value.eq_ignore_ascii_case("unnest")
                                );
                            let cond_ok = match &sel.selection {
                                Some(Expr::BinaryOp {
                                    left,
                                    op: BinaryOperator::Gt,
                                    right,
                                }) => {
                                    matches!(left.as_ref(), Expr::Identifier(ref id) if id.value.eq_ignore_ascii_case("unnest"))
                                        && matches!(right.as_ref(), Expr::Identifier(ref id) if id.value.eq_ignore_ascii_case("extversion"))
                                }
                                _ => false,
                            };

                            if from_ok && proj_ok && cond_ok {
                                *expr = Expr::Value(ValueWithSpan {
                                    value: Value::Null,
                                    span: Span::empty(),
                                });
                                rewritten = true;
                                return ControlFlow::<DataFusionError, ()>::Continue(());
                            }
                        }
                    }
                }
            }

            ControlFlow::Continue(())
        });

        match inner {
            ControlFlow::Break(e) => ControlFlow::Break(e),
            ControlFlow::Continue(()) => ControlFlow::<DataFusionError, ()>::Continue(()),
        }
    });

    if rewritten {
        Ok(statements
            .into_iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join("; "))
    } else {
        Ok(sql.to_owned())
    }
}

/// Drop `COLLATE pg_catalog.default` clauses since `DataFusion` has no
/// notion of collations and the default adds no semantics.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn strip_default_collate(sql: &str) -> Result<String> {
    use sqlparser::ast::{
        visit_expressions_mut, visit_statements_mut, Expr, ObjectName, ObjectNamePart,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    /// Is `coll` the collation `pg_catalog.default`?
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
    let mut statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;

    let _ = visit_statements_mut(&mut statements, |stmt| {
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

    force_explicit_aliases(&mut statements);
    Ok(statements
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("; "))
}
/// Spell an `AT TIME ZONE 'utc'` literal as `'UTC'`, whatever case the client
/// wrote it in, since `DataFusion` matches time zone names exactly.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_time_zone_utc(sql: &str) -> Result<String> {
    use sqlparser::ast::{visit_expressions_mut, visit_statements_mut, Expr, Value, ValueWithSpan};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    let dialect = PostgreSqlDialect {};
    let mut stmts =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;
    let mut rewritten = false;

    let _ = visit_statements_mut(&mut stmts, |stmt| {
        visit_expressions_mut(stmt, |e| {
            if let Expr::AtTimeZone { time_zone, .. } = e {
                if let Expr::Value(ValueWithSpan {
                    value: Value::SingleQuotedString(ref mut s),
                    ..
                }) = time_zone.as_mut()
                {
                    if s.eq_ignore_ascii_case("utc") && s != "UTC" {
                        *s = "UTC".into();
                        rewritten = true;
                    }
                }
            }
            ControlFlow::<()>::Continue(())
        })?;
        ControlFlow::Continue(())
    });

    if rewritten {
        Ok(stmts
            .into_iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join("; "))
    } else {
        Ok(sql.to_owned())
    }
}

/// The single argument of an `array(...)` call, taken from either the list form
/// `array(x)` or the sub-query form `array(SELECT ...)`.
///
/// # Errors
///
/// Returns a plan error when the call carries more than one argument, when its
/// argument is a named argument or a wildcard rather than an expression, or when
/// it carries no argument list at all - none of which `ARRAY()` can mean.
fn single_array_call_argument(args: &FunctionArguments) -> Result<Expr> {
    match args {
        FunctionArguments::List(FunctionArgumentList { args, .. }) => {
            if args.len() != 1 {
                return Err(DataFusionError::Plan(
                    "ARRAY() must have exactly one argument".into(),
                ));
            }
            match &args[0] {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Ok(e.clone()),
                _ => Err(DataFusionError::Plan(
                    "ARRAY() argument must be an expression".into(),
                )),
            }
        }
        FunctionArguments::Subquery(q) => Ok(Expr::Subquery(Box::new((**q).clone()))),
        FunctionArguments::None => Err(DataFusionError::Plan(
            "ARRAY() with unsupported argument form".into(),
        )),
    }
}

/// The expression an `array(<sub-query>)` collapses to when the sub-query only
/// round-trips a value through `UNNEST`, or `None` for every other shape.
///
/// `PostgreSQL` allows `ARRAY(SELECT unnest FROM UNNEST(col))` to effectively
/// return the original array. The generic rewrite would turn it into
/// `pg_catalog.pg_get_array((SELECT unnest FROM UNNEST(col)))`, which later fails
/// when the scalar sub-query is converted into a CTE because it references the
/// outer table; so this yields `col` itself. The sibling shape that projects NULL
/// instead of `unnest` yields `CAST(NULL AS TEXT)`.
fn unnest_roundtrip_replacement(arg_expr: &Expr) -> Option<Expr> {
    let Expr::Subquery(subquery) = arg_expr else {
        return None;
    };
    let SetExpr::Select(inner_sel) = subquery.body.as_ref() else {
        return None;
    };
    let from_is_unnest = inner_sel.from.len() == 1
        && matches!(inner_sel.from[0].relation, TableFactor::UNNEST { .. });
    if !from_is_unnest || inner_sel.selection.is_some() {
        return None;
    }

    let projects_unnest = inner_sel.projection.len() == 1
        && matches!(
            inner_sel.projection[0],
            SelectItem::UnnamedExpr(Expr::Identifier(ref id))
            if id.value.to_lowercase() == "unnest"
        );
    if projects_unnest {
        if let TableFactor::UNNEST {
            ref array_exprs, ..
        } = inner_sel.from[0].relation
        {
            if array_exprs.len() == 1 {
                return Some(array_exprs[0].clone());
            }
        }
        return None;
    }

    let projects_null = inner_sel.projection.len() == 1
        && match &inner_sel.projection[0] {
            SelectItem::UnnamedExpr(Expr::Value(ValueWithSpan {
                value: Value::Null, ..
            })) => true,
            SelectItem::UnnamedExpr(Expr::Cast { expr, .. }) => matches!(
                **expr,
                Expr::Value(ValueWithSpan {
                    value: Value::Null,
                    ..
                })
            ),
            _ => false,
        };
    if projects_null {
        return Some(Expr::Cast {
            kind: CastKind::Cast,
            expr: Box::new(Expr::Value(ValueWithSpan {
                value: Value::Null,
                span: Span::empty(),
            })),
            data_type: DataType::Text,
            array: false,
            format: None,
        });
    }
    None
}

/// Re-write  ARRAY( <sub-query> )
///        ->  `pg_catalog.pg_get_array`( ( <sub-query> ) )
///
/// - no regexes - uses `sqlparser` AST
/// - only the `array( ... )` form with ONE argument is accepted
/// - any other shape causes an explicit `Err(DataFusionError::Plan(..))`
/// - **if nothing matches we just pass the SQL back untouched**
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`, or if an `array(...)`
/// call does not have exactly one expression argument.
pub fn rewrite_array_subquery(sql: &str) -> Result<String> {
    use sqlparser::ast::{
        visit_expressions_mut, visit_statements_mut, Expr, Function, FunctionArg, FunctionArgExpr,
        FunctionArgumentList, FunctionArguments, Ident, ObjectName, ObjectNamePart,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    let dialect = PostgreSqlDialect {};
    let mut stmts =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;

    let mut rewritten_any = false;

    /* --------------------------------------------------------- */
    let flow: ControlFlow<DataFusionError, ()> = visit_statements_mut(&mut stmts, |stmt| {
        let inner = visit_expressions_mut(stmt, |expr| {
            /* -- 1  bail out on ARRAY[...] literals --------------- */
            if let Expr::Array(_) = expr {
                return ControlFlow::Continue(());
            }

            /* -- 2  handle ARRAY( ... ) rewrites ------------------- */
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
                    let arg_expr: Expr = match single_array_call_argument(&func.args) {
                        Ok(arg) => arg,
                        Err(err) => return ControlFlow::Break(err),
                    };

                    // Collapse the UNNEST round-trip shapes before the generic
                    // rewrite, which cannot be planned for them.
                    if let Some(replacement) = unnest_roundtrip_replacement(&arg_expr) {
                        *expr = replacement;
                        rewritten_any = true;
                        return ControlFlow::Continue(());
                    }

                    /* add parentheses only when necessary */
                    let wrapped = match &arg_expr {
                        Expr::Subquery(_) | Expr::Nested(_) => arg_expr.clone(),
                        _ => Expr::Nested(Box::new(arg_expr.clone())),
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
                            args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(wrapped))],
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
            ControlFlow::Break(e) => ControlFlow::Break(e),
            ControlFlow::Continue(()) => ControlFlow::Continue(()),
        }
    });

    /* propagate any error triggered above */
    if let ControlFlow::Break(err) = flow {
        return Err(err);
    }

    /* nothing matched - just echo input back verbatim */
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
/// (`'{1,2,3}'`, `'{"a","b"}'`, ...) into an `Expr::Array`, which
/// `sqlparser` renders as `ARRAY[...]`.
///
///  * pure-AST rewrite - no regexes
///  * if *nothing* matches we pass SQL back unchanged
///  * malformed literals raise `DataFusionError::Plan`
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn rewrite_brace_array_literal(sql: &str) -> Result<String> {
    use sqlparser::ast::{visit_expressions_mut, visit_statements_mut, Expr, Value, ValueWithSpan};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    let dialect = PostgreSqlDialect {};
    let mut stmts =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;

    let mut rewritten_any = false;

    let flow: ControlFlow<DataFusionError, ()> = visit_statements_mut(&mut stmts, |stmt| {
        let inner = visit_expressions_mut(stmt, |expr| {
            if let Expr::Value(ValueWithSpan {
                value: Value::SingleQuotedString(s),
                ..
            }) = expr
            {
                if s.starts_with('{') && s.ends_with('}') {
                    let inside = &s[1..s.len() - 1]; // strip the braces

                    // split respecting the simple {a,b,c} grammar
                    // (no escape handling - good enough for catalogue OIDs
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
                        named: false, // <- `false` for the normal ARRAY[...] form
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

    force_explicit_aliases(&mut stmts);
    Ok(stmts
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("; "))
}

/// Replace tuple equality `(a, b) = (c, d)` with `a = c AND b = d`.
///
/// `DataFusion` does not support tuple comparisons, so we decompose them
/// into a conjunction of element comparisons. Only equality is handled;
/// all other expressions are left untouched.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
///
/// # Panics
///
/// The first element pair is unwrapped out of the zipped iterator, which is
/// sound because the decomposition only runs for tuples of equal, non-zero
/// length.
pub fn rewrite_tuple_equality(sql: &str) -> Result<String> {
    use sqlparser::ast::{visit_expressions_mut, visit_statements_mut, BinaryOperator, Expr};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    let dialect = PostgreSqlDialect {};
    let mut stmts =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;

    let _ = visit_statements_mut(&mut stmts, |stmt| {
        visit_expressions_mut(stmt, |expr| {
            if let Expr::BinaryOp { left, op, right } = expr {
                if matches!(op, BinaryOperator::Eq)
                    && matches!(left.as_ref(), Expr::Tuple(_))
                    && matches!(right.as_ref(), Expr::Tuple(_))
                {
                    let l_elems = match left.as_ref() {
                        Expr::Tuple(list) => list.clone(),
                        _ => unreachable!(),
                    };
                    let r_elems = match right.as_ref() {
                        Expr::Tuple(list) => list.clone(),
                        _ => unreachable!(),
                    };

                    if l_elems.len() == r_elems.len() && !l_elems.is_empty() {
                        let mut pairs = l_elems.into_iter().zip(r_elems);
                        let (l_first, r_first) = pairs.next().unwrap();
                        let mut new_expr = Expr::BinaryOp {
                            left: Box::new(l_first),
                            op: BinaryOperator::Eq,
                            right: Box::new(r_first),
                        };

                        for (l, r) in pairs {
                            let pair_expr = Expr::BinaryOp {
                                left: Box::new(l),
                                op: BinaryOperator::Eq,
                                right: Box::new(r),
                            };
                            new_expr = Expr::BinaryOp {
                                left: Box::new(new_expr),
                                op: BinaryOperator::And,
                                right: Box::new(pair_expr),
                            };
                        }

                        *expr = new_expr;
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

/// Ensure tables referenced inside subqueries are schema qualified and
/// given aliases so the planner can resolve them unambiguously.
///
/// # Errors
///
/// Returns an error if `sql` does not parse as `PostgreSQL`.
pub fn alias_subquery_tables(sql: &str) -> Result<String> {
    use sqlparser::ast::{
        visit_expressions_mut, visit_statements_mut, Expr, Ident, Query, TableAlias, TableFactor,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::ops::ControlFlow;

    /// Schema-qualify and alias the tables one subquery selects from, then point
    /// the subquery's own column references at the aliases.
    fn alias_tables(query: &mut Query, counter: &mut usize) {
        use sqlparser::ast::{SetExpr, TableWithJoins};

        // (original bare table name -> synthetic alias) for tables we just aliased.
        let mut table_alias_pairs: Vec<(String, String)> = Vec::new();
        if let SetExpr::Select(select) = query.body.as_mut() {
            for TableWithJoins { relation, joins } in &mut select.from {
                alias_table_factor(relation, counter, &mut table_alias_pairs);
                for j in joins {
                    alias_table_factor(&mut j.relation, counter, &mut table_alias_pairs);
                }
            }
        }

        // Re-qualify column refs that used the original table name to the new
        // alias (e.g. `pg_database.datname` -> `subq0_t.datname`). References to
        // OTHER tables (e.g. a correlated outer `rel.oid`) are left untouched.
        if !table_alias_pairs.is_empty() {
            let _ = visit_expressions_mut(query, |e| {
                if let Expr::CompoundIdentifier(parts) = e {
                    if parts.len() == 2 {
                        if let Some((_, alias)) = table_alias_pairs
                            .iter()
                            .find(|(orig, _)| orig.eq_ignore_ascii_case(&parts[0].value))
                        {
                            parts[0] = Ident::new(alias.clone());
                        }
                    }
                }
                ControlFlow::<()>::Continue(())
            });
        }
    }

    /// Qualify one FROM item with `pg_catalog` and give it a synthetic alias when
    /// it has none, recording the (original name -> alias) pair so the body's
    /// references to that name can follow.
    fn alias_table_factor(
        tf: &mut TableFactor,
        counter: &mut usize,
        table_alias_pairs: &mut Vec<(String, String)>,
    ) {
        if let TableFactor::Table { name, alias, .. } = tf {
            // The bare table name the subquery body refers to (last name part).
            let bare = name.0.last().and_then(|p| match p {
                ObjectNamePart::Identifier(i) => Some(i.value.clone()),
                ObjectNamePart::Function(_) => None,
            });
            if name.0.len() == 1 {
                name.0
                    .insert(0, ObjectNamePart::Identifier(Ident::new("pg_catalog")));
            }
            if alias.is_none() {
                let new_alias = format!("subq{counter}_t");
                if let Some(bare) = bare {
                    table_alias_pairs.push((bare, new_alias.clone()));
                }
                *alias = Some(TableAlias {
                    explicit: true,
                    name: Ident::new(new_alias),
                    columns: vec![],
                    at: None,
                });
                *counter += 1;
            }
        }
    }

    let dialect = PostgreSqlDialect {};
    let mut stmts =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;

    let mut counter = 0usize;
    let _ = visit_statements_mut(&mut stmts, |stmt| {
        visit_expressions_mut(stmt, |expr| {
            // Qualify/alias tables inside scalar subqueries AND `EXISTS (...)` and
            // `IN (...)` subqueries - the latter become scalar subqueries via the
            // later EXISTS->count rewrite, but their tables must be qualified here.
            match expr {
                Expr::Subquery(subquery)
                | Expr::Exists { subquery, .. }
                | Expr::InSubquery { subquery, .. } => alias_tables(subquery, &mut counter),
                _ => {}
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

/// Unit tests for the SQL rewrites, each pinning the exact text one rewrite
/// produces for the client queries it exists to serve.
#[cfg(test)]
mod tests {
    use super::*;

    /// Casts to `regtype`, bare or `pg_catalog`-qualified, become TEXT casts.
    #[test]
    fn test_rewrite_regtype_cast() {
        let cases = vec![
            ("SELECT x::regtype", "SELECT x::TEXT"),
            ("SELECT x::pg_catalog.regtype", "SELECT x::TEXT"),
            ("SELECT y::pg_catalog.regtype::text", "SELECT y::TEXT::TEXT"),
        ];
        for (input, expected) in cases {
            assert_eq!(rewrite_regtype_cast(input).unwrap(), expected);
        }
    }

    /// `array_agg(...)::varchar` becomes an `array_to_string` call, while a cast
    /// to another text type is left alone.
    #[test]
    fn test_rewrite_array_agg_varchar_cast() {
        let input =
            "SELECT array_agg(inhparent::bigint ORDER BY inhseqno)::varchar FROM pg_catalog.pg_inherits";
        let expected = "SELECT pg_catalog.array_to_string(array_agg(inhparent::BIGINT ORDER BY inhseqno), ',') FROM pg_catalog.pg_inherits";
        assert_eq!(rewrite_array_agg_varchar_cast(input).unwrap(), expected);

        let untouched = "SELECT array_agg(inhparent)::text FROM pg_catalog.pg_inherits";
        assert_eq!(
            rewrite_array_agg_varchar_cast(untouched).unwrap(),
            "SELECT array_agg(inhparent)::TEXT FROM pg_catalog.pg_inherits"
        );
    }

    /// Schema-qualified catalog type casts become TEXT, in both the `::` and the
    /// `CAST(... AS ...)` spelling.
    #[test]
    fn test_rewrite_pg_custom_types() {
        let cases = vec![
            // literal keeps ::
            ("SELECT 'a'::pg_catalog.text", "SELECT 'a'::TEXT"),
            // simple identifier keeps ::
            ("SELECT x::pg_catalog.regtype", "SELECT x::TEXT"),
            ("SELECT x::pg_catalog.regclass", "SELECT x::TEXT"),
            // an explicit CAST stays CAST
            (
                "SELECT CAST(y AS pg_catalog.regtype)",
                "SELECT CAST(y AS TEXT)",
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(
                rewrite_schema_qualified_custom_types(input).unwrap(),
                expected,
                "Failed for input: {input}"
            );
        }
    }

    /// A `'name'::regclass::oid` literal collapses to a single `oid('name')`
    /// lookup, at statement level, inside a CTE and beside a wildcard.
    #[test]
    fn test_regclass_with_oid() {
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
            assert_eq!(transformed, expected, "Failed for input: {input}");
        }
    }

    /// `pg_catalog.text` casts lose the schema prefix and become plain TEXT.
    #[test]
    fn test_rewrite_schema_qualified_text() {
        let cases = vec![
            ("SELECT 'a'::pg_catalog.text", "SELECT 'a'::TEXT"),
            (
                "SELECT CAST('x' AS pg_catalog.text)",
                "SELECT CAST('x' AS TEXT)",
            ),
            (
                "WITH q AS (SELECT 'b'::pg_catalog.text) SELECT * FROM q",
                "WITH q AS (SELECT 'b'::TEXT) SELECT * FROM q",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(rewrite_schema_qualified_text(input).unwrap(), expected);
        }
    }

    /// Non-literal `::regclass` and `::oid` casts are dropped, while the literal
    /// and numeric forms are left to the passes that resolve them.
    #[test]
    fn test_drop_redundant_oid_and_regclass_casts() -> Result<(), Box<dyn std::error::Error>> {
        // Non-literal `::regclass` / `::oid` casts are dropped (value preserved).
        let r = drop_redundant_oid_and_regclass_casts("SELECT c.oid::regclass")?;
        assert!(
            !r.to_lowercase().contains("regclass"),
            "regclass dropped: {r}"
        );
        assert!(r.contains("c.oid"), "{r}");

        let o = drop_redundant_oid_and_regclass_casts("SELECT proargtypes::oid")?;
        assert!(
            o.contains("proargtypes") && !o.to_lowercase().contains("::oid"),
            "{o}"
        );

        // String-literal regclass and numeric ::oid are left for the dedicated
        // passes (OID lookup / BIGINT mapping).
        let lit = drop_redundant_oid_and_regclass_casts("SELECT 'pg_class'::regclass")?;
        assert!(
            lit.to_lowercase().contains("regclass"),
            "literal kept: {lit}"
        );
        let num = drop_redundant_oid_and_regclass_casts("SELECT 0::oid")?;
        assert!(
            num.to_lowercase().contains("::oid") || num.contains("oid"),
            "num kept: {num}"
        );
        Ok(())
    }

    /// `::oid[]` casts are dropped because the column is already an integer
    /// array, while a scalar `::oid` cast is not this pass's business.
    #[test]
    fn test_drop_oid_array_cast() -> Result<(), Box<dyn std::error::Error>> {
        // `proargtypes::oid[]` (element_types/parameters) drops its cast - the
        // column is already an integer array, so the bare expression remains.
        let out = drop_oid_array_cast("SELECT proargtypes::oid[]")?;
        assert!(out.contains("proargtypes"), "{out}");
        assert!(!out.to_lowercase().contains("oid["), "oid[] gone: {out}");
        assert!(!out.to_uppercase().contains("::"), "cast dropped: {out}");

        // Scalar `::oid` and unrelated array casts are untouched.
        let scalar = drop_oid_array_cast("SELECT x::oid")?;
        assert!(
            scalar.to_lowercase().contains("oid"),
            "scalar oid kept: {scalar}"
        );
        Ok(())
    }

    /// The whole-row arguments of `_pg_truetypid` / `_pg_truetypmod` expand into
    /// the columns each function reads, keeping the call site's aliases.
    #[test]
    fn test_rewrite_pg_truetypid_composite_args() -> Result<(), Box<dyn std::error::Error>> {
        // The whole-row wildcard args expand into the columns each function reads,
        // preserving the call-site aliases.
        let id = rewrite_pg_truetypid_composite_args(
            "SELECT information_schema._pg_truetypid(a.*, t.*) FROM pg_attribute a, pg_type t",
        )?;
        assert!(id.contains("a.atttypid"), "{id}");
        assert!(id.contains("t.typtype"), "{id}");
        assert!(id.contains("t.typbasetype"), "{id}");
        assert!(!id.contains(".*"), "wildcards gone: {id}");

        let md = rewrite_pg_truetypid_composite_args(
            "SELECT information_schema._pg_truetypmod(att.*, ty.*) FROM pg_attribute att, pg_type ty",
        )?;
        assert!(md.contains("att.atttypmod"), "{md}");
        assert!(md.contains("ty.typtype"), "{md}");
        assert!(md.contains("ty.typtypmod"), "{md}");

        // Unrelated functions and already-scalar calls are untouched.
        let other = rewrite_pg_truetypid_composite_args("SELECT some_func(a.*, t.*)")?;
        assert!(other.contains("a.*") && other.contains("t.*"), "{other}");
        Ok(())
    }

    /// Every `information_schema` domain cast becomes its base type, unbounded
    /// `character varying` becomes TEXT, and `varchar(n)` stays as written.
    #[test]
    fn test_rewrite_information_schema_casts() -> Result<(), Box<dyn std::error::Error>> {
        // Each information_schema domain cast becomes its base type.
        let cases = vec![
            ("SELECT x::information_schema.sql_identifier", "TEXT"),
            ("SELECT x::information_schema.character_data", "TEXT"),
            ("SELECT x::information_schema.yes_or_no", "TEXT"),
            ("SELECT x::information_schema.cardinal_number", "INTEGER"),
            ("SELECT x::information_schema.time_stamp", "TIMESTAMP"),
            (
                "SELECT CAST(c.relname AS information_schema.sql_identifier)",
                "TEXT",
            ),
        ];
        for (input, base) in cases {
            let out = rewrite_information_schema_casts(input)?;
            assert!(
                !out.to_lowercase().contains("information_schema"),
                "domain type should be gone, got: {out}"
            );
            assert!(
                out.to_uppercase().contains(base),
                "expected base type {base} in: {out}"
            );
        }

        // Casts to other schemas are left untouched.
        let untouched = rewrite_information_schema_casts("SELECT 'a'::pg_catalog.text")?;
        assert!(untouched.to_lowercase().contains("pg_catalog.text"));

        // Bare `character varying` (unbounded varchar) -> TEXT.
        let cv = rewrite_information_schema_casts("SELECT NULL::character varying")?;
        assert!(
            cv.to_uppercase().contains("TEXT") && !cv.to_uppercase().contains("CHARACTER VARYING"),
            "character varying should become TEXT: {cv}"
        );
        // Length-qualified varchar is left as-is (DataFusion plans it).
        let vl = rewrite_information_schema_casts("SELECT NULL::varchar(10)")?;
        assert!(vl.to_lowercase().contains("varchar(10)"), "got {vl}");
        Ok(())
    }

    /// A multi-column `IN` subquery becomes a correlated `EXISTS` with one
    /// equality per column, while a single-column `IN` is left to `DataFusion`.
    #[test]
    fn test_rewrite_tuple_in_subquery_to_exists() -> Result<(), Box<dyn std::error::Error>> {
        // Multi-column IN -> correlated EXISTS, with one equality per column,
        // preserving the subquery's own WHERE.
        let out = rewrite_tuple_in_subquery_to_exists(
            "SELECT a FROM t1 WHERE (a, b) IN (SELECT s.x, s.y FROM s WHERE s.k = 1)",
        )?;
        let lo = out.to_lowercase();
        assert!(lo.contains("exists"), "expected EXISTS: {out}");
        assert!(!lo.contains(" in ("), "the IN should be gone: {out}");
        assert!(
            lo.contains("s.x = a") && lo.contains("s.y = b"),
            "per-column equalities: {out}"
        );
        assert!(
            lo.contains("s.k = 1"),
            "subquery's own WHERE preserved: {out}"
        );

        // NOT IN -> NOT EXISTS.
        let neg = rewrite_tuple_in_subquery_to_exists(
            "SELECT a FROM t1 WHERE (a, b) NOT IN (SELECT s.x, s.y FROM s)",
        )?;
        assert!(
            neg.to_lowercase().contains("not exists"),
            "expected NOT EXISTS: {neg}"
        );

        // Single-column IN is left for DataFusion (it handles that natively).
        let single =
            rewrite_tuple_in_subquery_to_exists("SELECT a FROM t1 WHERE a IN (SELECT x FROM s)")?;
        assert!(
            single.to_lowercase().contains(" in (") && !single.to_lowercase().contains("exists"),
            "single-column IN untouched: {single}"
        );
        Ok(())
    }

    /// `EXISTS` becomes `count(*) > 0` and `NOT EXISTS` becomes `count(*) = 0`,
    /// except over a grouped subquery where the two are not equivalent.
    #[test]
    fn test_rewrite_exists_to_count() -> Result<(), Box<dyn std::error::Error>> {
        // EXISTS in a CASE position -> (SELECT count(*) ...) > 0
        let out = rewrite_exists_to_count(
            "SELECT CASE WHEN EXISTS (SELECT 1 FROM t2 WHERE t2.id = t1.id) THEN 1 ELSE 0 END FROM t1",
        )?;
        let lo = out.to_lowercase();
        assert!(!lo.contains("exists"), "EXISTS should be gone: {out}");
        assert!(lo.contains("count(*)"), "expected count(*): {out}");
        assert!(lo.contains("> 0"), "expected > 0 comparison: {out}");

        // NOT EXISTS -> = 0
        let neg = rewrite_exists_to_count(
            "SELECT id FROM t1 WHERE NOT EXISTS (SELECT 1 FROM t2 WHERE t2.id = t1.id)",
        )?;
        let nlo = neg.to_lowercase();
        assert!(!nlo.contains("exists"), "NOT EXISTS should be gone: {neg}");
        assert!(nlo.contains("= 0"), "expected = 0 comparison: {neg}");

        // A subquery with GROUP BY is left untouched (count(*) not equivalent).
        let grouped =
            rewrite_exists_to_count("SELECT 1 WHERE EXISTS (SELECT 1 FROM t GROUP BY x)")?;
        assert!(
            grouped.to_lowercase().contains("exists"),
            "grouped EXISTS must be left as-is: {grouped}"
        );
        Ok(())
    }

    /// A boolean scalar subquery becomes an `EXISTS` carrying the comparison,
    /// chains into the count rewrite, and leaves value subqueries alone.
    #[test]
    fn test_rewrite_boolean_scalar_subquery_to_exists() -> Result<(), Box<dyn std::error::Error>> {
        // getTypeInfo's predicate: a boolean scalar subquery becomes EXISTS with
        // the projected comparison folded into the WHERE.
        let out = rewrite_boolean_scalar_subquery_to_exists(
            "SELECT t.typname FROM pg_type t WHERE t.typrelid = 0 \
             OR (SELECT c.relkind = 'c' FROM pg_class c WHERE c.oid = t.typrelid)",
        )?;
        let lo = out.to_lowercase();
        assert!(lo.contains("exists"), "expected EXISTS: {out}");
        assert!(
            lo.contains("c.relkind = 'c'"),
            "comparison should be folded into the body: {out}"
        );
        assert!(
            lo.contains("c.oid = t.typrelid"),
            "original correlation must be kept: {out}"
        );

        // Chained with rewrite_exists_to_count, the whole thing becomes a
        // count(*) > 0 scalar DataFusion can plan.
        let counted = rewrite_exists_to_count(&out)?;
        let clo = counted.to_lowercase();
        assert!(
            !clo.contains("exists"),
            "EXISTS should be reduced: {counted}"
        );
        assert!(clo.contains("count(*)"), "expected count(*): {counted}");

        // A scalar subquery that projects a data value (not a comparison) is
        // left untouched.
        let untouched = rewrite_boolean_scalar_subquery_to_exists(
            "SELECT (SELECT c.relname FROM pg_class c WHERE c.oid = t.typrelid) FROM pg_type t",
        )?;
        assert!(
            !untouched.to_lowercase().contains("exists"),
            "value subquery must be left as-is: {untouched}"
        );
        Ok(())
    }

    /// `array_upper` is renamed to `array_length`, whether the call is bare or
    /// `pg_catalog`-qualified.
    #[test]
    fn test_rewrite_array_upper_to_array_length() -> Result<(), Box<dyn std::error::Error>> {
        let out = rewrite_array_upper_to_array_length(
            "SELECT generate_series(1, array_upper(current_schemas(false), 1))",
        )?;
        let lo = out.to_lowercase();
        assert!(
            !lo.contains("array_upper"),
            "array_upper should be gone: {out}"
        );
        assert!(lo.contains("array_length"), "expected array_length: {out}");
        // A schema-qualified call is renamed too.
        let qualified = rewrite_array_upper_to_array_length("SELECT pg_catalog.array_upper(a, 1)")?;
        assert!(
            qualified.to_lowercase().contains("array_length"),
            "{qualified}"
        );
        Ok(())
    }

    /// A boolean column compared to `'t'` / `'f'` gets a boolean literal, while a
    /// character column compared to the same text keeps it.
    #[test]
    fn test_rewrite_boolean_column_char_comparisons() -> Result<(), Box<dyn std::error::Error>> {
        // A boolean column compared to a char literal becomes a boolean literal.
        let out = rewrite_boolean_column_char_comparisons(
            "SELECT 1 FROM pg_index i WHERE i.indisprimary = 't' AND i.indisunique = 'f'",
        )?;
        let lo = out.to_lowercase();
        assert!(lo.contains("indisprimary = true"), "expected = true: {out}");
        assert!(
            lo.contains("indisunique = false"),
            "expected = false: {out}"
        );

        // A character column that happens to compare to 't' is left untouched
        // (contype = 't' means a trigger constraint, not a boolean).
        let untouched = rewrite_boolean_column_char_comparisons(
            "SELECT 1 FROM pg_constraint WHERE contype = 't'",
        )?;
        assert!(
            untouched.to_lowercase().contains("contype = 't'"),
            "char column must be left as-is: {untouched}"
        );
        Ok(())
    }

    /// `OPERATOR(pg_catalog.~)` becomes the plain regex-match operator.
    #[test]
    fn test_rewrite_pg_custom_operator() {
        let cases = vec![
            ("SELECT 'b' OPERATOR(pg_catalog.~) 'a'", "SELECT 'b' ~ 'a'"),
            (
                "SELECT c.relname OPERATOR(pg_catalog.~) '^(t)$' FROM pg_class c",
                "SELECT c.relname ~ '^(t)$' FROM pg_class AS c",
            ),
        ];
        for (input, expected) in cases {
            let transformed = rewrite_pg_custom_operator(input).unwrap();
            assert_eq!(transformed, expected);
        }
    }

    /// `COLLATE pg_catalog.default` is stripped wherever it appears, leaving the
    /// collated expression itself untouched.
    #[test]
    fn test_strip_default_collate() {
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
            assert_eq!(transformed, expected, "Failed for input: {input}");
        }
    }

    /// A lower-case `utc` time zone literal is normalised, and an already
    /// upper-case one is returned verbatim.
    #[test]
    fn test_rewrite_time_zone_utc() {
        let input = "SELECT pg_postmaster_start_time() AT TIME ZONE 'utc'";
        let expected = "SELECT pg_postmaster_start_time() AT TIME ZONE 'UTC'";
        assert_eq!(rewrite_time_zone_utc(input).unwrap(), expected);

        let unchanged = "SELECT pg_postmaster_start_time() AT TIME ZONE 'UTC'";
        assert_eq!(rewrite_time_zone_utc(unchanged).unwrap(), unchanged);
    }

    /// `'name'::regclass` becomes an `oid('name')` lookup in a plain query, a
    /// CTE, a join and a derived table.
    #[test]
    fn test_various_sql_cases() {
        let cases = vec![
            (
                "SELECT 'pg_namespace'::regclass FROM foo LIMIT 10",
                "SELECT oid('pg_namespace') FROM foo LIMIT 10",
            ),
            (
                "WITH cte AS (SELECT 'pg_class'::regclass) SELECT * FROM cte",
                "WITH cte AS (SELECT oid('pg_class')) SELECT * FROM cte",
            ),
            (
                "SELECT t.*, 'pg_class'::regclass FROM table1 t JOIN table2 ON true",
                "SELECT t.*, oid('pg_class') FROM table1 AS t JOIN table2 ON true",
            ),
            (
                "SELECT * FROM (SELECT 'pg_class'::regclass) sub",
                "SELECT * FROM (SELECT oid('pg_class')) AS sub",
            ),
        ];

        for (input, expected) in cases {
            let transformed = replace_regclass(input).unwrap();
            assert_eq!(transformed, expected, "Failed for input: {input}");
        }
    }

    /// `SET` variables are qualified with `pg_catalog`, including the `SET LOCAL
    /// ... TO` spelling, while a plain query is passed through.
    #[test]
    fn test_set_show_query_rewrite() {
        assert_eq!(
            replace_set_command_with_namespace("SET application_name = 'x'").unwrap(),
            "SET pg_catalog.application_name = 'x'"
        );
        assert_eq!(
            replace_set_command_with_namespace("SELECT foo FROM bar").unwrap(),
            "SELECT foo FROM bar"
        );

        assert_eq!(
            replace_set_command_with_namespace("SET LOCAL work_mem TO '4MB'").unwrap(),
            "SET LOCAL pg_catalog.work_mem = '4MB'"
        );
    }

    /// `array(<sub-query>)` becomes a `pg_get_array` call, a multi-argument
    /// `array(...)` is rejected, and `ARRAY[...]` literals are left alone.
    #[test]
    fn test_rewrite_array_subquery() {
        /* basic happy-path */
        let in_sql = "SELECT array(SELECT rolname FROM pg_catalog.pg_roles ORDER BY 1)";
        let expect =
            "SELECT pg_catalog.pg_get_array((SELECT rolname FROM pg_catalog.pg_roles ORDER BY 1))";
        let out_sql = rewrite_array_subquery(in_sql).unwrap();
        log::debug!("test_rewrite_array_subquery {out_sql}");
        assert_eq!(out_sql, expect);

        let in_sql = "select 1";
        let out_sql = rewrite_array_subquery(in_sql).unwrap();
        assert_eq!(in_sql, out_sql);

        /* ARRAY with more than one arg - rejected */
        let bad_sql = "SELECT array(x, y)";
        assert!(rewrite_array_subquery(bad_sql).is_err());

        /* array literal is *not* touched */
        let lit_sql = "SELECT ARRAY[1,2,3]";
        let out_sql = rewrite_array_subquery(lit_sql).unwrap();
        assert_eq!(lit_sql, out_sql);
    }

    /// A `'{...}'` brace literal becomes an array literal, and SQL without one
    /// comes back verbatim.
    #[test]
    fn test_rewrite_brace_array_literal() {
        let in_sql = "SELECT pol.polroles = '{0}' FROM pg_catalog.pg_policy pol";
        let expect = "SELECT pol.polroles = ['0'] FROM pg_catalog.pg_policy AS pol";
        assert_eq!(rewrite_brace_array_literal(in_sql).unwrap(), expect);

        // nothing to do -> echoes input
        let plain = "SELECT 1";
        assert_eq!(rewrite_brace_array_literal(plain).unwrap(), plain);
    }

    /// A `regoper` cast collapses to NULL, keeping any cast layered on top of it.
    #[test]
    fn test_rewrite_regoper_cast() {
        let input = "SELECT conexclop::regoper::text FROM pg_catalog.pg_constraint";
        let expected = "SELECT NULL::TEXT FROM pg_catalog.pg_constraint";
        assert_eq!(rewrite_regoper_cast(input).unwrap(), expected);

        let input = "SELECT conexclop::pg_catalog.regoper::varchar FROM pg_catalog.pg_constraint";
        let expected = "SELECT NULL::VARCHAR FROM pg_catalog.pg_constraint";
        assert_eq!(rewrite_regoper_cast(input).unwrap(), expected);

        let input = "SELECT conexclop::regoper FROM pg_catalog.pg_constraint";
        let expected = "SELECT NULL FROM pg_catalog.pg_constraint";
        assert_eq!(rewrite_regoper_cast(input).unwrap(), expected);
    }

    /// Casts to the quoted `"char"` type become plain `CHAR`.
    #[test]
    fn test_rewrite_char_cast() {
        let cases = vec![
            ("SELECT 'c'::\"char\"", "SELECT 'c'::CHAR"),
            ("SELECT CAST('a' AS \"char\")", "SELECT CAST('a' AS CHAR)"),
            ("SELECT x::pg_catalog.\"char\"", "SELECT x::CHAR"),
        ];

        for (input, expected) in cases {
            assert_eq!(rewrite_char_cast(input).unwrap(), expected);
        }
    }

    /// Casts to `xid` become BIGINT, the type transaction ids are stored as.
    #[test]
    fn test_rewrite_xid_cast() {
        let cases = vec![
            ("SELECT x::xid", "SELECT x::BIGINT"),
            ("SELECT x::pg_catalog.xid::text", "SELECT x::BIGINT::TEXT"),
        ];

        for (input, expected) in cases {
            assert_eq!(rewrite_xid_cast(input).unwrap(), expected);
        }
    }

    /// Casts to the `name` pseudo-type become TEXT.
    #[test]
    fn test_rewrite_name_cast() {
        let cases = vec![
            ("SELECT '_RETURN'::name", "SELECT '_RETURN'::TEXT"),
            (
                "SELECT CAST('foo' AS pg_catalog.name)",
                "SELECT CAST('foo' AS TEXT)",
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(rewrite_name_cast(input).unwrap(), expected);
        }
    }

    /// The `available_updates` correlated ARRAY sub-query collapses to NULL.
    #[test]
    fn test_rewrite_available_updates() {
        let input =
            "SELECT array(select unnest from unnest(available_versions) where unnest > extversion)";
        let expected = "SELECT NULL";
        assert_eq!(rewrite_available_updates(input).unwrap(), expected);
    }

    /// Casts to `regoperator` become TEXT.
    #[test]
    fn test_rewrite_regoperator_cast() {
        let cases = vec![
            ("SELECT x::regoperator", "SELECT x::TEXT"),
            (
                "SELECT x::pg_catalog.regoperator::varchar",
                "SELECT x::TEXT::VARCHAR",
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(rewrite_regoperator_cast(input).unwrap(), expected);
        }
    }

    /// Casts to `regprocedure` become TEXT.
    #[test]
    fn test_rewrite_regprocedure_cast() {
        let cases = vec![
            ("SELECT x::regprocedure", "SELECT x::TEXT"),
            (
                "SELECT x::pg_catalog.regprocedure::varchar",
                "SELECT x::TEXT::VARCHAR",
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(rewrite_regprocedure_cast(input).unwrap(), expected);
        }
    }

    /// Casts to `regproc` become TEXT.
    #[test]
    fn test_rewrite_regproc_cast() {
        let cases = vec![
            ("SELECT x::regproc", "SELECT x::TEXT"),
            (
                "SELECT x::pg_catalog.regproc::varchar",
                "SELECT x::TEXT::VARCHAR",
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(rewrite_regproc_cast(input).unwrap(), expected);
        }
    }

    /// A `regproc` column compared against an OID is sent through
    /// `pg_proc_oid()`, on whichever side it is written, and comparisons that
    /// need no lookup are left alone.
    #[test]
    fn test_resolve_regproc_columns_to_oids_in_comparisons() {
        let cases = vec![
            // The Npgsql type-loading join: the OID side stays, the name side is resolved.
            (
                "SELECT 1 FROM pg_type a JOIN pg_proc ON pg_proc.oid = a.typreceive",
                "SELECT 1 FROM pg_type a JOIN pg_proc ON pg_proc.oid = pg_proc_oid(a.typreceive)",
            ),
            // ... whichever side the client writes it on.
            (
                "SELECT 1 FROM pg_type a JOIN pg_proc p ON a.typoutput = p.oid",
                "SELECT 1 FROM pg_type a JOIN pg_proc p ON pg_proc_oid(a.typoutput) = p.oid",
            ),
            // A function name written as a literal resolves too, so a schema-qualified
            // spelling still matches the bare name the catalog stores.
            (
                "SELECT typinput = 'pg_catalog.array_in' FROM pg_type",
                "SELECT pg_proc_oid(typinput) = pg_proc_oid('pg_catalog.array_in') FROM pg_type",
            ),
            (
                "SELECT typreceive <> 0 FROM pg_type",
                "SELECT pg_proc_oid(typreceive) <> 0 FROM pg_type",
            ),
            // Two regproc columns already hold comparable names.
            (
                "SELECT 1 FROM pg_type WHERE typinput = typoutput",
                "SELECT 1 FROM pg_type WHERE typinput = typoutput",
            ),
            // Columns that are not regproc, and non-equality operators, are untouched.
            (
                "SELECT 1 FROM pg_type a JOIN pg_class c ON c.oid = a.typrelid",
                "SELECT 1 FROM pg_type a JOIN pg_class c ON c.oid = a.typrelid",
            ),
            (
                "SELECT 1 FROM pg_type WHERE typreceive < 'b'",
                "SELECT 1 FROM pg_type WHERE typreceive < 'b'",
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(
                resolve_regproc_columns_to_oids_in_comparisons(input).unwrap(),
                expected
            );
        }
    }

    /// A bare `ORDER BY` name that matches an output column becomes that
    /// column's position; qualified names, unknown names and wildcard SELECT
    /// lists are left as written.
    #[test]
    fn test_resolve_order_by_names_to_output_positions() {
        let cases = vec![
            // Npgsql's enum query: `oid` is the selected pg_type.oid, but pg_enum has an
            // `oid` too, so the name has to become the position to stay unambiguous.
            (
                "SELECT pg_type.oid, enumlabel FROM pg_enum JOIN pg_type ON pg_type.oid = enumtypid ORDER BY oid, enumsortorder",
                "SELECT pg_type.oid, enumlabel FROM pg_enum JOIN pg_type ON pg_type.oid = enumtypid ORDER BY 1, enumsortorder",
            ),
            // An alias is an output column name as well, and sort options survive.
            (
                "SELECT relname AS name FROM pg_class ORDER BY name DESC",
                "SELECT relname AS name FROM pg_class ORDER BY 1 DESC",
            ),
            // A qualified name means the input column in PostgreSQL, so it stays.
            (
                "SELECT pg_type.oid FROM pg_type ORDER BY pg_type.oid",
                "SELECT pg_type.oid FROM pg_type ORDER BY pg_type.oid",
            ),
            // A name that is not in the SELECT list stays; it can only be an input column.
            (
                "SELECT typname FROM pg_type ORDER BY oid",
                "SELECT typname FROM pg_type ORDER BY oid",
            ),
            // Positions after a wildcard are unknown before expansion, so nothing moves.
            (
                "SELECT *, typname FROM pg_type ORDER BY typname",
                "SELECT *, typname FROM pg_type ORDER BY typname",
            ),
            // The ORDER BY of a subquery reads that subquery's own SELECT list.
            (
                "SELECT * FROM (SELECT pg_type.oid FROM pg_enum JOIN pg_type ON true ORDER BY oid) t",
                "SELECT * FROM (SELECT pg_type.oid FROM pg_enum JOIN pg_type ON true ORDER BY 1) t",
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(
                resolve_order_by_names_to_output_positions(input).unwrap(),
                expected
            );
        }
    }

    /// `::oid` on a `regproc` column resolves through `pg_proc`, and on any
    /// other name through `pg_class`.
    #[test]
    fn test_rewrite_oid_cast_on_regproc_column() -> Result<(), Box<dyn std::error::Error>> {
        // A regproc column holds a function name, so `::oid` resolves it in pg_proc;
        // every other name is a relation name and resolves in pg_class.
        assert_eq!(
            rewrite_oid_cast("SELECT amhandler::oid FROM pg_am")?,
            "SELECT pg_proc_oid(amhandler) FROM pg_am"
        );
        assert_eq!(
            rewrite_oid_cast("SELECT a.typinput::oid FROM pg_type a")?,
            "SELECT pg_proc_oid(a.typinput) FROM pg_type a"
        );
        assert_eq!(
            rewrite_oid_cast("SELECT relname::oid FROM pg_class")?,
            "SELECT oid(relname) FROM pg_class"
        );

        Ok(())
    }

    /// A `pg_catalog`-qualified table function call loses the prefix, and SQL
    /// without one is returned verbatim.
    #[test]
    fn test_rewrite_schema_qualified_udtfs() {
        let input = "SELECT * FROM pg_catalog.pg_get_keywords()";
        let expected = "SELECT * FROM pg_get_keywords()";
        assert_eq!(rewrite_schema_qualified_udtfs(input).unwrap(), expected);

        let plain = "SELECT 1";
        assert_eq!(rewrite_schema_qualified_udtfs(plain).unwrap(), plain);
    }

    /// Tuple equality decomposes into one `AND`-ed comparison per element.
    #[test]
    fn test_rewrite_tuple_equality() {
        let input = "SELECT * FROM t JOIN u ON (t.a, t.b) = (u.c, u.d)";
        let expected = "SELECT * FROM t JOIN u ON t.a = u.c AND t.b = u.d";
        assert_eq!(rewrite_tuple_equality(input).unwrap(), expected);

        // unchanged if no tuples
        let plain = "SELECT 1";
        assert_eq!(rewrite_tuple_equality(plain).unwrap(), plain);
    }

    /// A subquery's table is schema-qualified and aliased, while the outer
    /// correlated reference keeps its own qualifier.
    #[test]
    fn test_alias_subquery_tables() -> Result<(), Box<dyn std::error::Error>> {
        let sql =
            "SELECT (SELECT count(*) FROM pg_trigger WHERE tgrelid = rel.oid) FROM pg_class rel";
        let out = alias_subquery_tables(sql)?;
        assert!(out.contains("FROM pg_catalog.pg_trigger AS subq0_t"));
        // The outer correlated reference `rel.oid` must NOT be re-aliased.
        assert!(
            out.contains("rel.oid"),
            "outer ref must be preserved: {out}"
        );
        Ok(())
    }

    /// Aliasing a subquery's table also re-qualifies the subquery's own
    /// references to that table, which would otherwise stop resolving.
    #[test]
    fn test_alias_subquery_tables_requalifies_self_refs() -> Result<(), Box<dyn std::error::Error>>
    {
        // When the subquery refers to its OWN table by name (`pg_database.datname`),
        // aliasing the table to `subq0_t` must re-qualify those refs too - otherwise
        // they no longer resolve. (Regression: the information_schema `collations`,
        // `usage_privileges`, etc. views hit exactly this.)
        let sql = "SELECT 1 WHERE x = (SELECT pg_database.encoding FROM pg_database \
                   WHERE pg_database.datname = 'd')";
        let out = alias_subquery_tables(sql)?;
        assert!(
            out.contains("FROM pg_catalog.pg_database AS subq0_t"),
            "{out}"
        );
        // Both the projected and the WHERE self-references are re-qualified.
        assert!(out.contains("subq0_t.encoding"), "projection ref: {out}");
        assert!(out.contains("subq0_t.datname"), "where ref: {out}");
        assert!(
            !out.contains("pg_database.datname"),
            "stale self-ref left behind: {out}"
        );
        Ok(())
    }

    /// Tables inside `EXISTS` subqueries are qualified and aliased too, and the
    /// outer correlated reference survives.
    #[test]
    fn test_alias_subquery_tables_handles_exists() -> Result<(), Box<dyn std::error::Error>> {
        // Tables inside EXISTS (and IN) subqueries must also be qualified to
        // pg_catalog and aliased - the `views` information_schema view relies on
        // this for its `EXISTS (SELECT 1 FROM pg_trigger ...)`.
        let sql =
            "SELECT 1 WHERE EXISTS (SELECT 1 FROM pg_trigger WHERE pg_trigger.tgrelid = c.oid)";
        let out = alias_subquery_tables(sql)?;
        assert!(
            out.contains("FROM pg_catalog.pg_trigger AS subq0_t"),
            "{out}"
        );
        assert!(
            out.contains("subq0_t.tgrelid"),
            "self-ref requalified: {out}"
        );
        assert!(
            out.contains("c.oid"),
            "outer correlated ref preserved: {out}"
        );
        Ok(())
    }

    /// A correlated LATERAL aggregate becomes a grouped equi-join on a
    /// synthesised key column.
    #[test]
    fn test_decorrelate_lateral_aggregate() -> Result<(), Box<dyn std::error::Error>> {
        let out = decorrelate_lateral_aggregate(
            "SELECT c.relname, i.n FROM pg_class c \
             LEFT JOIN LATERAL (SELECT sum(pg_index.x) AS n FROM pg_index \
             WHERE pg_index.indrelid = c.oid) i ON true",
        )?;
        let lo = out.to_lowercase();
        assert!(!lo.contains("lateral"), "LATERAL removed: {out}");
        assert!(lo.contains("group by"), "GROUP BY added: {out}");
        assert!(
            lo.contains("__decorr_key_0") && lo.contains("= c.oid"),
            "equi-join on the decorrelated key: {out}"
        );
        Ok(())
    }

    /// A LATERAL subquery without an aggregate is no grouping candidate, so it
    /// stays a LATERAL join.
    #[test]
    fn test_decorrelate_lateral_aggregate_leaves_non_aggregate_lateral(
    ) -> Result<(), Box<dyn std::error::Error>> {
        // A LATERAL without an aggregate is not a grouping candidate; leave it untouched.
        let sql = "SELECT c.relname, i.x FROM pg_class c \
             LEFT JOIN LATERAL (SELECT pg_index.x FROM pg_index \
             WHERE pg_index.indrelid = c.oid) i ON true";
        let out = decorrelate_lateral_aggregate(sql)?;
        assert!(out.to_lowercase().contains("lateral"), "untouched: {out}");
        Ok(())
    }

    /// The table function call is renamed to the internal name, while a bare
    /// reference to the view of the same name is left alone.
    #[test]
    fn test_rewrite_available_extension_versions_source() -> Result<(), Box<dyn std::error::Error>>
    {
        // The function-call form (in FROM, with args) is renamed to the internal name...
        let called = rewrite_available_extension_versions_source(
            "SELECT * FROM pg_available_extension_versions() e(name)",
        )?;
        assert!(
            called
                .to_lowercase()
                .contains("available_extension_versions()"),
            "{called}"
        );
        assert!(
            !called
                .to_lowercase()
                .contains("pg_available_extension_versions"),
            "call renamed: {called}"
        );
        // ...but a bare reference to the view of that name is left untouched.
        let view = rewrite_available_extension_versions_source(
            "SELECT count(*) FROM pg_catalog.pg_available_extension_versions",
        )?;
        assert!(
            view.to_lowercase()
                .contains("pg_available_extension_versions"),
            "view reference preserved: {view}"
        );
        Ok(())
    }

    /// `anyarray` becomes scalar TEXT and `name[]` becomes `text[]`, matching
    /// how those columns are stored.
    #[test]
    fn test_rewrite_text_backed_type_casts() -> Result<(), Box<dyn std::error::Error>> {
        // `anyarray` becomes scalar `text`; `name[]` becomes `text[]`.
        let out = rewrite_text_backed_type_casts(
            "SELECT NULL::anyarray AS a, '{x}'::name[] AS b, x::pg_catalog.anyarray AS c",
        )?;
        let up = out.to_uppercase();
        assert!(!up.contains("ANYARRAY"), "anyarray rewritten away: {out}");
        assert!(!up.contains("NAME[]"), "name[] rewritten away: {out}");
        assert!(
            up.contains("::TEXT[]") || up.contains("TEXT[]"),
            "name[] -> text[]: {out}"
        );
        Ok(())
    }

    /// `oid[]` belongs to [`drop_oid_array_cast`], so this rewrite must not
    /// touch it.
    #[test]
    fn test_rewrite_text_backed_type_casts_leaves_oid_array(
    ) -> Result<(), Box<dyn std::error::Error>> {
        // `oid[]` is the job of drop_oid_array_cast; this rewrite must not touch it.
        let out = rewrite_text_backed_type_casts("SELECT proargtypes::oid[]")?;
        assert!(
            out.to_lowercase().contains("oid"),
            "oid[] left intact: {out}"
        );
        Ok(())
    }
}
