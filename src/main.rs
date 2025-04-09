use datafusion::prelude::*;
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;
use sqlparser::ast::{Statement, Query, SetExpr, Select, TableWithJoins, TableFactor, TableAlias, Expr, BinaryOperator, Value};
use std::env;
use std::collections::{HashMap, HashSet};


fn process_table_factor(factor: &TableFactor, table_info: &mut HashMap<String, (HashSet<String>, HashSet<String>)>) {
    match factor {
        TableFactor::Table { name, .. } => {
            let table_name = name.to_string();
            table_info.entry(table_name).or_default();
        }
        TableFactor::Derived { subquery, alias, .. } => {
            let table_name = alias.as_ref().map(|a| a.name.to_string()).unwrap_or_else(|| subquery.to_string());
            table_info.entry(table_name).or_default();
        }
        _ => {}
    }
}


#[tokio::main]
async fn main() -> datafusion::error::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: {} \"SQL_QUERY\"", args[0]);
        std::process::exit(1);
    }
    let sql_query = &args[1];

    let dialect = GenericDialect {};
    let ast = Parser::parse_sql(&dialect, sql_query).expect("Failed to parse SQL");

    let mut table_info: HashMap<String, (HashSet<String>, HashSet<String>)> = HashMap::new();

    if let Some(Statement::Query(query)) = ast.get(0) {
        if let SetExpr::Select(select) = query.body.as_ref() {
            process_select(select, &mut table_info);
        }
    }

    let result: Vec<_> = table_info.into_iter().map(|(table, (columns, filters))| {
        serde_json::json!({
            "table": table,
            "columns": columns,
            "filters": filters,
        })
    }).collect();

    println!("{}", serde_json::to_string_pretty(&result).unwrap());

    Ok(())
}

fn process_select(select: &Select, table_info: &mut HashMap<String, (HashSet<String>, HashSet<String>)>) {
    for table_with_joins in &select.from {
        process_table_with_joins(table_with_joins, table_info);
    }

    for projection in &select.projection {
        if let sqlparser::ast::SelectItem::UnnamedExpr(expr) = projection {
            process_expr(expr, table_info, true);
        }
    }

    if let Some(selection) = &select.selection {
        process_expr(selection, table_info, false);
    }
}

fn process_table_with_joins(table_with_joins: &TableWithJoins, table_info: &mut HashMap<String, (HashSet<String>, HashSet<String>)>) {
    let table_name = match &table_with_joins.relation {
        TableFactor::Table { name, .. } => name.to_string(),
        TableFactor::Derived { subquery, alias, .. } => {
            if let Some(alias) = alias {
                alias.name.to_string()
            } else {
                subquery.to_string()
            }
        }
        _ => return,
    };

    table_info.entry(table_name.clone()).or_default();

    for join in &table_with_joins.joins {
        process_table_factor(&join.relation, table_info);

        match &join.join_operator {
            sqlparser::ast::JoinOperator::Inner(constraint)
            | sqlparser::ast::JoinOperator::LeftOuter(constraint)
            | sqlparser::ast::JoinOperator::RightOuter(constraint)
            | sqlparser::ast::JoinOperator::FullOuter(constraint)
            | sqlparser::ast::JoinOperator::LeftAnti(constraint)
            | sqlparser::ast::JoinOperator::LeftSemi(constraint)
            | sqlparser::ast::JoinOperator::RightAnti(constraint)
            | sqlparser::ast::JoinOperator::RightSemi(constraint) => {
                if let sqlparser::ast::JoinConstraint::On(expr) = constraint {
                    process_expr(expr, table_info, false);
                }
            }
            sqlparser::ast::JoinOperator::CrossJoin
            | sqlparser::ast::JoinOperator::CrossApply
            | sqlparser::ast::JoinOperator::OuterApply => {
                // no ON condition to process
            }
            _ => {}
        }
    }
}

fn process_expr(expr: &Expr, table_info: &mut HashMap<String, (HashSet<String>, HashSet<String>)>, is_projection: bool) {
    match expr {
        Expr::Identifier(ident) => {
            if is_projection {
                for (table, (columns, _)) in table_info.iter_mut() {
                    columns.insert(ident.value.clone());
                }
            }
        }
        Expr::CompoundIdentifier(idents) => {
            if idents.len() == 2 {
                let table = &idents[0].value;
                let column = &idents[1].value;
                if let Some((columns, _)) = table_info.get_mut(table) {
                    columns.insert(column.clone());
                }
            }
        }
        Expr::BinaryOp { left, op, right } => {
            if !is_projection {
                let filter = format!("{} {} {}", left, op, right);
                for (table, (_, filters)) in table_info.iter_mut() {
                    filters.insert(filter.clone());
                }
            }
            process_expr(left, table_info, is_projection);
            process_expr(right, table_info, is_projection);
        }
        Expr::Nested(expr) => process_expr(expr, table_info, is_projection),
        _ => {}
    }
}
