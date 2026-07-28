"""Reapply the view fixes the raw PostgreSQL extraction can't express.

`schema.py generate` dumps the catalog verbatim from PostgreSQL. A few views
can't be served as-is by our engine, so this post-processing step rewrites them.
It is idempotent and is run by `regenerate-catalog.sh` after generation.

  1. SRF-backed views whose set-returning function we don't implement, but which
     ship a row snapshot (pg_config, pg_settings, pg_timezone_names, ...), are
     served as base tables: `type: view` -> `system_catalog`, drop `view_sql`.
  2. information_schema.table_constraints: its `nulls_distinct` correlated scalar
     subquery (which DataFusion 54 can't decorrelate inside a CASE) -> the
     constant 'YES' (the default for unique constraints).
  3. information_schema.user_mapping_options: its `LATERAL pg_options_to_table()`
     set-returning function in the FROM clause -> the equivalent projection form
     `(pg_options_to_table(um.umoptions)).option_name`, which our SRF->unnest
     rewrite handles.

Usage:  python patch_views.py pg_catalog_data/pg_schema
"""
import re
import sys

import yaml

from yaml_loader import load_yaml

# Views served from their captured row snapshot as a base table rather than from
# their (unsupported) set-returning-function definition.
FLIP_TO_TABLE = {
    "pg_catalog__pg_config",
    "pg_catalog__pg_settings",
    "pg_catalog__pg_available_extensions",
    "pg_catalog__pg_hba_file_rules",
    "pg_catalog__pg_ident_file_mappings",
    "pg_catalog__pg_timezone_abbrevs",
    "pg_catalog__pg_timezone_names",
}


def _view_node(doc):
    """The innermost dict holding `type`/`view_sql` for the single table in `doc`."""
    if isinstance(doc, dict):
        if "type" in doc or "view_sql" in doc:
            return doc
        for value in doc.values():
            found = _view_node(value)
            if found is not None:
                return found
    return None


def _load(path):
    return load_yaml(path)


def _dump(doc, path):
    yaml.safe_dump(doc, open(path, "w"), default_flow_style=False, sort_keys=False, width=10000)


def _rewrite_view_sql(path, transform):
    doc = _load(path)
    node = _view_node(doc)
    if node and node.get("view_sql"):
        new_sql = transform(node["view_sql"])
        if new_sql != node["view_sql"]:
            node["view_sql"] = new_sql
            _dump(doc, path)
            return True
    return False


def patch(schema_dir):
    """Apply every view fix to the generated catalog under `schema_dir`."""
    patched = []

    for stem in FLIP_TO_TABLE:
        path = f"{schema_dir}/{stem}.yaml"
        doc = _load(path)
        node = _view_node(doc)
        if node and node.get("type") == "view":
            node["type"] = "system_catalog"
            node.pop("view_sql", None)
            _dump(doc, path)
            patched.append(stem + " (view->table)")

    # table_constraints: replace the correlated nulls_distinct subquery CASE.
    if _rewrite_view_sql(
        f"{schema_dir}/information_schema__table_constraints.yaml",
        lambda s: re.sub(
            r"CASE\s+WHEN \( SELECT NOT pg_index\.indnullsnotdistinct\s+FROM pg_index"
            r"\s+WHERE pg_index\.indexrelid = c\.conindid\) THEN 'YES'::text"
            r"\s+ELSE 'NO'::text\s+END",
            "'YES'::text",
            s,
        ),
    ):
        patched.append("table_constraints (subquery->constant)")

    # user_mapping_options: LATERAL SRF -> projection form.
    def _ump(sql):
        sql = sql.replace("opts.option_name", "(pg_options_to_table(um.umoptions)).option_name")
        sql = sql.replace("opts.option_value", "(pg_options_to_table(um.umoptions)).option_value")
        return re.sub(
            r",\s*LATERAL pg_options_to_table\(um\.umoptions\) opts\(option_name, option_value\)",
            "",
            sql,
        )

    if _rewrite_view_sql(f"{schema_dir}/information_schema__user_mapping_options.yaml", _ump):
        patched.append("user_mapping_options (LATERAL->projection)")

    return patched


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <schema_dir>", file=sys.stderr)
        sys.exit(1)
    done = patch(sys.argv[1])
    print(f"patched {len(done)} views:")
    for item in done:
        print(f"  - {item}")
