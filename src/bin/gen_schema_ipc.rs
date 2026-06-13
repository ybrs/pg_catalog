//! Regenerate the embedded IPC catalog artifact from the YAML schema zip.
//!
//! Run after the YAML catalog changes (e.g. after `make create_schema_zip`):
//!
//! ```bash
//! cargo run --release --bin gen_schema_ipc
//! ```
//!
//! It reads `pg_catalog_data/postgres-schema-nightly.zip` (the human-editable
//! YAML source) and writes `pg_catalog_data/postgres-schema-nightly-ipc.zip`
//! (the fast, Arrow-IPC artifact embedded into the binary and loaded at startup).

use std::fs;

fn main() {
    let yaml_zip = "pg_catalog_data/postgres-schema-nightly.zip";
    let out = "pg_catalog_data/postgres-schema-nightly-ipc.zip";

    let yaml_bytes = fs::read(yaml_zip)
        .unwrap_or_else(|e| panic!("failed to read {yaml_zip}: {e}"));
    let ipc = datafusion_pg_catalog::build_ipc_artifact(&yaml_bytes);
    fs::write(out, &ipc).unwrap_or_else(|e| panic!("failed to write {out}: {e}"));

    println!(
        "wrote {out} ({} KiB) from {yaml_zip} ({} KiB)",
        ipc.len() / 1024,
        yaml_bytes.len() / 1024,
    );
}
