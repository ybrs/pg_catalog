use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write, Cursor};
use std::path::Path;

use arrow::datatypes::SchemaRef;
use arrow::record_batch::RecordBatch;
use arrow::ipc::writer::StreamWriter;
use arrow::ipc::reader::StreamReader;

pub type CatalogData = HashMap<String, HashMap<String, HashMap<String, (SchemaRef, Vec<RecordBatch>)>>>;

const MAGIC: &[u8] = b"PGCAT\0\1";

pub fn write_binary(path: &Path, catalogs: &CatalogData) -> anyhow::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(MAGIC)?;
    let mut count: u32 = 0;
    for schemas in catalogs.values() {
        for tables in schemas.values() {
            count += tables.len() as u32;
        }
    }
    file.write_all(&count.to_le_bytes())?;
    for (catalog, schemas) in catalogs {
        for (schema, tables) in schemas {
            for (table, (schema_ref, batches)) in tables {
                file.write_all(&[catalog.len() as u8])?;
                file.write_all(catalog.as_bytes())?;
                file.write_all(&[schema.len() as u8])?;
                file.write_all(schema.as_bytes())?;
                file.write_all(&[table.len() as u8])?;
                file.write_all(table.as_bytes())?;

                let mut buf = Vec::new();
                {
                    let mut writer = StreamWriter::try_new(&mut buf, schema_ref)?;
                    for batch in batches {
                        writer.write(batch)?;
                    }
                    writer.finish()?;
                }
                file.write_all(&(buf.len() as u64).to_le_bytes())?;
                file.write_all(&buf)?;
            }
        }
    }
    Ok(())
}

pub fn read_binary(path: &Path) -> anyhow::Result<CatalogData> {
    let mut file = File::open(path)?;
    let mut header = vec![0u8; MAGIC.len()];
    file.read_exact(&mut header)?;
    if header != MAGIC {
        anyhow::bail!("invalid magic header");
    }
    let mut count_buf = [0u8; 4];
    file.read_exact(&mut count_buf)?;
    let count = u32::from_le_bytes(count_buf);
    let mut catalogs: CatalogData = HashMap::new();
    for _ in 0..count {
        let catalog = read_string(&mut file)?;
        let schema = read_string(&mut file)?;
        let table = read_string(&mut file)?;
        let mut len_buf = [0u8;8];
        file.read_exact(&mut len_buf)?;
        let len = u64::from_le_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf)?;
        let mut reader = StreamReader::try_new(Cursor::new(buf), None)?;
        let schema_ref = reader.schema();
        let mut batches = Vec::new();
        for batch in reader {
            batches.push(batch?);
        }
        catalogs
            .entry(catalog)
            .or_insert_with(HashMap::new)
            .entry(schema)
            .or_insert_with(HashMap::new)
            .insert(table, (schema_ref, batches));
    }
    Ok(catalogs)
}

fn read_string<R: Read>(mut r: R) -> anyhow::Result<String> {
    let mut len = [0u8;1];
    r.read_exact(&mut len)?;
    let mut buf = vec![0u8; len[0] as usize];
    r.read_exact(&mut buf)?;
    Ok(String::from_utf8(buf)?)
}
