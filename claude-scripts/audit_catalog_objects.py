"""Audit every catalog object and report its real, verified state.

This walks the entire YAML catalog under ``pg_catalog_data/pg_schema`` and, for
each object, reports:

  - what it is: a base table (``system_catalog`` / ``table``) or a ``view``;
  - for views, whether the defining ``view_sql`` actually executes on our engine
    (run live over pgwire against a server the caller has started);
  - for views that also ship a captured-from-PostgreSQL ``rows`` snapshot,
    whether our output matches that snapshot in row count and in content.

The point is to replace hand-maintained claims in ``CATALOG-REFERENCE.md`` with a
machine-checked inventory. It does not guess: a view is ``working`` only if it
both executes and reproduces the snapshot; ``partial`` if it executes but the
count or content diverges; ``broken-exec`` if the SQL errors. Views with no
snapshot rows (the live-server-runtime views) are reported as ``no-snapshot`` and
their execution result is still recorded.

Run it from the project ROOT as a module, so the catalog helper modules
(``yaml_loader`` etc.) import normally (a server must already be listening):

    .venv/bin/python -m claude-scripts.audit_catalog_objects \
        --conn "host=127.0.0.1 port=5444 dbname=pgtry user=dbuser password=pencil sslmode=disable" \
        --out claude-scripts/catalog_audit.md

The execution/content logic mirrors ``tests/test_view_output_snapshot.py`` so this
report and that regression test never disagree about what passes.
"""

import argparse
import glob
import json
import re
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, List, Optional

import psycopg

from yaml_loader import load_yaml

SCHEMA_DIR = Path("pg_catalog_data/pg_schema")

# A column named alias_<n> is a tell that the view-creation rewrite dropped a real
# column name (e.g. a bare `tbl.col` projection with no AS) - the served view is
# broken even if it returns the right rows.
ALIAS_COLUMN = re.compile(r"^alias_\d+$")

# The single demo table the test server registers at startup (public.users). The
# object-enumerating views return one extra row for it; we strip exactly those
# rows by name before comparing counts, the same way the snapshot test does.
DEMO_TABLE_NAME = "users"
DEMO_TABLE_ROW_COLUMN = {
    "pg_catalog.pg_tables": "tablename",
    "information_schema.tables": "table_name",
    "information_schema.columns": "table_name",
    "information_schema.column_udt_usage": "table_name",
    "information_schema.data_type_privileges": "object_name",
}


@dataclass
class CatalogObject:
    """One object found in the YAML catalog and the verdict of auditing it.

    ``kind`` is ``table`` or ``view``. ``exec_status`` is ``ok`` or ``error`` for
    views (``None`` for tables, which are always queryable seed/registered data).
    ``content_status`` is one of ``match`` / ``count-mismatch`` / ``content-mismatch``
    / ``no-snapshot`` / ``n-a``. ``status`` is the rolled-up verdict shown to a human.
    """

    schema: str
    name: str
    kind: str
    yaml_type: str
    has_snapshot: bool
    expected_rows: Optional[int] = None
    got_rows: Optional[int] = None
    exec_status: Optional[str] = None
    exec_error: Optional[str] = None
    content_status: str = "n-a"
    differing_columns: List[str] = field(default_factory=list)
    status: str = ""
    broken_category: str = ""
    served_as: str = ""  # "view" (live CREATE VIEW) or "table" (materialized MemTable)
    alias_columns: List[str] = field(default_factory=list)  # bogus alias_N cols, if any

    @property
    def qualified_name(self) -> str:
        """The ``schema.name`` identifier used as the object's key everywhere."""
        return f"{self.schema}.{self.name}"


def collect_objects(schema_dir: Path) -> List[CatalogObject]:
    """Read every YAML file and yield one CatalogObject per catalog object.

    Tables carry ``type: system_catalog`` (pg_catalog) or ``type: table``
    (information_schema); views carry ``type: view``. A view's snapshot rows live
    under ``rows:`` and may be absent (live-runtime views) or an empty list.
    """
    objects: List[CatalogObject] = []
    for path in sorted(schema_dir.glob("*.yaml")):
        doc = load_yaml(path)
        for schema, name, node in _walk_objects(doc):
            yaml_type = node.get("type", "")
            if yaml_type == "view":
                rows = node.get("rows")
                objects.append(
                    CatalogObject(
                        schema=schema,
                        name=name,
                        kind="view",
                        yaml_type=yaml_type,
                        has_snapshot=rows is not None,
                        expected_rows=len(rows) if rows is not None else None,
                    )
                )
            elif yaml_type in ("system_catalog", "table"):
                objects.append(
                    CatalogObject(
                        schema=schema,
                        name=name,
                        kind="table",
                        yaml_type=yaml_type,
                        has_snapshot=False,
                    )
                )
    return objects


def _walk_objects(doc: Dict):
    """Yield ``(schema, name, node)`` for each leaf object dict in a catalog YAML.

    The YAML nests ``catalog -> schema -> object -> {type, ...}``; an object node is
    any dict carrying a ``type`` key. Yielding the surrounding schema/name lets the
    caller key objects without re-deriving them from the file name.
    """
    stack = [([], doc)]
    while stack:
        prefix, node = stack.pop()
        if not isinstance(node, dict):
            continue
        if "type" in node and not isinstance(node["type"], dict):
            schema = prefix[-2] if len(prefix) >= 2 else "?"
            name = prefix[-1] if prefix else "?"
            yield schema, name, node
            continue
        for key, value in node.items():
            stack.append((prefix + [key], value))


def _view_sql_by_name(schema_dir: Path) -> Dict[str, str]:
    """Map ``schema.name -> view_sql`` for every view in the catalog.

    Read straight from YAML so a view with no snapshot still has its defining SQL
    available to execute live.
    """
    sql: Dict[str, str] = {}
    for path in sorted(schema_dir.glob("*.yaml")):
        doc = load_yaml(path)
        for schema, name, node in _walk_objects(doc):
            if node.get("type") == "view" and node.get("view_sql"):
                sql[f"{schema}.{name}"] = node["view_sql"]
    return sql


def _snapshot_rows_by_name(schema_dir: Path) -> Dict[str, list]:
    """Map ``schema.name -> snapshot rows`` for every view that ships rows."""
    rows_by_name: Dict[str, list] = {}
    for path in sorted(schema_dir.glob("*.yaml")):
        doc = load_yaml(path)
        for schema, name, node in _walk_objects(doc):
            if node.get("type") == "view" and node.get("rows") is not None:
                rows_by_name[f"{schema}.{name}"] = node["rows"]
    return rows_by_name


def _canon(value, db_remap):
    """Canonicalize a cell so ``1``, ``1.0`` and ``"1"`` compare equal, etc.

    Mirrors the snapshot test's ``_canon`` exactly, including the database-name
    remap: the snapshot was captured on one database while our server advertises
    another, so every ``*_catalog`` column holds a different name. ``db_remap`` maps
    the live database name onto the snapshot's, so that one expected difference is
    not mistaken for a real content divergence.
    """
    if value is None:
        return None
    # db_remap keys are database-name strings; guard the membership test so an
    # unhashable cell (e.g. an array column like pg_group.grolist) does not raise.
    if isinstance(value, str) and value in db_remap:
        return db_remap[value]
    if isinstance(value, bool):
        return "t" if value else "f"
    if isinstance(value, float):
        return str(int(value)) if value.is_integer() else repr(value)
    if isinstance(value, int):
        return str(value)
    if isinstance(value, (list, tuple)):
        return "[" + ",".join(_canon(v, db_remap) or "" for v in value) + "]"
    return str(value)


def _row_multiset(rows, db_remap):
    """An order-independent multiset of canonicalized ``column -> value`` rows."""
    return Counter(
        tuple(sorted((col, _canon(val, db_remap)) for col, val in row.items()))
        for row in rows
    )


def _snapshot_db(schema_dir: Path) -> str:
    """The database name baked into the snapshot, read from the snapshot itself.

    The ``information_schema_catalog_name`` view holds exactly that one row, so the
    name is derived rather than hardcoded - matching the snapshot test.
    """
    doc = load_yaml(schema_dir / "information_schema__information_schema_catalog_name.yaml")
    from yaml_loader import find_in_doc

    return find_in_doc(doc, "rows")[0]["catalog_name"]


def _live_db(conn) -> str:
    """The database name our server advertises, read from ``current_database()``."""
    cur = conn.cursor()
    cur.execute("SELECT current_database()")
    return cur.fetchone()[0]


def _differing_columns(only_expected, only_got):
    """The columns whose value distribution differs across the symmetric diff."""
    columns = {col for row in only_expected + only_got for col, _ in row}
    differing = []
    for col in sorted(columns):
        expected_vals = Counter(dict(row).get(col) for row in only_expected)
        got_vals = Counter(dict(row).get(col) for row in only_got)
        if expected_vals != got_vals:
            differing.append(col)
    return differing


def _strip_demo_rows(name, dicts):
    """Drop the demo ``users`` table's rows from an object-enumerating view's output."""
    demo_col = DEMO_TABLE_ROW_COLUMN.get(name)
    if demo_col is None:
        return dicts
    return [d for d in dicts if d.get(demo_col) != DEMO_TABLE_NAME]


# Server-runtime functions that report live process/IO/WAL/lock/progress state. A
# view that fails only because one of these is missing is inherently empty in a
# static catalog - it is a different class of "broken" than a planner/UDF gap that
# we could close to make a view with real seed data come alive.
RUNTIME_FUNCTION_MARKERS = (
    "pg_stat_get_",
    "pg_lock_status",
    "pg_cursor",
    "pg_prepared_statement",
    "pg_prepared_xact",
    "pg_get_wait_events",
    "pg_show_all_file_settings",
    "pg_get_backend_memory_contexts",
    "pg_get_shmem_allocations",
    "pg_get_replication_slots",
    "pg_show_replication_origin_status",
)


def classify_broken(exec_error: str) -> str:
    """Bucket a non-executing view by whether it needs a live-server-runtime function.

    ``runtime-function`` means the view's SQL calls a process/IO/WAL/lock/progress
    table or scalar function we do not host (inherently empty in a static catalog);
    ``engine-gap`` means a planner/parser/UDF gap over data we DO have, which is the
    fixable, higher-value class. Mirrors the two broken buckets in CATALOG-REFERENCE.md.
    """
    err = (exec_error or "").lower()
    if any(marker.lower() in err for marker in RUNTIME_FUNCTION_MARKERS):
        return "runtime-function"
    return "engine-gap"


def audit_view(conn, obj: CatalogObject, view_sql: str, snapshot_rows: Optional[list], db_remap):
    """Execute one view live and fill in its exec/content verdict in place.

    The content query depends on how the object is served (set on ``obj.served_as``
    before this call): for a real view we query the **served relation**
    (``SELECT * FROM schema.name``), which exercises the registered, rewritten view
    body - the rows a client actually gets; for an object served as a table we run
    its raw ``view_sql`` instead, which answers "would its definition work if it were
    promoted to a view" while the headline status stays broken (it is a table).

    On error, records ``exec_status='error'`` and the first error line. On success
    with a snapshot, compares row count (after stripping the demo table) and, when
    counts match, row content; records which columns diverge. ``db_remap`` maps the
    live database name onto the snapshot's so the expected ``current_database()``
    difference is not counted as a content divergence.
    """
    content_query = (
        f"SELECT * FROM {obj.schema}.{obj.name}" if obj.served_as == "view" else view_sql
    )
    cur = conn.cursor()
    try:
        cur.execute(content_query)
        raw = cur.fetchall()
        cols = [d.name for d in cur.description]
    except Exception as exc:  # noqa: BLE001 - any planner/runtime error means "does not execute"
        obj.exec_status = "error"
        obj.exec_error = str(exc).splitlines()[0][:160]
        obj.content_status = "n-a"
        obj.broken_category = classify_broken(obj.exec_error)
        return

    # A real view that exposes alias_N column names is broken regardless of row
    # content: the rewrite dropped the real column names, so anything joining to it
    # by name fails. Empty views (0 rows) would otherwise pass the content check
    # despite this, so the column names are inspected explicitly.
    if obj.served_as == "view":
        obj.alias_columns = [c for c in cols if ALIAS_COLUMN.match(c)]

    obj.exec_status = "ok"
    if snapshot_rows is None:
        obj.content_status = "no-snapshot"
        obj.got_rows = len(raw)
        return

    engine_dicts = _strip_demo_rows(obj.qualified_name, [dict(zip(cols, r)) for r in raw])
    obj.got_rows = len(engine_dicts)
    if obj.got_rows != obj.expected_rows:
        obj.content_status = "count-mismatch"
        return

    expected_ms = _row_multiset(snapshot_rows, db_remap)
    got_ms = _row_multiset(engine_dicts, db_remap)
    if expected_ms == got_ms:
        obj.content_status = "match"
    else:
        obj.content_status = "content-mismatch"
        obj.differing_columns = _differing_columns(
            list((expected_ms - got_ms).elements()),
            list((got_ms - expected_ms).elements()),
        )


def detect_served_as(conn, schema: str, name: str) -> str:
    """Return how the running server physically serves ``schema.name``: as a live
    ``view`` or as a materialized ``table``.

    This is read empirically from the query plan, not assumed from the YAML: a real
    DataFusion view (registered via ``CREATE VIEW``) inlines its body, so the plan
    root is ``SubqueryAlias: schema.name`` over the base tables; a ``MemTable`` (the
    materialized snapshot path) shows ``TableScan: schema.name``. Anything declared a
    view in the YAML but answering ``table`` here is a frozen snapshot, not a view.
    """
    cur = conn.cursor()
    try:
        cur.execute(f"EXPLAIN SELECT * FROM {schema}.{name}")
        first_plan_line = cur.fetchone()[-1].splitlines()[0].strip()
    except Exception:  # noqa: BLE001 - if even a plain scan fails the object is unusable
        return "unqueryable"
    if first_plan_line.startswith("SubqueryAlias:"):
        return "view"
    if first_plan_line.startswith("TableScan:"):
        return "table"
    return "unknown"


def audit_table(conn, obj: CatalogObject):
    """Actually query one base table and record whether it is selectable.

    A base table holds seed (and possibly runtime-registered) rows; "working" means
    ``SELECT count(*)`` over it succeeds. This is checked rather than assumed so the
    report never claims a table works without having queried it.
    """
    cur = conn.cursor()
    try:
        cur.execute(f"SELECT count(*) FROM {obj.schema}.{obj.name}")
        obj.got_rows = cur.fetchone()[0]
        obj.exec_status = "ok"
    except Exception as exc:  # noqa: BLE001 - an unqueryable base table is a real defect
        obj.exec_status = "error"
        obj.exec_error = str(exc).splitlines()[0][:160]
        obj.broken_category = classify_broken(obj.exec_error)


def roll_up_status(obj: CatalogObject) -> str:
    """Collapse served-as + exec + content into one human verdict.

    A base table is ``working`` if it is served as a table and ``SELECT count(*)``
    succeeds. For an object the YAML declares a VIEW, being served as a view is a
    precondition for any positive verdict: if the server materializes it as a table
    (``served_as == 'table'``) it is ``broken-not-a-view`` - a frozen snapshot that
    does not re-derive from its base tables, regardless of whether its `view_sql`
    would have run. Only objects actually served as views are graded ``working`` /
    ``partial`` on their `view_sql` execution and content.
    """
    if obj.kind == "table":
        return "working" if obj.served_as == "table" and obj.exec_status == "ok" else "broken"
    # A YAML-declared view that the server serves as a materialized table is not a
    # view at all - the defect this audit was rerun to surface.
    if obj.served_as == "table":
        return "broken-not-a-view"
    if obj.served_as != "view":
        return "broken"  # unqueryable / unrecognized plan shape
    if obj.exec_status == "error":
        return "broken"
    if obj.alias_columns:
        return "broken-alias-columns"  # served as a view but exposes alias_N names
    if obj.content_status == "match":
        return "working"
    if obj.content_status == "no-snapshot":
        return "partial-unverified"
    return "partial"


def render_report(objects: List[CatalogObject]) -> str:
    """Render the full audit as Markdown: a summary plus a per-object table."""
    lines: List[str] = []
    w = lines.append

    tables = [o for o in objects if o.kind == "table"]
    views = [o for o in objects if o.kind == "view"]

    w("# Catalog object audit (machine-checked)\n")
    w(f"- base tables: {len(tables)} "
      f"({sum(o.schema == 'pg_catalog' for o in tables)} pg_catalog, "
      f"{sum(o.schema == 'information_schema' for o in tables)} information_schema)")
    w(f"- views: {len(views)} "
      f"({sum(o.schema == 'pg_catalog' for o in views)} pg_catalog, "
      f"{sum(o.schema == 'information_schema' for o in views)} information_schema)\n")

    by_status = Counter(o.status for o in views)
    w("## View status\n")
    w("| status | count |")
    w("|---|---|")
    for st, n in by_status.most_common():
        w(f"| {st} | {n} |")
    w("")

    table_status = Counter(o.status for o in tables)
    w("## Base-table status (every table queried with SELECT count(*))\n")
    w("| status | count |")
    w("|---|---|")
    for st, n in table_status.most_common():
        w(f"| {st} | {n} |")
    w("")

    served = Counter(o.served_as for o in views)
    w("## How declared views are actually served\n")
    w("A YAML `type: view` is only a real view if the server registers it with "
      "`CREATE VIEW` (it then re-derives from its base tables on every query). "
      "Anything served as a `table` is a frozen MemTable snapshot - a view in name "
      "only. Read empirically from each object's query plan.\n")
    w("| served as | count |")
    w("|---|---|")
    for s, n in served.most_common():
        w(f"| {s} | {n} |")
    w("")

    promotable = Counter(
        ("view_sql ok" if o.exec_status == "ok" else "view_sql errors")
        for o in views if o.served_as != "view"
    )
    w("Of the declared views NOT served as views, whether their `view_sql` would "
      "even run if promoted to a real view:\n")
    w("| view_sql | count |")
    w("|---|---|")
    for s, n in promotable.most_common():
        w(f"| {s} | {n} |")
    w("")

    for schema in ("pg_catalog", "information_schema"):
        w(f"## {schema} views\n")
        w("| view | status | served as | view_sql exec | content | diverging / error |")
        w("|---|---|---|---|---|---|")
        for o in sorted(views, key=lambda o: o.name):
            if o.schema != schema:
                continue
            detail = ", ".join(o.differing_columns) if o.differing_columns else (o.exec_error or "")
            if o.alias_columns:
                detail = f"alias columns: {', '.join(o.alias_columns)}"
            w(f"| {o.name} | {o.status} | {o.served_as} | {o.exec_status or ''} "
              f"| {o.content_status} | {detail} |")
        w("")

    for schema in ("pg_catalog", "information_schema"):
        w(f"## {schema} base tables\n")
        w("| table | status | rows | error |")
        w("|---|---|---|---|")
        for o in sorted(tables, key=lambda o: o.name):
            if o.schema != schema:
                continue
            got = "" if o.got_rows is None else str(o.got_rows)
            w(f"| {o.name} | {o.status} | {got} | {o.exec_error or ''} |")
        w("")

    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--conn",
        default="host=127.0.0.1 port=5444 dbname=pgtry user=dbuser password=pencil sslmode=disable",
        help="psycopg connection string for an already-running server",
    )
    parser.add_argument("--schema-dir", type=Path, default=SCHEMA_DIR)
    parser.add_argument("--out", type=Path, default=Path("claude-scripts/catalog_audit.md"))
    parser.add_argument("--json-out", type=Path, default=Path("claude-scripts/catalog_audit.json"))
    args = parser.parse_args()

    objects = collect_objects(args.schema_dir)
    view_sql = _view_sql_by_name(args.schema_dir)
    snapshots = _snapshot_rows_by_name(args.schema_dir)

    # Map the live database name onto the snapshot's, so the expected
    # current_database() difference on every *_catalog column is not counted.
    with psycopg.connect(args.conn, autocommit=True) as conn:
        db_remap = {_live_db(conn): _snapshot_db(args.schema_dir)}

    for obj in objects:
        # Fresh connection per object: one that panics the backend drops only its
        # own connection and cannot poison the objects audited after it.
        with psycopg.connect(args.conn, autocommit=True) as conn:
            obj.served_as = detect_served_as(conn, obj.schema, obj.name)
            if obj.kind == "table":
                audit_table(conn, obj)
            else:
                audit_view(
                    conn,
                    obj,
                    view_sql[obj.qualified_name],
                    snapshots.get(obj.qualified_name),
                    db_remap,
                )
        obj.status = roll_up_status(obj)

    args.out.write_text(render_report(objects), encoding="utf-8")
    args.json_out.write_text(
        json.dumps(
            [
                {
                    "schema": o.schema,
                    "name": o.name,
                    "kind": o.kind,
                    "status": o.status,
                    "exec_status": o.exec_status,
                    "content_status": o.content_status,
                    "expected_rows": o.expected_rows,
                    "got_rows": o.got_rows,
                    "differing_columns": o.differing_columns,
                    "exec_error": o.exec_error,
                    "broken_category": o.broken_category,
                    "served_as": o.served_as,
                    "alias_columns": o.alias_columns,
                }
                for o in objects
            ],
            indent=2,
        ),
        encoding="utf-8",
    )

    views = [o for o in objects if o.kind == "view"]
    by_status = Counter(o.status for o in views)
    print(f"audited {len(objects)} objects "
          f"({len(views)} views, {len(objects) - len(views)} tables)")
    print("view status: " + ", ".join(f"{n} {s}" for s, n in by_status.most_common()))
    print(f"wrote {args.out} and {args.json_out}")


if __name__ == "__main__":
    main()
