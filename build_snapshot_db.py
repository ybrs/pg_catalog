"""Precompute the catalog view snapshots into a DuckDB file for fast test loads.

The snapshot regression test (`tests/test_view_output_snapshot.py`) needs, for every
view, the expected rows and the defining SQL captured from real PostgreSQL. Those live
in the per-table YAML under `pg_catalog_data/pg_schema/`, but re-parsing all ~200 files
on every test run costs ~11s (even with libyaml). This script reads them ONCE and writes
the (name, view_sql, rows) triples into a single DuckDB file, which the test loads in a
fraction of a second.

Run it after the YAML catalog changes - the same point in the pipeline as
`gen_schema_ipc` (see `regenerate-catalog.sh`):

    python build_snapshot_db.py

The rows are stored as a JSON string per view (the snapshot rows are heterogeneous
dicts), so the test deserializes them back into the same list-of-dicts it used to read
straight from YAML.
"""

import glob
import json
import os

import duckdb

from yaml_loader import find_in_doc, load_yaml

SCHEMA_DIR = "pg_catalog_data/pg_schema"
DB_PATH = "pg_catalog_data/view_snapshots.duckdb"


def build(db_path=DB_PATH, schema_dir=SCHEMA_DIR):
    """Read every view's snapshot out of the YAML in `schema_dir` and write them to a
    fresh DuckDB file at `db_path`. Returns the path written."""
    snapshots = []
    for path in sorted(glob.glob(f"{schema_dir}/*.yaml")):
        doc = load_yaml(path)
        view_sql = find_in_doc(doc, "view_sql")
        rows = find_in_doc(doc, "rows")
        if not view_sql or rows is None:
            continue
        name = path.split("/")[-1].replace(".yaml", "").replace("__", ".")
        snapshots.append((name, view_sql, json.dumps(rows)))

    # Write to a temp path then replace, so a reader never sees a half-built file.
    tmp_path = f"{db_path}.tmp"
    if os.path.exists(tmp_path):
        os.remove(tmp_path)
    con = duckdb.connect(tmp_path)
    con.execute(
        "CREATE TABLE view_snapshots (name VARCHAR, view_sql VARCHAR, rows_json VARCHAR)"
    )
    con.executemany("INSERT INTO view_snapshots VALUES (?, ?, ?)", snapshots)
    con.close()
    os.replace(tmp_path, db_path)
    return db_path, len(snapshots)


if __name__ == "__main__":
    written, count = build()
    print(f"wrote {written} ({count} views)")
