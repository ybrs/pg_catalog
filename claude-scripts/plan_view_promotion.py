"""Compute the facts needed to promote materialized 'views' into real views.

Many objects the catalog declares ``type: view`` are served as frozen ``MemTable``
snapshots, not as live views (see ``audit_catalog_objects.py``). Turning one into a
real view means listing it in ``VIEW_ONLY_TABLES`` (``src/session.rs``) so the
server registers it with ``CREATE VIEW``. Whether that is safe and worthwhile
depends on a few signals this script derives, per object, from the audit JSON and
the YAML ``view_sql``:

  - ``view_sql_status`` - does the defining SQL run, and does it match the snapshot?
    (``match`` / ``diverge`` / ``error``). Only ``match``/``diverge`` are promotable;
    an ``error`` body would abort view registration at startup.
  - ``depends_on_views`` - other declared views referenced in the body. A view that
    reads only base tables is order-independent and safest to promote; one that
    reads other views needs those available first.
  - ``merge_target`` - whether runtime registration appends rows into it
    (``information_schema.tables``/``columns``/``schemata``). Promoting these to
    views would break ``register_user_relation``'s ``append_catalog_row`` writes, so
    they are held back.
  - ``blocker`` - for ``error`` bodies, the first error line.

Run from the project ROOT (no server needed - reads the YAML and the last audit):

    .venv/bin/python -m claude-scripts.plan_view_promotion

It prints the objects grouped by promotability so the plan in
``update-views-plan.md`` can be authored and kept honest.
"""

import json
import re
from collections import defaultdict
from pathlib import Path

from yaml_loader import load_yaml, walk_catalog_objects

SCHEMA_DIR = Path("pg_catalog_data/pg_schema")
AUDIT_JSON = Path("claude-scripts/catalog_audit.json")

# Runtime registration appends rows directly into these via append_catalog_row, so
# they must stay tables until that write path is reworked (see register_user_relation).
MERGE_TARGETS = {
    "information_schema.tables",
    "information_schema.columns",
    "information_schema.schemata",
}


def load_catalog():
    """Return ``(view_sql_by_name, all_view_names)`` read from the YAML catalog."""
    view_sql = {}
    view_names = set()
    for path in sorted(SCHEMA_DIR.glob("*.yaml")):
        for schema, name, node in walk_catalog_objects(load_yaml(path)):
            if node.get("type") == "view":
                view_names.add(name)
                if node.get("view_sql"):
                    view_sql[f"{schema}.{name}"] = node["view_sql"]
    return view_sql, view_names


def referenced_views(sql: str, all_view_names, self_name):
    """The declared-view names a body references (whole-word match, excluding self).

    A heuristic - it scans the SQL text for each known view name as a standalone
    token. Good enough to tell "reads only base tables" from "reads other views",
    which is what decides promotion ordering.
    """
    found = set()
    for vname in all_view_names:
        if vname == self_name:
            continue
        if re.search(rf"\b{re.escape(vname)}\b", sql):
            found.add(vname)
    return sorted(found)


def view_sql_status(rec):
    """Collapse the audit record's exec/content into ``match`` / ``diverge`` / ``error``."""
    if rec["exec_status"] != "ok":
        return "error"
    return "match" if rec["content_status"] == "match" else "diverge"


def main():
    view_sql, all_view_names = load_catalog()
    audit = {f"{o['schema']}.{o['name']}": o for o in json.loads(AUDIT_JSON.read_text())}

    # Objects declared as views but NOT currently served as views.
    targets = [
        o for o in audit.values()
        if o["kind"] == "view" and o["served_as"] != "view"
    ]

    rows = []
    for o in targets:
        qn = f"{o['schema']}.{o['name']}"
        sql = view_sql.get(qn, "")
        rows.append({
            "name": qn,
            "schema": o["schema"],
            "view_sql_status": view_sql_status(o),
            "depends_on_views": referenced_views(sql, all_view_names, o["name"]),
            "merge_target": qn in MERGE_TARGETS,
            "broken_category": o.get("broken_category", ""),
            "blocker": (o.get("exec_error") or "") if o["exec_status"] != "ok" else "",
            "diverging": o.get("differing_columns") or [],
        })

    buckets = defaultdict(list)
    for r in rows:
        if r["merge_target"]:
            key = "blocked: merge append target"
        elif r["view_sql_status"] == "error" and r["broken_category"] == "runtime-function":
            key = "blocked: needs server-runtime function"
        elif r["view_sql_status"] == "error":
            key = "blocked: view_sql errors (engine/UDF gap)"
        elif not r["depends_on_views"]:
            key = f"promotable, base-tables-only ({r['view_sql_status']})"
        else:
            key = f"promotable, reads other views ({r['view_sql_status']})"
        buckets[key].append(r)

    print(f"{len(rows)} declared views NOT served as views\n")
    for key in sorted(buckets):
        items = buckets[key]
        print(f"## {key} ({len(items)})")
        for r in sorted(items, key=lambda r: r["name"]):
            extra = ""
            if r["depends_on_views"]:
                extra += f"  <- {', '.join(r['depends_on_views'])}"
            if r["diverging"]:
                extra += f"  [diverges: {', '.join(r['diverging'])}]"
            if r["blocker"]:
                extra += f"  [{r['blocker'][:70]}]"
            print(f"  {r['name']}{extra}")
        print()


if __name__ == "__main__":
    main()
