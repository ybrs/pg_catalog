#[test]
fn test_default_schema_dir_exists() {
    let path = datafusion_pg_catalog::DEFAULT_SCHEMA_DIR;
    let entries: Vec<_> = std::fs::read_dir(path).expect("schema dir missing").collect();
    assert!(!entries.is_empty(), "schema directory should contain files");
}
