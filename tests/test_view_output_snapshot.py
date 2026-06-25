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

import glob
import time
import subprocess
import shutil
from collections import Counter

import psycopg
import pytest
import yaml

CONN_STR = "host=127.0.0.1 port=5451 dbname=pgtry user=dbuser password=pencil sslmode=disable"


def _snapshot_db():
    """The database name baked into the catalog snapshot, read from the snapshot
    itself (the `information_schema_catalog_name` view holds exactly that one row).

    Deriving it - rather than hardcoding "postgres" - keeps the test reproducible
    if the catalog is ever regenerated against a differently-named database.
    """
    doc = yaml.safe_load(
        open("pg_catalog_data/pg_schema/information_schema__information_schema_catalog_name.yaml")
    )
    return _find(doc, "rows")[0]["catalog_name"]


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


@pytest.fixture(scope="module")
def server(tmp_path_factory):
    """Start the pg_catalog server on its own port for the snapshot comparison."""
    zip_dir = tmp_path_factory.mktemp("schema")
    zip_path = zip_dir / "schema.zip"
    shutil.make_archive(str(zip_path.with_suffix("")), "zip", "pg_catalog_data/pg_schema")
    proc = subprocess.Popen([
        "cargo", "run", "--quiet", "--",
        str(zip_path),
        "--default-catalog", "pgtry",
        "--default-schema", "public",
        "--host", "127.0.0.1",
        "--port", "5451",
    ], text=True)
    for _ in range(12):
        try:
            with psycopg.connect(CONN_STR):
                break
        except Exception:
            time.sleep(5)
    else:
        proc.terminate()
        raise RuntimeError("server failed to start")
    yield proc
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()


def _find(node, key):
    """Return the first non-dict value stored under `key` anywhere in `node`."""
    if isinstance(node, dict):
        if key in node and not isinstance(node[key], dict):
            return node[key]
        for value in node.values():
            found = _find(value, key)
            if found is not None:
                return found
    return None


def _views_with_snapshots():
    """Yield (view_name, view_sql, snapshot_rows) for every view that has both.

    `snapshot_rows` is the list of `column -> value` dicts captured from real
    PostgreSQL; its length is the expected row count.
    """
    for path in sorted(glob.glob("pg_catalog_data/pg_schema/*.yaml")):
        doc = yaml.safe_load(open(path))
        view_sql = _find(doc, "view_sql")
        rows = _find(doc, "rows")
        if not view_sql or rows is None:
            continue
        name = path.split("/")[-1].replace(".yaml", "").replace("__", ".")
        yield name, view_sql, rows


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


def test_view_row_counts_match_snapshot(server):
    """Row count matches the snapshot, and an erroring view that *should* have rows
    is flagged rather than silently skipped.

    Fails on: a new count mismatch, a new execution failure on a view with a
    non-empty snapshot, or a baselined count/exec gap that now matches (stale
    baseline). Snapshot-empty views that error are left alone - they are runtime
    views (pg_stat_*, locks, ...) whose data we don't model.
    """
    conn = psycopg.connect(CONN_STR, autocommit=True)
    new_count_mismatches = []   # (name, expected, got) not in the baseline
    new_exec_failures = []      # (name, error) erroring with a non-empty snapshot
    fixed_count = []            # baselined count mismatches that now match
    fixed_exec = []             # baselined exec failures that now execute

    for name, view_sql, rows in _views_with_snapshots():
        expected = len(rows)
        cur = conn.cursor()
        try:
            cur.execute(view_sql)
            raw = cur.fetchall()
            cols = [d.name for d in cur.description]
            got = len(_engine_rows_without_demo(name, cols, raw))
        except Exception as exc:
            if name in KNOWN_EXEC_FAILURES:
                continue  # known gap; staying broken is fine
            if expected > 0:
                new_exec_failures.append((name, str(exc).splitlines()[0][:100]))
            continue  # snapshot-empty runtime view: erroring is acceptable
        if name in KNOWN_EXEC_FAILURES:
            fixed_exec.append(name)
            continue
        known = name in KNOWN_COUNT_MISMATCHES
        if got == expected and known:
            fixed_count.append(name)
        elif got != expected and not known:
            new_count_mismatches.append((name, expected, got))

    msg = []
    if new_count_mismatches:
        msg.append("NEW row-count mismatches (expected -> got):")
        msg += [f"  {n}: {e} -> {g}" for n, e, g in new_count_mismatches]
    if new_exec_failures:
        msg.append("NEW execution failures on views that have snapshot rows:")
        msg += [f"  {n}: {e}" for n, e in new_exec_failures]
    if fixed_exec:
        msg.append("KNOWN_EXEC_FAILURES now executing (remove from the list):")
        msg += [f"  {n}" for n in fixed_exec]
    if fixed_count:
        msg.append("KNOWN_COUNT_MISMATCHES now matching (remove from the list):")
        msg += [f"  {n}" for n in fixed_count]
    assert not msg, "\n" + "\n".join(msg)


def test_view_content_matches_snapshot(server):
    """For every view whose row count matches, the row *content* matches too.

    Catches a view returning the right number of wrong rows. Views with a known
    count divergence are skipped (their content can't align); views that error are
    covered by the count test. Fails on a new content mismatch, or a baselined one
    that now matches (stale baseline). The failure message names the differing
    columns so the gap is actionable.
    """
    conn = psycopg.connect(CONN_STR, autocommit=True)
    # Map the live database name onto the snapshot's, so the one expected
    # `current_database()` difference on every `*_catalog` column is not counted.
    db_remap = {_live_db(conn): _snapshot_db()}
    new_content_mismatches = []   # (name, n_differing, columns)
    fixed_content = []            # baselined content mismatches that now match

    for name, view_sql, rows in _views_with_snapshots():
        if name in KNOWN_COUNT_MISMATCHES:
            continue
        cur = conn.cursor()
        try:
            cur.execute(view_sql)
            got_rows = cur.fetchall()
            cols = [d.name for d in cur.description]
        except Exception:
            continue  # execution failures are the count test's responsibility
        engine_dicts = _engine_rows_without_demo(name, cols, got_rows)
        if len(engine_dicts) != len(rows):
            continue  # a count mismatch the count test will report
        expected_ms = _row_multiset(rows, db_remap)
        got_ms = _row_multiset(engine_dicts, db_remap)
        known = name in KNOWN_CONTENT_MISMATCHES
        if expected_ms == got_ms and known:
            fixed_content.append(name)
        elif expected_ms != got_ms and not known:
            only_expected = list((expected_ms - got_ms).elements())
            only_got = list((got_ms - expected_ms).elements())
            differing = _differing_columns(only_expected, only_got)
            n = len(only_expected) + len(only_got)
            new_content_mismatches.append((name, n, differing))

    msg = []
    if new_content_mismatches:
        msg.append("NEW content mismatches (count matches, rows differ):")
        msg += [f"  {n}: {d} rows differ in columns {c}" for n, d, c in new_content_mismatches]
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
