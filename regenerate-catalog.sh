#!/bin/bash
# One-shot catalog regeneration pipeline.
#
#   download PostgreSQL (this OS/arch)  ->  initdb (UTF8, owner "sysuser") + start
#   ->  extract the catalog to YAML     ->  rebuild the YAML zip
#   ->  rebuild the embedded Arrow IPC fast-load artifact  ->  stop PostgreSQL
#
# Run from the pg_catalog repo root:
#
#     ./regenerate-catalog.sh
#
# Requirements: a Python with `psycopg` (override with PYTHON=/path/to/python),
# `cargo`, and network access to download the PostgreSQL binary on first run.
#
# After it finishes: review the diff, run `cargo test` + `pytest`, then commit
# pg_catalog_data/ (YAMLs + both zips). The owner is pinned to "sysuser" and the
# encoding to UTF8 so the output is identical no matter who/where it runs.

set -euo pipefail

PYTHON="${PYTHON:-python}"
PORT=5434
SCHEMA_DIR="pg_catalog_data/pg_schema"
YAML_ZIP="pg_catalog_data/postgres-schema-nightly.zip"

cleanup() {
  if [ -d postgres-data ]; then
    ./postgres-17/bin/pg_ctl -D postgres-data stop -m fast >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

echo "==> [1/6] download PostgreSQL binary for this OS/arch (if missing)"
if [ ! -x postgres-17/bin/postgres ]; then
  ./download_postgresql.sh
else
  echo "    postgres-17 already present, skipping download"
fi

echo "==> [2/6] init (UTF8, sysuser) + start PostgreSQL on :$PORT"
rm -rf postgres-data pg_log
# run-postgres.sh runs postgres in the foreground; background it for the pipeline.
./run-postgres.sh >/dev/null 2>&1 &

echo "==> [3/6] wait for the server to accept connections"
for _ in $(seq 1 60); do
  if ./postgres-17/bin/pg_isready -h localhost -p "$PORT" -q; then break; fi
  sleep 1
done
./postgres-17/bin/pg_isready -h localhost -p "$PORT"

echo "==> [4/6] extract the catalog into $SCHEMA_DIR"
"$PYTHON" schema.py generate "$SCHEMA_DIR"

echo "==> [4b/6] reapply view fixes the raw extraction can't express"
"$PYTHON" patch_views.py "$SCHEMA_DIR"

echo "==> [5/6] rebuild the YAML zip ($YAML_ZIP)"
# Use Python's zipfile so this works without the `zip` binary (portable).
"$PYTHON" - "$SCHEMA_DIR" "$YAML_ZIP" <<'PY'
import sys, glob, zipfile
schema_dir, out = sys.argv[1], sys.argv[2]
files = sorted(glob.glob(f"{schema_dir}/*.yaml"))
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
    for f in files:
        z.write(f, arcname=f)
print(f"    wrote {out} ({len(files)} files)")
PY

echo "==> [6/7] rebuild the embedded Arrow IPC artifact"
cargo run --release --bin gen_schema_ipc

echo "==> [7/7] rebuild the precomputed test snapshot DuckDB"
"$PYTHON" build_snapshot_db.py

echo
echo "==> done. Regenerated:"
echo "      $SCHEMA_DIR/*.yaml"
echo "      $YAML_ZIP                          (human-editable source)"
echo "      pg_catalog_data/postgres-schema-nightly-ipc.zip  (embedded fast-load)"
echo "      pg_catalog_data/view_snapshots.duckdb            (test snapshot cache, gitignored)"
echo "    Next: review the diff, run 'cargo test' + pytest, then commit."
