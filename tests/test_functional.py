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
    ], stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    for _ in range(30):
        try:
            with psycopg.connect(CONN_STR):
                break
        except Exception:
            time.sleep(1)
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
