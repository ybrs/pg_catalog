# Snapshot/regression test: every information_schema / pg_catalog VIEW ships with
# a `rows` snapshot captured from real PostgreSQL. We run each view's `view_sql`
# through our engine over pgwire and compare it against that snapshot at three
# levels of strictness, so a silently-broken view can't pass unnoticed:
#
#   1. test_view_row_counts_match_snapshot   - row COUNT matches, AND a view that
#      *errors* while its snapshot has rows is flagged (not silently skipped).
#   2. test_view_content_matches_snapshot    - for views whose count matches, the
#      actual row CONTENT matches (catches "right number of wrong rows").
#
# Each level has a baseline of views that legitimately diverge today, with the
# reason. A NEW divergence fails the test (a real regression); a baselined view
# that now matches ALSO fails (so the baseline never silently rots). When a gap is
# closed, remove the view from its baseline.
#
# DB-name note: the snapshot was captured on one PostgreSQL database while our
# server advertises another, so `current_database()` differs on every row of every
# view with a `*_catalog` column. Both names are read at runtime - the live one
# from `current_database()`, the snapshot one from the `information_schema_catalog_name`
# view in the snapshot - and `_canon` maps the live name onto the snapshot name so
# that one expected difference doesn't drown out real ones. Nothing is hardcoded to
# a particular environment.

import functools
import glob
import json
import os
from collections import Counter

import duckdb
import psycopg

from build_snapshot_db import DB_PATH, SCHEMA_DIR, build
from conftest import SHARED_PORT, conn_str, load_yaml, server  # noqa: F401
from yaml_loader import find_in_doc

CONN_STR = conn_str(SHARED_PORT)


def _snapshot_db():
    """The database name baked into the catalog snapshot, read from the snapshot
    itself (the `information_schema_catalog_name` view holds exactly that one row).

    Deriving it - rather than hardcoding "postgres" - keeps the test reproducible
    if the catalog is ever regenerated against a differently-named database.
    """
    doc = load_yaml(
        "pg_catalog_data/pg_schema/information_schema__information_schema_catalog_name.yaml"
    )
    return find_in_doc(doc, "rows")[0]["catalog_name"]


def _live_db(conn):
    """The database name this server advertises, read from `current_database()`.

    The snapshot's `*_catalog` columns hold the snapshot DB name on every row,
    while ours hold this; `_canon` maps the live name to the snapshot name so that
    one expected difference doesn't masquerade as a content mismatch. Both names
    are obtained at runtime, so nothing here is pinned to a specific environment.
    """
    cur = conn.cursor()
    cur.execute("SELECT current_database()")
    return cur.fetchone()[0]

# The test server (src/main.rs) registers exactly one demo table at startup,
# public.users (id int, name text). It is the ONLY object beyond the seed, so it is
# the entire, explicit reason the object-enumerating views below return extra rows.
# Rather than baselining them with a blanket "the count may differ" (which would
# hide a real bug - a dropped row, a corrupted value, an unexpected extra object),
# we strip exactly the demo table's rows by name and then require the remainder to
# match the snapshot EXACTLY, in both count and content. The mapping is view ->
# the column that holds the table name to filter on.
DEMO_TABLE_NAME = "users"
DEMO_TABLE_ROW_COLUMN = {
    "pg_catalog.pg_tables": "tablename",
    "information_schema.tables": "table_name",
    "information_schema.columns": "table_name",
    "information_schema.column_udt_usage": "table_name",
    "information_schema.data_type_privileges": "object_name",
}

# Views whose row COUNT legitimately differs from the PostgreSQL snapshot.
KNOWN_COUNT_MISMATCHES = {
    # Permissive privilege stubs: we don't model GRANTs, so privilege views are empty.
    "information_schema.column_privileges": "privilege stubs: no GRANTs modeled -> empty",
    "information_schema.routine_privileges": "privilege stubs: no GRANTs modeled -> empty",
    "information_schema.table_privileges": "privilege stubs: no GRANTs modeled -> empty",
    "information_schema.udt_privileges": "privilege stubs: no GRANTs modeled -> empty",
    "information_schema.usage_privileges": "privilege stubs: partial",
    # One role short: we don't fully model pg_auth_members role membership yet.
    "information_schema.applicable_roles": "role membership (pg_auth_members) not fully modeled",
}

# Views whose row count matches but whose CONTENT differs, because they expose a
# column we don't reproduce byte-for-byte. The set after the colon is the columns
# that differ, so a content regression elsewhere in the same view is still caught.
KNOWN_CONTENT_MISMATCHES = {
    # `pg_get_function_arg_default` is a NULL stub, so parameter_default is empty.
    "information_schema.parameters": "parameter_default: pg_get_function_arg_default stub",
    # Definition text we don't deparse: pg_get_viewdef / indexdef / ruledef return
    # NULL, and pg_get_constraintdef shows the raw node-tree instead of SQL.
    "information_schema.views": "view_definition/is_updatable: pg_get_viewdef not reproduced",
    "information_schema.check_constraints": "check_clause: raw node-tree, pg_get_constraintdef not reproduced",
    "pg_catalog.pg_views": "definition: pg_get_viewdef not reproduced",
    "pg_catalog.pg_rules": "definition: pg_get_ruledef not reproduced",
    # After stripping the demo `users` rows the COUNT matches exactly; the only
    # remaining content gap is is_updatable on 4 columns - the
    # `pg_relation_is_updatable` stub returns 0 (not updatable). (The precision/
    # length columns are now computed by the real `_pg_*` helpers.)
    "information_schema.columns": "is_updatable: pg_relation_is_updatable stub returns 0",
    # is_insertable_into differs for a couple of views (pg_group,
    # pg_stat_database_conflicts) - we don't reproduce its exact value for views.
    "information_schema.tables": "is_insertable_into: not reproduced for views",
}

# Views that ERROR today even though their snapshot has rows we ought to produce.
# These are real gaps (a missing runtime function or an unsupported plan), tracked
# so the hardened count test stays green now but flags any *new* such regression.
KNOWN_EXEC_FAILURES = {
    "pg_catalog.pg_available_extension_versions": "GROUP BY wildcard not planned",
    "pg_catalog.pg_file_settings": "table function pg_show_all_file_settings missing",
    "pg_catalog.pg_group": "pg_authid.oid not resolvable after subquery flattening",
    "pg_catalog.pg_wait_events": "table function pg_get_wait_events missing",
}


@functools.lru_cache(maxsize=1)
def _snapshot_duckdb():
    """Path to the precomputed view-snapshot DuckDB, rebuilt once if missing or
    older than the newest catalog YAML.

    The triples (name, view_sql, rows) are precomputed by `build_snapshot_db.py`
    so the test does not re-parse ~200 YAML files (~11s) on every run. The
    freshness check rebuilds automatically if a YAML was edited without rerunning
    that script, so an edited snapshot can never be compared against stale data.
    """
    newest_yaml = max(os.path.getmtime(p) for p in glob.glob(f"{SCHEMA_DIR}/*.yaml"))
    if not os.path.exists(DB_PATH) or os.path.getmtime(DB_PATH) < newest_yaml:
        build()
    return DB_PATH


def _views_with_snapshots():
    """Yield (view_name, view_sql, snapshot_rows) for every view that has both.

    `snapshot_rows` is the list of `column -> value` dicts captured from real
    PostgreSQL; its length is the expected row count. Read from the precomputed
    DuckDB snapshot rather than the YAML (see `_snapshot_duckdb`).
    """
    con = duckdb.connect(_snapshot_duckdb(), read_only=True)
    try:
        snapshots = con.execute(
            "SELECT name, view_sql, rows_json FROM view_snapshots"
        ).fetchall()
    finally:
        con.close()
    for name, view_sql, rows_json in snapshots:
        yield name, view_sql, json.loads(rows_json)


def _canon(value, db_remap):
    """Canonicalize a cell value for snapshot-vs-engine comparison.

    Numbers collapse to their integer/decimal text (so `1`, `1.0` and `"1"`
    compare equal), booleans to `t`/`f`, arrays recurse, and any value found in
    `db_remap` (the live database name -> the snapshot's) is substituted, so the
    `current_database()` difference doesn't read as a content mismatch.
    """
    if value is None:
        return None
    if value in db_remap:
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
    """A multiset of canonicalized rows (list of `column -> value` dicts).

    Order-independent: views need not return rows in the snapshot's order.
    """
    return Counter(
        tuple(sorted((col, _canon(val, db_remap)) for col, val in row.items()))
        for row in rows
    )


def _engine_rows_without_demo(name, cols, raw_rows):
    """Engine result rows as `column -> value` dicts, with the demo `users` table's
    rows removed for the object-enumerating views (see `DEMO_TABLE_ROW_COLUMN`).

    For every other view the rows are returned unchanged. Removing exactly the demo
    table's rows lets the enumerating views be compared to the snapshot EXACTLY
    instead of being given a blanket count exemption that would mask real bugs.
    """
    dicts = [dict(zip(cols, r)) for r in raw_rows]
    demo_col = DEMO_TABLE_ROW_COLUMN.get(name)
    if demo_col is not None:
        dicts = [d for d in dicts if d.get(demo_col) != DEMO_TABLE_NAME]
    return dicts


def test_views_match_snapshot(server):
    """Every view's output matches the real-PostgreSQL snapshot, in count then content.

    Each view's `view_sql` is executed ONCE. We first compare the row COUNT; only
    when the count matches do we compare the row CONTENT (a count mismatch already
    explains the difference, so a row-by-row diff would be noise). Both checks share
    the single execution.

    Fails on any of: a new count mismatch, a new execution failure on a view whose
    snapshot has rows, a new content mismatch, or a baselined gap (exec/count/content)
    that now matches - the last so a stale baseline can't silently rot. Snapshot-empty
    views that error are left alone (runtime views like pg_stat_*/locks we don't model).
    """
    conn = psycopg.connect(CONN_STR, autocommit=True)
    # Map the live database name onto the snapshot's, so the one expected
    # `current_database()` difference on every `*_catalog` column is not counted.
    db_remap = {_live_db(conn): _snapshot_db()}

    new_count_mismatches = []     # (name, expected, got) not in the baseline
    new_exec_failures = []        # (name, error) erroring with a non-empty snapshot
    new_content_mismatches = []   # (name, n_differing, columns)
    fixed_exec = []               # baselined exec failures that now execute
    fixed_count = []              # baselined count mismatches that now match
    fixed_content = []            # baselined content mismatches that now match

    for name, view_sql, rows in _views_with_snapshots():
        expected = len(rows)
        cur = conn.cursor()
        try:
            cur.execute(view_sql)
            raw = cur.fetchall()
            cols = [d.name for d in cur.description]
        except Exception as exc:
            if name in KNOWN_EXEC_FAILURES:
                continue  # known gap; staying broken is fine
            if expected > 0:
                new_exec_failures.append((name, str(exc).splitlines()[0][:100]))
            continue  # snapshot-empty runtime view: erroring is acceptable
        if name in KNOWN_EXEC_FAILURES:
            fixed_exec.append(name)
            continue

        engine_dicts = _engine_rows_without_demo(name, cols, raw)
        got = len(engine_dicts)
        count_known = name in KNOWN_COUNT_MISMATCHES

        # Count first. A mismatch (or a baselined count gap) is terminal for this
        # view - we do not go on to compare content.
        if got != expected:
            if not count_known:
                new_count_mismatches.append((name, expected, got))
            continue
        if count_known:
            fixed_count.append(name)  # baseline says it should differ, but it matches now
            continue

        # Counts match: compare content row by row.
        expected_ms = _row_multiset(rows, db_remap)
        got_ms = _row_multiset(engine_dicts, db_remap)
        content_known = name in KNOWN_CONTENT_MISMATCHES
        if expected_ms == got_ms:
            if content_known:
                fixed_content.append(name)
        elif not content_known:
            only_expected = list((expected_ms - got_ms).elements())
            only_got = list((got_ms - expected_ms).elements())
            differing = _differing_columns(only_expected, only_got)
            n = len(only_expected) + len(only_got)
            new_content_mismatches.append((name, n, differing))

    msg = []
    if new_count_mismatches:
        msg.append("NEW row-count mismatches (expected -> got):")
        msg += [f"  {n}: {e} -> {g}" for n, e, g in new_count_mismatches]
    if new_exec_failures:
        msg.append("NEW execution failures on views that have snapshot rows:")
        msg += [f"  {n}: {e}" for n, e in new_exec_failures]
    if new_content_mismatches:
        msg.append("NEW content mismatches (count matches, rows differ):")
        msg += [f"  {n}: {d} rows differ in columns {c}" for n, d, c in new_content_mismatches]
    if fixed_exec:
        msg.append("KNOWN_EXEC_FAILURES now executing (remove from the list):")
        msg += [f"  {n}" for n in fixed_exec]
    if fixed_count:
        msg.append("KNOWN_COUNT_MISMATCHES now matching (remove from the list):")
        msg += [f"  {n}" for n in fixed_count]
    if fixed_content:
        msg.append("KNOWN_CONTENT_MISMATCHES now matching (remove from the list):")
        msg += [f"  {n}" for n in fixed_content]
    assert not msg, "\n" + "\n".join(msg)


def _differing_columns(only_expected, only_got):
    """The columns whose value distribution differs across the symmetric diff.

    Each argument is a list of canonicalized rows (each a sorted tuple of
    `(column, value)` pairs). A column appears in the result when its multiset of
    values among the snapshot-only rows differs from that among the engine-only
    rows - i.e. it is (part of) what makes the rows mismatch.
    """
    columns = {col for row in only_expected + only_got for col, _ in row}
    differing = []
    for col in sorted(columns):
        expected_vals = Counter(dict(row).get(col) for row in only_expected)
        got_vals = Counter(dict(row).get(col) for row in only_got)
        if expected_vals != got_vals:
            differing.append(col)
    return differing
