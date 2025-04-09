use sqlparser::ast::{BinaryOperator, Expr, ObjectName, SelectItem, SetExpr, Statement, TableAlias, TableFactor, TableWithJoins};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;
use std::collections::{HashMap, HashSet};
use std::env;

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: {} \"SQL_QUERY\"", args[0]);
        std::process::exit(1);
    }
    let sql_query = &args[1];

    let dialect = GenericDialect {};
    let ast = Parser::parse_sql(&dialect, sql_query).expect("Failed to parse SQL");

    let mut table_info: HashMap<String, (HashSet<String>, HashSet<String>)> = HashMap::new();
    let mut aliases: HashMap<String, String> = HashMap::new();

    if let Some(Statement::Query(query)) = ast.get(0) {
        if let SetExpr::Select(select) = query.body.as_ref() {
            process_select(select, &mut table_info, &mut aliases);
        }
    }

    let result: Vec<_> = table_info
        .into_iter()
        .map(|(table, (columns, filters))| {
            serde_json::json!({
                "table": table,
                "columns": columns,
                "filters": filters,
            })
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&result).unwrap());
}

fn process_select(select: &sqlparser::ast::Select, table_info: &mut HashMap<String, (HashSet<String>, HashSet<String>)>, aliases: &mut HashMap<String, String>) {
    for table_with_joins in &select.from {
        process_table_with_joins(table_with_joins, table_info, aliases);
    }

    for projection in &select.projection {
        match projection {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                collect_columns(expr, table_info, aliases);
            }
            SelectItem::QualifiedWildcard(obj_name, _) => {
                let table = resolve_table_name(&obj_name.to_string(), aliases);
                table_info.entry(table).or_default();
            }
            SelectItem::Wildcard(_) => {}
        }
    }

    if let Some(selection) = &select.selection {
        distribute_filters(selection, table_info, aliases);
    }
}

fn collect_columns(expr: &Expr, table_info: &mut HashMap<String, (HashSet<String>, HashSet<String>)>, aliases: &HashMap<String, String>) {
    match expr {
        Expr::CompoundIdentifier(idents) if idents.len() == 2 => {
            let table = resolve_table_name(&idents[0].value, aliases);
            let column = &idents[1].value;
            table_info.entry(table).or_default().0.insert(column.clone());
        }
        Expr::Identifier(ident) => {
            for (_table, (cols, _)) in table_info.iter_mut() {
                cols.insert(ident.value.clone());
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_columns(left, table_info, aliases);
            collect_columns(right, table_info, aliases);
        }
        Expr::Nested(e) => collect_columns(e, table_info, aliases),
        _ => {}
    }
}

fn distribute_filters(expr: &Expr, table_info: &mut HashMap<String, (HashSet<String>, HashSet<String>)>, aliases: &HashMap<String, String>) {
    match expr {
        Expr::BinaryOp { left, op, right } if matches!(op, BinaryOperator::And) => {
            distribute_filters(left, table_info, aliases);
            distribute_filters(right, table_info, aliases);
        }
        Expr::BinaryOp { .. } => {
            let involved_tables: HashSet<_> = extract_tables_from_expr(expr, aliases);
            if involved_tables.len() == 1 {
                if let Some(table) = involved_tables.iter().next() {
                    table_info.entry(table.clone()).or_default().1.insert(expr.to_string());
                }
            }
        }
        Expr::Nested(e) => distribute_filters(e, table_info, aliases),
        _ => {}
    }
}

fn extract_tables_from_expr(expr: &Expr, aliases: &HashMap<String, String>) -> HashSet<String> {
    let mut set = HashSet::new();
    match expr {
        Expr::CompoundIdentifier(idents) if idents.len() == 2 => {
            let table = resolve_table_name(&idents[0].value, aliases);
            set.insert(table);
        }
        Expr::BinaryOp { left, right, .. } => {
            set.extend(extract_tables_from_expr(left, aliases));
            set.extend(extract_tables_from_expr(right, aliases));
        }
        Expr::Nested(e) => {
            set.extend(extract_tables_from_expr(e, aliases));
        }
        _ => {}
    }
    set
}

fn process_table_with_joins(table_with_joins: &TableWithJoins, table_info: &mut HashMap<String, (HashSet<String>, HashSet<String>)>, aliases: &mut HashMap<String, String>) {
    process_table_factor(&table_with_joins.relation, table_info, aliases);
    for join in &table_with_joins.joins {
        process_table_factor(&join.relation, table_info, aliases);
    }
}

fn process_table_factor(factor: &TableFactor, table_info: &mut HashMap<String, (HashSet<String>, HashSet<String>)>, aliases: &mut HashMap<String, String>) {
    match factor {
        TableFactor::Table { name, alias, .. } => {
            let real = name.to_string();
            table_info.entry(real.clone()).or_default();
            if let Some(TableAlias { name: alias_name, .. }) = alias {
                aliases.insert(alias_name.value.clone(), real);
            }
        }
        TableFactor::Derived { subquery, alias, .. } => {
            let table = alias.as_ref().map(|a| a.name.to_string()).unwrap_or_else(|| subquery.to_string());
            table_info.entry(table).or_default();
        }
        _ => {}
    }
}

fn resolve_table_name(alias_or_name: &str, aliases: &HashMap<String, String>) -> String {
    aliases.get(alias_or_name).cloned().unwrap_or_else(|| alias_or_name.to_string())
}
