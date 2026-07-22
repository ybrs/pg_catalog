"""Keep the `regproc` column list in the SQL rewriter in step with the catalog dump.

`src/replace.rs` carries `REGPROC_COLUMN_NAMES`, the columns whose values are function
names rather than OIDs, and rewrites comparisons against those columns into `pg_proc`
lookups. The rewriter cannot read the catalog's `pg_types` metadata at query time - the
shipped Arrow IPC artifact keeps only the resolved Arrow types - so the list is written
out by hand. This test regenerates it from `pg_catalog_data/pg_schema` so a catalog
refresh that adds or drops a `regproc` column fails here instead of silently leaving
that column unresolvable.
"""

import glob
import os
import re

from yaml_loader import load_yaml, walk_catalog_objects

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCHEMA_DIR = os.path.join(REPO_ROOT, "pg_catalog_data", "pg_schema")
REWRITER_SOURCE = os.path.join(REPO_ROOT, "src", "replace.rs")


def regproc_columns_in_catalog():
    """The set of column names the catalog dump declares as `pg_types: regproc`."""
    columns = set()
    for path in glob.glob(os.path.join(SCHEMA_DIR, "*.yaml")):
        doc = load_yaml(path)
        for _schema, _name, node in walk_catalog_objects(doc):
            pg_types = node.get("pg_types") or {}
            columns.update(
                column for column, pg_type in pg_types.items() if pg_type == "regproc"
            )
    return columns


def regproc_columns_in_rewriter():
    """The set of column names listed in `REGPROC_COLUMN_NAMES` in `src/replace.rs`."""
    source = open(REWRITER_SOURCE).read()
    body = re.search(
        r"REGPROC_COLUMN_NAMES: &\[&str\] = &\[(.*?)\];", source, re.DOTALL
    )
    assert body, "REGPROC_COLUMN_NAMES not found in src/replace.rs"
    return set(re.findall(r'"([^"]+)"', body.group(1)))


def test_rewriter_lists_every_regproc_column():
    catalog = regproc_columns_in_catalog()
    assert catalog, "no regproc columns found in the catalog dump"
    assert regproc_columns_in_rewriter() == catalog
