#!/usr/bin/env python3
"""Group the failing rows of catalog_views_report.md by their blocking symbol.

Reads the per-view report and writes catalog-failing-views.md: failing views
grouped by the first missing table/function/error, split into "likely useful"
vs "runtime/monitoring" (the latter would only ever be empty/zero stubs).
"""
import re
from collections import defaultdict

STATUSES = {
    "success", "missing_table", "missing_function", "other_error",
    "missing_column", "parse_error", "unsupported_cast_or_type",
}
MONITORING_MARKERS = (
    "stat", "progress", "activity", "replication", "wal", "lock", "prepared",
    "cursor", "backend", "shmem", "archiver", "recovery", "slru",
    "subscription", "_io", "wait", "hba_file", "ident_file", "file_settings",
)


def blocking_symbol(error: str) -> str:
    """A short label for what blocks a view: the named missing
    table-function/table/function if there is one, otherwise a category for the
    planner/parser error (so nothing hides behind a vague catch-all)."""
    for pattern in (
        r"table function '([^']+)'",
        r"table '([^']+)' not found",
        r"function ([\w.]+)\(",
    ):
        match = re.search(pattern, error)
        if match:
            return match.group(1)
    # Not a missing symbol — categorize the planner/parser failure.
    categories = [
        ("Correlated scalar subquery must be aggregated", "correlated scalar subquery (needs aggregation)"),
        ("The subquery should only return one column", "multi-column IN subquery"),
        ("must be in GROUP BY", "spurious GROUP BY (group-by heuristic)"),
        ("No field named", "column scoping after rewrite"),
        ("Unsupported SQL type", "unsupported SQL type"),
        ("sql parser error", "sqlparser parse error"),
        ("Assertion failed", "upstream DataFusion internal assertion"),
        ("Internal error", "upstream DataFusion internal error"),
    ]
    for needle, label in categories:
        if needle in error:
            return label
    return "other planner/type error"


def is_monitoring(view: str) -> bool:
    """True for runtime/monitoring views (live stats, no static answer)."""
    return any(marker in view for marker in MONITORING_MARKERS)


def short(view: str) -> str:
    """Drop the schema prefix for compact listing."""
    return view.replace("pg_catalog.", "").replace("information_schema.", "is.")


def main() -> None:
    """Parse the report and emit the grouped markdown document."""
    succeeded, failed = [], []
    for line in open("catalog_views_report.md"):
        if not line.startswith("| "):
            continue
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) < 2:
            continue
        view, status = cells[0], cells[1]
        if "." not in view or status not in STATUSES:
            continue  # skip legend / summary rows
        error = "|".join(cells[2:]) if len(cells) > 2 else ""
        (succeeded if status == "success" else failed).append((view, status, error))

    grouped = defaultdict(list)
    for view, status, error in failed:
        grouped[blocking_symbol(error)].append((view, status))
    groups = sorted(grouped.items(), key=lambda kv: -len(kv[1]))
    useful = [(k, v) for k, v in groups if not all(is_monitoring(view) for view, _ in v)]
    monitoring = [(k, v) for k, v in groups if all(is_monitoring(view) for view, _ in v)]

    lines = [
        "# Failing catalog views — grouped by blocker",
        "",
        f"From `catalog_views_report.md`: **{len(succeeded)} succeed, {len(failed)} fail** "
        f"({len(succeeded) + len(failed)} views total).",
        "A view fails at its FIRST missing symbol; fixing one may reveal another blocker.",
        "",
        "## Likely useful (non-monitoring) — prioritize",
        "",
        "| Blocker | #views | Views |",
        "|---|---|---|",
    ]
    for symbol, views in useful:
        lines.append(f"| `{symbol}` | {len(views)} | " + ", ".join(short(v) for v, _ in views) + " |")
    lines += [
        "",
        "## Runtime / monitoring views — low value (would be empty/zero stubs)",
        "",
        "| Blocker | #views | Views |",
        "|---|---|---|",
    ]
    for symbol, views in monitoring:
        lines.append(f"| `{symbol}` | {len(views)} | " + ", ".join(short(v) for v, _ in views) + " |")

    with open("catalog-failing-views.md", "w") as out:
        out.write("\n".join(lines) + "\n")
    print(f"succeed={len(succeeded)} fail={len(failed)}")
    print(f"useful failing views={sum(len(v) for _, v in useful)} "
          f"monitoring failing views={sum(len(v) for _, v in monitoring)}")


if __name__ == "__main__":
    main()
