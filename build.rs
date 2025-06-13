use std::env;
use std::fs::{self, File};
use std::path::Path;
use std::process::Command;

fn main() {
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR env var not set");
    let schema_dir = Path::new(&out_dir).join("pg_schema");
    fs::create_dir_all(&schema_dir).expect("failed to create schema dir");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let local_dir = Path::new(&manifest_dir).join("pg_catalog_data/pg_schema");

    let marker = schema_dir.join("pg_class.yaml");
    if !marker.exists() {
        if local_dir.exists() {
            copy_dir(&local_dir, &schema_dir).expect("failed to copy local schema");
        } else {
            let zip_path = Path::new(&out_dir).join("postgres-schema-nightly.zip");
            if !zip_path.exists() {
                if let Err(e) = download_zip(&zip_path) {
                    panic!("failed to download schema: {}", e);
                }
            }
            unzip_schema(&zip_path, &schema_dir);
        }
    }
    println!("cargo:rustc-env=DEFAULT_SCHEMA_DIR={}", schema_dir.display());
    println!("cargo:rerun-if-changed=build.rs");
}

fn download_zip(zip_path: &Path) -> Result<(), String> {
    let url = "https://github.com/ybrs/pg_catalog/releases/download/schema-nightly-15632378697/postgres-schema-nightly.zip";
    let status = Command::new("curl")
        .arg("-L")
        .arg("-o")
        .arg(zip_path)
        .arg(url)
        .status()
        .map_err(|e| format!("failed to execute curl: {}", e))?;
    if !status.success() {
        return Err(format!("curl failed with status: {}", status));
    }
    Ok(())
}

fn unzip_schema(zip_path: &Path, dest: &Path) {
    let file = File::open(zip_path).expect("failed to open zip file");
    let mut archive = zip::ZipArchive::new(file).expect("invalid zip archive");
    archive.extract(dest).expect("failed to extract zip");
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            let file_name = path.file_name().unwrap();
            fs::copy(&path, dst.join(file_name))?;
        }
    }
    Ok(())
}

