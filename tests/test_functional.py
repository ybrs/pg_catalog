import os
import time
import subprocess
import psycopg
import pytest

CONN_STR = "host=127.0.0.1 port=5444 dbname=pgtry user=dbuser password=pencil sslmode=disable"

@pytest.fixture(scope="module")
def server():
    proc = subprocess.Popen([
        "cargo", "run", "--quiet", "--",
        "pg_catalog_data/pg_schema",
        "--default-catalog", "pgtry",
        "--default-schema", "public",
        "--host", "127.0.0.1",
        "--port", "5444",
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

def test_query_returns_text(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT relname FROM pg_catalog.pg_class LIMIT 1")
        row = cur.fetchone()
        assert isinstance(row[0], str)

def test_query_returns_int(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT reltype FROM pg_catalog.pg_class LIMIT 1")
        row = cur.fetchone()
        assert isinstance(row[0], int)

def test_parameter_query(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(
            "SELECT count(*) FROM pg_catalog.pg_class WHERE relname = %s",
            ("pg_class",),
        )
        row = cur.fetchone()
        assert row[0] >= 1

def test_pg_get_one_subquery(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT pg_get_one((select relname FROM pg_catalog.pg_class LIMIT 1))")
        row = cur.fetchone()
        assert row[0] is not None

def test_pg_get_array_subquery(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT relname FROM pg_catalog.pg_class LIMIT 1")
        expected = cur.fetchone()[0]

        cur.execute("SELECT pg_get_array((SELECT relname FROM pg_catalog.pg_class LIMIT 1))")
        raw = cur.pgresult.get_value(0, 0).decode()

        if raw.startswith('"') and raw.endswith('"'):
            raw = raw[1:-1]

        assert raw.startswith("{") and raw.endswith("}")
        items = raw[1:-1].split(',') if raw != '{}' else []
        assert items == [expected]

def test_empty_result_schema(server):
    """Ensure that queries returning no rows still expose column metadata."""
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT relname FROM pg_catalog.pg_class WHERE false")
        assert cur.fetchall() == []
        assert cur.description[0].name == "relname"
        # OID 25 is the TEXT type returned by our server for name columns
        assert cur.description[0].type_code == 25
