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

#[tokio::test(flavor = "multi_thread")]
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

#[tokio::test(flavor = "multi_thread")]
async fn test_get_base_session_context_public() -> datafusion::error::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let zip_path = dir.path().join("schema.zip");
    create_zip(zip_path.as_path());
    let _ = get_base_session_context(
        Some(zip_path.to_str().unwrap()),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_get_base_session_context_embedded() -> datafusion::error::Result<()> {
    // `None` loads the embedded postgres-schema-nightly.zip (the path riffq uses).
    let (ctx, _log) =
        get_base_session_context(None, "pgtry".to_string(), "public".to_string()).await?;

    // The embedded zip must carry the float-precision fix: pg_class.reltuples is
    // declared float4 and must materialize as Float32 (FLOAT4 / OID 700 on the
    // wire), not Float32->NULL or Float64. This guards the embedded artifact, not
    // just the on-disk YAML directory.
    let batches = ctx
        .sql("SELECT reltuples FROM pg_catalog.pg_class LIMIT 1")
        .await?
        .collect()
        .await?;
    assert_eq!(
        batches[0].schema().field(0).data_type(),
        &arrow::datatypes::DataType::Float32,
        "embedded zip must map float4 -> Float32 for pg_class.reltuples"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_views_registered_as_view() -> datafusion::error::Result<()> {
    let (ctx, _log) = get_base_session_context(
        Some("pg_catalog_data/pg_schema"),
        "pgtry".to_string(),
        "public".to_string(),
    )
    .await?;

    let df = ctx
        .sql("SELECT viewname FROM pg_catalog.pg_views WHERE viewname = 'pg_views'")
        .await?;
    let batches = df.collect().await?;
    let total_rows: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert!(
        total_rows >= 1,
        "expected pg_views view to return at least one row"
    );

    let catalog = ctx.catalog("pgtry").expect("catalog should exist");
    let schema = catalog
        .schema("pg_catalog")
        .expect("pg_catalog schema should exist");
    let provider = schema
        .table("pg_views")
        .await?
        .expect("pg_views table should be registered");
    // datafusion 54 removed `as_any` from `TableProvider`; a registered view
    // reports `TableType::View`, whereas our `ScanRecordingMemTable` reports `Base`.
    assert_eq!(
        provider.table_type(),
        datafusion::datasource::TableType::View,
        "pg_views should be registered as a view, not an ScanRecordingMemTable"
    );
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
