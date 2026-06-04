use arrow::datatypes::Schema;
use datafusion::execution::context::SessionContext;
use datafusion_pg_catalog::{dispatch_query, get_base_session_context, start_server};
use std::fs::File;
use std::io::Read;
use std::io::Write as IoWrite;
use std::path::Path;
use std::sync::{Arc, Mutex};
use zip::write::FileOptions;
use zip::ZipWriter;

#[tokio::test]
async fn test_dispatch_query_public() -> datafusion::error::Result<()> {
    let ctx = SessionContext::new();

    let called = Arc::new(Mutex::new(false));
    let called_clone = called.clone();
    let handler = move |_ctx: &SessionContext, _sql: &str, _p, _t| {
        let called_clone = called_clone.clone();
        async move {
            *called_clone.lock().unwrap() = true;
            Ok((Vec::new(), Arc::new(Schema::empty())))
        }
    };

    dispatch_query(&ctx, "SELECT 1", None, None, handler).await?;
    assert!(*called.lock().unwrap());
    Ok(())
}

#[tokio::test]
async fn test_get_base_session_context_public() -> datafusion::error::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let zip_path = dir.path().join("schema.zip");
    create_zip(zip_path.as_path());
    let _ = get_base_session_context(
        Some(zip_path.to_str().unwrap()),
        "pgtry".to_string(),
        "public".to_string(),
        None,
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn test_get_base_session_context_embedded() -> datafusion::error::Result<()> {
    let _ = get_base_session_context(None, "pgtry".to_string(), "public".to_string(), None).await?;
    Ok(())
}

fn create_zip(path: &Path) {
    let file = File::create(path).unwrap();
    let mut zip = ZipWriter::new(file);
    let options: FileOptions<()> = FileOptions::default();
    for entry in std::fs::read_dir("pg_catalog_data/pg_schema").unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|s| s.to_str()) != Some("yaml") {
            continue;
        }
        let mut contents = String::new();
        File::open(&path)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        zip.start_file(path.file_name().unwrap().to_str().unwrap(), options)
            .unwrap();
        zip.write_all(contents.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

#[test]
fn test_start_server_public() {
    let _f = start_server;
}
