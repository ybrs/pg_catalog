"""Analyze which catalog views actually run on our DataFusion engine.

For every `type: view` defined in the YAML catalog (information_schema + pg_catalog),
run its `view_sql` against a running server and classify the outcome. Produces a
Markdown report of what works and, for what doesn't, *why* (missing function,
unsupported cast/type, unknown column, etc.) so we know what to implement to turn
the materialized snapshots back into live views.

Usage (server must be listening on the conn string below):

    python analyze_catalog_views.py [--schema-dir DIR] [--out report.md]
"""

import argparse
import re
from collections import Counter
from pathlib import Path
from typing import List

import psycopg

from validate_pg_catalog_views import (
    ViewDefinition,
    ViewResult,
    _extract_views_from_yaml,
    run_view_query,
)

CONN_STR = "host=127.0.0.1 port=5444 dbname=pgtry user=dbuser password=pencil sslmode=disable"


def collect_all_views(schema_dir: Path) -> List[ViewDefinition]:
    """Collect every view definition under both catalog prefixes."""
    views: List[ViewDefinition] = []
    for path in sorted(schema_dir.glob("*.yaml")):
        if not (path.name.startswith("information_schema__") or path.name.startswith("pg_catalog__")):
            continue
        data = __import__("yaml").safe_load(path.read_text(encoding="utf-8"))
        views.extend(_extract_views_from_yaml(data, path))
    return views


def classify(result: ViewResult) -> str:
    """Bucket a failure by its root cause (from the error text)."""
    if result.status == "success":
        return "success"
    err = (result.error or "").lower()
    if result.status == "missing_function" or "invalid function" in err or (
        "function" in err and "does not exist" in err
    ):
        return "missing_function"
    if "no field named" in err or ("column" in err and "does not exist" in err):
        return "missing_column"
    if "cannot cast" in err or "cast to" in err or "no function matches" in err:
        return "unsupported_cast_or_type"
    if "table" in err and ("not found" in err or "does not exist" in err):
        return "missing_table"
    if "sql error" in err or "expected" in err or "parse" in err or "syntax" in err:
        return "parse_error"
    return "other_error"


def missing_symbol(result: ViewResult) -> str:
    """Best-effort extraction of the offending function/symbol from the error."""
    err = result.error or ""
    m = re.search(r"[Ii]nvalid function '([^']+)'", err)
    if m:
        return m.group(1) + "()"
    m = re.search(r"function ([\w.]+)\b.*does not exist", err)
    if m:
        return m.group(1) + "()"
    m = re.search(r"No field named (\S+)", err)
    if m:
        return m.group(1)
    return ""


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--schema-dir", type=Path, default=Path("pg_catalog_data/pg_schema"))
    parser.add_argument("--out", type=Path, default=Path("catalog_views_report.md"))
    args = parser.parse_args()

    views = collect_all_views(args.schema_dir)
    results = []
    # Use a fresh connection per view: a view that triggers a server-side panic
    # (e.g. a DataFusion runtime overflow) drops only its own connection, so it
    # can't poison the results of the views that follow it.
    for view in views:
        try:
            with psycopg.connect(CONN_STR, autocommit=True) as conn:
                res = run_view_query(conn, view)
        except Exception as exc:  # noqa: BLE001 - connection-level failure (e.g. backend crash)
            res = ViewResult(view=view, status="error", error=f"connection error: {exc}".strip())
        results.append((view, res, classify(res), missing_symbol(res)))

    # ---- aggregate ----
    by_schema = Counter(v.schema for v, *_ in results)
    by_cat = Counter(cat for *_, cat, _ in results)
    by_schema_cat = Counter((v.schema, cat) for v, _, cat, _ in results)
    missing_fns = Counter(
        sym for *_, cat, sym in results if cat == "missing_function" and sym
    )

    lines: List[str] = []
    w = lines.append
    w("# Catalog view compatibility report\n")
    w(f"Ran every `type: view` from `{args.schema_dir}` against the live engine "
      f"and recorded whether DataFusion can execute its `view_sql`.\n")
    w(f"**{len(views)} views total** "
      + ", ".join(f"{n} {s}" for s, n in sorted(by_schema.items())) + ".\n")

    w("## Outcome by category\n")
    w("| Category | Count |")
    w("|---|---|")
    for cat, n in by_cat.most_common():
        w(f"| {cat} | {n} |")
    w("")

    w("## Outcome by schema\n")
    w("| Schema | " + " | ".join(c for c, _ in by_cat.most_common()) + " |")
    w("|---" * (len(by_cat) + 1) + "|")
    for schema in sorted(by_schema):
        row = [schema] + [str(by_schema_cat.get((schema, c), 0)) for c, _ in by_cat.most_common()]
        w("| " + " | ".join(row) + " |")
    w("")

    if missing_fns:
        w("## Missing functions (most impactful to implement)\n")
        w("| Function | Views blocked |")
        w("|---|---|")
        for fn, n in missing_fns.most_common():
            w(f"| `{fn}` | {n} |")
        w("")

    w("## Per-view detail\n")
    w("| Schema.View | Category | Problem |")
    w("|---|---|---|")
    for v, res, cat, sym in sorted(results, key=lambda r: (r[2] != "success", r[0].schema, r[0].name)):
        prob = "" if cat == "success" else (res.error or "").replace("\n", " ")[:160]
        w(f"| {v.schema}.{v.name} | {cat} | {prob} |")
    w("")

    args.out.write_text("\n".join(lines), encoding="utf-8")
    print(f"wrote {args.out} ({len(views)} views; "
          + ", ".join(f"{n} {c}" for c, n in by_cat.most_common()) + ")")


if __name__ == "__main__":
    main()
