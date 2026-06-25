import time
import psycopg
import pytest

from conftest import conn_str, load_yaml, pg_server

PORT = 5450
CONN_STR = conn_str(PORT)


@pytest.fixture(scope="module")
def server(tmp_path_factory):
    """A capture-enabled server on its own port, yielding (process, capture file).

    These tests assert on what the server records to its `--capture` file, so they need a
    dedicated process with capturing enabled rather than the shared read-only server.
    """
    cap_file = tmp_path_factory.mktemp("cap") / "capture.yaml"
    with pg_server(PORT, capture=cap_file) as proc:
        yield proc, cap_file


def test_datacl_capture(server):
    proc, cap_file = server
    query = (
        "SELECT db.oid,db.* FROM pg_catalog.pg_database db WHERE 1 = 1 "
        "AND datallowconn AND NOT datistemplate OR db.datname ='pgtry' "
        "ORDER BY db.datname"
    )
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(query)
        # no fetch - we just want the server to execute

    time.sleep(1)
    data = load_yaml(cap_file)

    entry = next(e for e in data if e["query"].startswith("SELECT db.oid"))
    assert entry["result"][0]["datacl"] == ["=Tc/dbuser", "dbuser=CTc/dbuser"]


def test_text_values_quoted(server):
    proc, cap_file = server
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(
            "select * from pg_catalog.pg_settings where name=%s",
            ("standard_conforming_strings",),
        )
        cur.fetchone()

    time.sleep(1)
    with open(cap_file) as f:
        text = f.read()

    assert 'boot_val: "on"' in text
