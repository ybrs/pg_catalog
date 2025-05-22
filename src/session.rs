use arrow::array::{Int32Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};

use datafusion::catalog::memory::{MemoryCatalogProvider, MemorySchemaProvider};
use datafusion::catalog::SchemaProvider;
use datafusion::datasource::MemTable;
use datafusion::execution::context::SessionContext;
use arrow::record_batch::RecordBatch;

use serde::Deserialize;
use serde_yaml;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use pgwire::api::Type;
use crate::clean_duplicate_columns::alias_all_columns;
use crate::replace::{regclass_udfs, replace_regclass, replace_set_command_with_namespace, rewrite_pg_custom_operator, rewrite_regtype_cast, rewrite_schema_qualified_text, strip_default_collate};
use crate::scalar_to_cte::rewrite_subquery_as_cte;
use bytes::Bytes;

use datafusion::scalar::ScalarValue;
use crate::user_functions::{register_scalar_format_type, register_scalar_pg_tablespace_location, register_scalar_regclass_oid};
use datafusion::common::{config_err, config::ConfigEntry};
use datafusion::common::config::{ConfigExtension, ExtensionOptions};
use crate::db_table::{map_pg_type, ObservableMemTable, ScanTrace};


#[derive(Default, Clone, Debug)]
pub struct ClientOpts {
    pub application_name: String,
}


impl ConfigExtension for ClientOpts {
    const PREFIX: &'static str = "pg_catalog";
}

impl ExtensionOptions for ClientOpts {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }
    fn cloned(&self) -> Box<dyn ExtensionOptions> { Box::new(self.clone()) }

    fn set(&mut self, key: &str, value: &str) -> datafusion::error::Result<()> {
        
        println!("set key {:?}", key);
        match key {
            "application_name" => {
                self.application_name = value.to_string();
                println!("value is set!!!");
                Ok(())
            }
            "extra_float_digits" => {
                Ok(())
            }
            _ => config_err!("unknown key {key}"),
        }
    }

    fn entries(&self) -> Vec<ConfigEntry> {
        vec![ConfigEntry {
            // key: format!("{}.application_name", Self::PREFIX),
            key: "application_name".to_string(),
            value: Some(self.application_name.clone()),
            description: "",
        }]
    }

}


#[derive(Debug, Deserialize)]
struct TableDef {
    schema: BTreeMap<String, String>,
    rows: Option<Vec<BTreeMap<String, serde_json::Value>>>,
}

#[derive(Debug, Deserialize)]
struct YamlSchema(HashMap<String, HashMap<String, HashMap<String, TableDef>>>);



fn rename_columns(batch: &RecordBatch, name_map: &HashMap<String, String>) -> RecordBatch {
    let new_fields = batch
        .schema()
        .fields()
        .iter()
        .map(|old_field| {
            let new_name = name_map
                .get(old_field.name())
                .map(|s| s.as_str())
                .unwrap_or_else(|| old_field.name().as_str());
            Field::new(new_name, old_field.data_type().clone(), old_field.is_nullable())
        })
        .collect::<Vec<_>>();

    let new_schema = std::sync::Arc::new(Schema::new(new_fields));
    RecordBatch::try_new(new_schema, batch.columns().to_vec()).unwrap()
}



pub fn print_params(params: &Vec<Option<Bytes>>) {
    for (i, param) in params.iter().enumerate() {
        match param {
            Some(bytes) => {
                match bytes.len() {
                    4 => {
                        let v = u32::from_be_bytes(bytes[..4].try_into().unwrap());
                        println!("param[{}] as u32: {}", i, v);
                    }
                    8 => {
                        let v = u64::from_be_bytes(bytes[..8].try_into().unwrap());
                        println!("param[{}] as u64: {}", i, v);
                    }
                    _ => {
                        println!("param[{}] raw bytes ({} bytes): {:?}", i, bytes.len(), bytes);
                    }
                }
            }
            None => {
                println!("param[{}] is NULL", i);
            }
        }
    }
}


pub async fn execute_sql(
    ctx: &SessionContext,
    sql: &str,
    vec: Option<Vec<Option<Bytes>>>,
    vec0: Option<Vec<Type>>,
) -> datafusion::error::Result<(Vec<RecordBatch>, Arc<Schema>)> {
    let sql = replace_set_command_with_namespace(&sql)?;
    let sql = strip_default_collate(&sql)?;
    let sql = rewrite_pg_custom_operator(&sql)?;
    let sql = rewrite_schema_qualified_text(&sql)?;
    let sql = replace_regclass(&sql)?;
    let sql = rewrite_regtype_cast(&sql)?;
    let (sql, aliases) = alias_all_columns(&sql)?;
    let sql = rewrite_subquery_as_cte(&sql);
    
    let df = if let (Some(params), Some(types)) = (vec, vec0) {
        println!("params {:?}", params);
        print_params(&params);

        let mut scalars = Vec::new();

        for (param, typ) in params.into_iter().zip(types.into_iter()) {
            let value = match (param, typ) {
                (Some(bytes), Type::INT2) => {
                    let v = i16::from_be_bytes(bytes[..].try_into().unwrap());
                    ScalarValue::Int16(Some(v))
                }
                (Some(bytes), Type::INT8) => {
                    let v = i64::from_be_bytes(bytes[..].try_into().unwrap());
                    ScalarValue::Int64(Some(v))
                }
                (Some(bytes), Type::INT4) => {
                    let v = i32::from_be_bytes(bytes[..].try_into().unwrap());
                    ScalarValue::Int32(Some(v))
                }
                (Some(bytes), Type::VARCHAR)
                | (Some(bytes), Type::TEXT)
                | (Some(bytes), Type::BPCHAR)
                | (Some(bytes), Type::NAME)
                | (Some(bytes), Type::UNKNOWN) => {
                    let s = String::from_utf8(bytes.to_vec()).unwrap();
                    ScalarValue::Utf8(Some(s))
                }
                (None, Type::INT2) => ScalarValue::Int16(None),
                (None, Type::INT8) => ScalarValue::Int64(None),
                (None, Type::INT4) => ScalarValue::Int32(None),
                (None, Type::VARCHAR)
                | (None, Type::TEXT)
                | (None, Type::BPCHAR)
                | (None, Type::NAME)
                | (None, Type::UNKNOWN) => ScalarValue::Utf8(None),
                (some, other_type) => {
                    panic!("unsupported param {:?} type {:?}", some, other_type);
                }
            };
            scalars.push(value);
        }

        let df = ctx.sql(&sql).await?.with_param_values(scalars)?;
        df
    } else {
        println!("final sql {:?}", sql);    
        let df = ctx.sql(&sql).await?;
        println!("executed sql");
        df
    };

    // TODO: fix scope
    let original_schema = df.schema();
    let renamed_fields = original_schema
        .fields()
        .iter()
        .map(|f| {
            let new_name = aliases
                .get(f.name())
                .map(|s| s.as_str())
                .unwrap_or_else(|| f.name().as_str());
            Field::new(new_name, f.data_type().clone(), f.is_nullable())
        })
        .collect::<Vec<_>>();
    let schema = Arc::new(Schema::new(renamed_fields));

    let results = df.collect().await?;
    let results = results
        .iter()
        .map(|batch| rename_columns(batch, &aliases))
        .collect();

    Ok((results, schema))


}


fn parse_schema(schema_path: &str) -> HashMap<String, HashMap<String, HashMap<String, (SchemaRef, Vec<RecordBatch>)>>> {
    if Path::new(schema_path).is_file() {
        parse_schema_file(schema_path)
    } else if Path::new(schema_path).is_dir() {
        parse_schema_dir(schema_path)
    } else {
        panic!("schema_path {} is neither a file nor a directory", schema_path);
    }
}

fn parse_schema_file(path: &str) -> HashMap<String, HashMap<String, HashMap<String, (SchemaRef, Vec<RecordBatch>)>>> {
    let contents = fs::read_to_string(path).expect("Failed to read schema file");
    let parsed: YamlSchema = serde_yaml::from_str(&contents).expect("Invalid YAML");
    parsed.0.into_iter()
        .map(|(catalog, schemas)| {
            let schemas = schemas.into_iter().map(|(schema, tables)| {
                let tables = tables.into_iter().map(|(table, def)| {
                    let (schema_ref, batches) = build_table(def);
                    (table, (schema_ref, batches))
                }).collect();
                (schema, tables)
            }).collect();
            (catalog, schemas)
        }).collect()
}

fn parse_schema_dir(
    dir_path: &str,
) -> HashMap<String, HashMap<String, HashMap<String, (SchemaRef, Vec<RecordBatch>)>>> {
    let mut all = HashMap::new();

    for entry in fs::read_dir(dir_path).expect("Failed to read directory") {
        let path = entry.expect("Invalid dir entry").path();
        if path.extension().and_then(|s| s.to_str()) == Some("yaml") {
            let mut partial = parse_schema_file(path.to_str().unwrap());

            for schemas in partial.values_mut() {
                schemas.retain(|name, _| name != "information_schema");
            }

            merge_schema_maps(&mut all, partial);
        }
    }

    all
}

fn merge_schema_maps(
    target: &mut HashMap<String, HashMap<String, HashMap<String, (SchemaRef, Vec<RecordBatch>)>>>,
    other: HashMap<String, HashMap<String, HashMap<String, (SchemaRef, Vec<RecordBatch>)>>>,
) {
    for (catalog, schemas) in other {
        let catalog_entry = target.entry(catalog).or_insert_with(HashMap::new);
        for (schema, tables) in schemas {
            let schema_entry = catalog_entry.entry(schema).or_insert_with(HashMap::new);
            schema_entry.extend(tables);
        }
    }
}

fn build_table(def: TableDef) -> (SchemaRef, Vec<RecordBatch>) {
    let fields: Vec<Field> = def.schema.iter()
        .map(|(col, typ)| Field::new(col, map_pg_type(typ), true))
        .collect();

    let schema_ref = Arc::new(Schema::new(fields.clone()));

    let record_batches = if let Some(rows) = def.rows {
        let mut cols: Vec<Vec<serde_json::Value>> = vec![vec![]; fields.len()];
        for row in rows {
            for (i, field) in fields.iter().enumerate() {
                cols[i].push(row.get(field.name()).cloned().unwrap_or(serde_json::Value::Null));
            }
        }

        let arrays = fields.iter().zip(cols.into_iter())
            .map(|(field, col_data)| {
                use arrow::array::*;
                use arrow::datatypes::DataType;
                let array: ArrayRef = match field.data_type() {
                    DataType::Utf8 => Arc::new(StringArray::from(
                        col_data.into_iter()
                            .map(|v| v.as_str().map(|s| s.to_string()))
                            .collect::<Vec<_>>()
                    )),
                    DataType::Int32 => Arc::new(Int32Array::from(
                        col_data.into_iter()
                            .map(|v| v.as_i64().map(|i| i as i32))
                            .collect::<Vec<_>>()
                    )),
                    DataType::Int64 => Arc::new(Int64Array::from(
                        col_data.into_iter()
                            .map(|v| v.as_i64())
                            .collect::<Vec<_>>()
                    )),
                    DataType::Boolean => Arc::new(BooleanArray::from(
                        col_data.into_iter()
                            .map(|v| v.as_bool())
                            .collect::<Vec<_>>()
                    )),
                    DataType::List(inner) if inner.data_type() == &DataType::Utf8 => {
                        let mut builder = ListBuilder::new(StringBuilder::new());
                        for v in col_data {
                            if let Some(items) = v.as_array() {
                                for item in items {
                                    match item.as_str() {
                                        Some(s) => builder.values().append_value(s),
                                        None    => builder.values().append_null(),
                                    }
                                }
                                builder.append(true);
                            } else if v.is_null() {
                                builder.append(false);
                            } else {
                                builder.values().append_value(v.to_string());
                                builder.append(true);
                            }
                        }
                        Arc::new(builder.finish())
                    },

                    _ => Arc::new(StringArray::from(
                        col_data.into_iter()
                            .map(|v| Some(v.to_string()))
                            .collect::<Vec<_>>()
                    )),
                };
                array
            })
            .collect::<Vec<_>>();

        vec![RecordBatch::try_new(schema_ref.clone(), arrays).unwrap()]
    } else {
        vec![RecordBatch::new_empty(schema_ref.clone())]
    };

    (schema_ref, record_batches)
}

pub async fn get_base_session_context(schema_path: &String, default_catalog:String, default_schema:String) -> datafusion::error::Result<(SessionContext, Arc<Mutex<Vec<ScanTrace>>>)> {
    let log: Arc<Mutex<Vec<ScanTrace>>> = Arc::new(Mutex::new(Vec::new()));

    let schemas = parse_schema(schema_path.as_str());

    let mut config = datafusion::execution::context::SessionConfig::new()
        .with_default_catalog_and_schema(&default_catalog, &default_schema)
        .with_option_extension(ClientOpts::default());

    config.options_mut().catalog.information_schema = true;

    let ctx = SessionContext::new_with_config(config);

    for (catalog_name, schemas) in schemas {

        let current_catalog = if catalog_name == "public" {
            default_catalog.to_string()
        } else {
            catalog_name
        };

        let catalog_provider = if let Some(catalog_provider) = ctx.catalog(&current_catalog){
            catalog_provider
        } else {
            let catalog_provider = Arc::new(MemoryCatalogProvider::new());
            ctx.register_catalog(&current_catalog, catalog_provider.clone());
            catalog_provider
        };

        for (schema_name, tables) in schemas {

            let schema_provider = if let Some(schema_provider) = catalog_provider.schema(&schema_name) {
                schema_provider
            } else {
                Arc::new(MemorySchemaProvider::new())
            };

            let _ = catalog_provider.register_schema(&schema_name, schema_provider.clone());
            println!("catalog/database: {:?} schema: {:?}", current_catalog, schema_name);

            for (table, (schema_ref, batches)) in tables {
                println!("-- table {:?}", &table);

                let wrapped = ObservableMemTable::new(table.clone(), schema_ref, log.clone(), batches);
                schema_provider.register_table(table, Arc::new(wrapped))?;
            }
        }
    }

    for f in regclass_udfs(&ctx) {
        ctx.register_udf(f);
    }

    ctx.register_udtf("regclass_oid", Arc::new(crate::user_functions::RegClassOidFunc));

    register_scalar_regclass_oid(&ctx)?;
    register_scalar_pg_tablespace_location(&ctx)?;
    register_scalar_format_type(&ctx)?;

    let catalogs = ctx.catalog_names();
    println!("registered catalogs: {:?}", catalogs);

    println!("Current catalog: {}", default_catalog);


    // register additional databases    
    if let Some(catalog) = ctx.catalog("crm") {
        let schema_provider = Arc::new(MemorySchemaProvider::new());
        catalog.register_schema("crm", schema_provider.clone())?;
        
        let table_schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("name", DataType::Utf8, false),
        ]));

        // Create a record batch
        let batch = RecordBatch::try_new(
            table_schema.clone(),
            vec![
                Arc::new(Int32Array::from(vec![1, 2, 3])),
                Arc::new(StringArray::from(vec!["Alice", "Bob", "Charlie"])),
            ],
        )?;        

        let table = MemTable::try_new(table_schema, vec![vec![batch]])?;

        schema_provider.register_table("users".to_string(), Arc::new(table))?;
    }
    // TODO: how to add a new db
    // TODO: how to add new columns in pg_catalog
    // ctx.sql("INSERT INTO pg_catalog.pg_class (relname, relnamespace, relkind, reltuples, reltype) VALUES ('users', 'crm', 'r', 3, 0);").await?;


    Ok((ctx, log))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use std::io::Write;
    use arrow::array::{Int32Array, StringArray};
    use arrow::datatypes::DataType;
    use arrow::array::{ArrayRef};

    #[test]
    fn test_parse_schema_file() {
        let yaml = r#"
public:
  myschema:
    employees:
      type: table
      schema:
        id: int
        name: varchar
      rows:
        - id: 1
          name: Alice
        - id: 2
          name: Bob
"#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", yaml).unwrap();

        let parsed = parse_schema_file(file.path().to_str().unwrap());

        let myschema = parsed.get("public").unwrap().get("myschema").unwrap();
        let (schema_ref, batches) = myschema.get("employees").unwrap();

        let fields = schema_ref.fields();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name(), "id");
        assert_eq!(fields[0].data_type(), &DataType::Int32);
        assert_eq!(fields[1].name(), "name");
        assert_eq!(fields[1].data_type(), &DataType::Utf8);

        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        assert_eq!(batch.num_rows(), 2);

        let id_array = batch.column(0).as_any().downcast_ref::<Int32Array>().unwrap();
        assert_eq!(id_array.value(0), 1);
        assert_eq!(id_array.value(1), 2);

        let name_array = batch.column(1).as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(name_array.value(0), "Alice");
        assert_eq!(name_array.value(1), "Bob");
    }

    #[test]
    fn test_rename_columns_all() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Utf8, true),
        ]));

        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int32Array::from(vec![1, 2])) as ArrayRef,
                Arc::new(StringArray::from(vec!["x", "y"])) as ArrayRef,
            ],
        )
        .unwrap();

        let mut map = HashMap::new();
        map.insert("a".to_string(), "alpha".to_string());
        map.insert("b".to_string(), "beta".to_string());

        let renamed = rename_columns(&batch, &map);

        assert_eq!(renamed.schema().field(0).name(), "alpha");
        assert_eq!(renamed.schema().field(1).name(), "beta");

        assert!(Arc::ptr_eq(batch.column(0), renamed.column(0)));
        assert!(Arc::ptr_eq(batch.column(1), renamed.column(1)));
    }


    #[test]
    fn test_parse_schema_text_array() {
        use arrow::array::{ListArray};
        let yaml = r#"
public:
  myschema:
    cfgtable:
      type: table
      schema:
        cfg: _text
      rows:
        - cfg:
            - "x"
            - "y"
"#;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, yaml.as_bytes()).unwrap();

        let parsed = parse_schema_file(file.path().to_str().unwrap());
        let myschema = parsed.get("public").unwrap().get("myschema").unwrap();
        let (schema_ref, batches) = myschema.get("cfgtable").unwrap();

        let field = &schema_ref.fields()[0];
        assert!(matches!(field.data_type(), DataType::List(_)));

        let batch = &batches[0];
        let list = batch.column(0).as_any().downcast_ref::<ListArray>().unwrap();
        let binding = list.value(0);
        let inner = binding.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(inner.value(0), "x");
        assert_eq!(inner.value(1), "y");
    }


    #[test]
    fn test_rename_columns_partial() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("name", DataType::Utf8, false),
        ]));

        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int32Array::from(vec![1, 2, 3])) as ArrayRef,
                Arc::new(StringArray::from(vec!["Alice", "Bob", "Carol"])) as ArrayRef,
            ],
        )
        .unwrap();

        let mut map = HashMap::new();
        map.insert("name".to_string(), "username".to_string());

        let renamed = rename_columns(&batch, &map);

        assert_eq!(renamed.schema().field(0).name(), "id");
        assert_eq!(renamed.schema().field(1).name(), "username");

        assert!(Arc::ptr_eq(batch.column(0), renamed.column(0)));
        assert!(Arc::ptr_eq(batch.column(1), renamed.column(1)));
    }
}

