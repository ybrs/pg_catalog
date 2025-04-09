use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::datasource::MemTable;
use datafusion::execution::context::SessionContext;
use datafusion::logical_expr::{Expr, LogicalPlan, TableScan};
use datafusion::common::TableReference;

use regex::Regex;
use serde::Deserialize;
use serde_yaml;
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
struct YamlSchema(HashMap<String, BTreeMap<String, String>>);

fn map_pg_type(pg_type: &str) -> DataType {
    let re_varchar = Regex::new(r"^varchar\\(\\d+\\)$").unwrap();
    match pg_type.to_lowercase().as_str() {
        "uuid" => DataType::Utf8,
        "int" | "integer" => DataType::Int32,
        "bigint" => DataType::Int64,
        "bool" | "boolean" => DataType::Boolean,
        other if re_varchar.is_match(other) => DataType::Utf8,
        _ => DataType::Utf8,
    }
}

fn parse_schema(yaml_path: &str) -> HashMap<String, SchemaRef> {
    let contents = fs::read_to_string(yaml_path).expect("Failed to read schema.yaml");
    let parsed: YamlSchema = serde_yaml::from_str(&contents).expect("Invalid YAML schema");

    parsed
        .0
        .into_iter()
        .map(|(table, columns)| {
            let fields: Vec<Field> = columns
                .into_iter()
                .map(|(col, typ)| Field::new(&col, map_pg_type(&typ), true))
                .collect();
            (table, Arc::new(Schema::new(fields)))
        })
        .collect()
}

fn collect_info_from_plan(plan: &LogicalPlan) -> serde_json::Value {
    let mut table_info: HashMap<String, (Vec<String>, Vec<String>, BTreeMap<String, String>)> = HashMap::new();

    fn walk(plan: &LogicalPlan, table_info: &mut HashMap<String, (Vec<String>, Vec<String>, BTreeMap<String, String>)>) {
        match plan {
            LogicalPlan::Projection(p) => {
                for e in &p.expr {
                    if let Expr::Column(col) = e {

                        let table = match &col.relation {
                            Some(TableReference::Bare { table }) => table.clone(),
                            Some(TableReference::Partial { table, .. }) => table.clone(),
                            Some(TableReference::Full { table, .. }) => table.clone(),
                            _ => Arc::from("<unknown>"),
                        };
                        table_info.entry(table.to_string()).or_default().0.push(col.name.clone());
                    }
                }
                walk(&p.input, table_info);
            }
            LogicalPlan::Filter(f) => {
                let mut exprs = vec![f.predicate.to_string()];
                walk(&f.input, table_info);
                if let LogicalPlan::TableScan(scan) = f.input.as_ref() {
                    let table = &scan.table_name;
                    table_info.entry(table.to_string()).or_default().1.append(&mut exprs);
                }
            }
            LogicalPlan::TableScan(scan) => {
                let schema = scan
                    .source
                    .schema()
                    .fields()
                    .iter()
                    .map(|f| (f.name().clone(), f.data_type().to_string()))
                    .collect::<BTreeMap<_, _>>();
                table_info.entry(scan.table_name.to_string()).or_default().2 = schema;
            }
            _ => {
                for child in plan.inputs() {
                    walk(child, table_info);
                }
            }
        }
    }

    walk(plan, &mut table_info);

    let out: Vec<_> = table_info
        .into_iter()
        .map(|(table, (columns, filters, types))| {
            serde_json::json!({
                "table": table,
                "columns": columns,
                "filters": filters,
                "types": types
            })
        })
        .collect();
    serde_json::Value::Array(out)
}

#[tokio::main]
async fn main() -> datafusion::error::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} schema.yaml \"SQL_QUERY\" [--show-plan]", args[0]);
        std::process::exit(1);
    }
    let schema_path = &args[1];
    let sql = &args[2];
    let show_plan = args.iter().any(|x| x == "--show-plan");

    let schemas = parse_schema(schema_path);
    let ctx = SessionContext::new();

    for (table, schema) in schemas.iter() {
        let empty = MemTable::try_new(schema.clone(), vec![])?;
        ctx.register_table(table, Arc::new(empty))?;
    }

    let df = ctx.sql(sql).await?;
    let plan = df.logical_plan();

    if show_plan {
        println!("{:?}", plan);
    }

    let json = collect_info_from_plan(&plan);
    println!("{}", serde_json::to_string_pretty(&json).unwrap());

    Ok(())
}
