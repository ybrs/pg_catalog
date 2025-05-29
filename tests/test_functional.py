# Integration tests that start the pg_catalog server and run basic queries over pgwire.
# Ensures the server behaves like PostgreSQL for fundamental cases.

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


def test_set_and_show_application_name(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SET application_name = 'pytest'")
        cur.execute("SHOW application_name")
        row = cur.fetchone()
        assert row == ("application_name", "pytest")


def test_show_datestyle(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SHOW datestyle")
        row = cur.fetchone()
        assert row == ("datestyle", "ISO, MDY")


def test_show_search_path(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SHOW search_path")
        row = cur.fetchone()
        assert row == ("search_path", '"$user", public')


def test_current_user(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT current_database(), current_schema(), current_user")
        row = cur.fetchone()
    assert row == ("pgtry", "public", "dbuser")


def test_current_schemas(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT * FROM unnest(current_schemas(true))")
        rows = cur.fetchall()
        assert rows == [("pg_catalog",), ("public",)]


def test_show_transaction_isolation_level(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SHOW TRANSACTION ISOLATION LEVEL")
        row = cur.fetchone()
        assert row == ("read committed",)


def test_discard_all(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("DISCARD ALL")
        assert cur.statusmessage == "DISCARD ALL"

def test_discard_all_semicolon(server):
    """DISCARD ALL with a trailing semicolon should be accepted."""
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("DISCARD ALL;")
        assert cur.statusmessage == "DISCARD ALL"

def test_system_columns_virtual(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT xmin FROM pg_catalog.pg_namespace LIMIT 1")
        row = cur.fetchone()
        assert row[0] == 1

def test_system_columns_hidden_from_star(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT * FROM pg_catalog.pg_namespace LIMIT 1")
        columns = [d.name for d in cur.description]
        assert "xmin" not in columns


def test_conexclop_unnest(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(
            "select array(select unnest from unnest(C.conexclop)) from pg_catalog.pg_constraint C limit 1"
        )
        row = cur.fetchone()
        assert row[0] is None


def test_conexclop_regoper_cast(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(
            "select array(select unnest::regoper::varchar from unnest(C.conexclop)) from pg_catalog.pg_constraint C limit 1"
        )
        row = cur.fetchone()
        assert row[0] is None

        cur.execute(
            "select conexclop::regoper::text from pg_catalog.pg_constraint limit 1"
        )
        row = cur.fetchone()
        assert row[0] is None


def test_pg_tablespace_location_alias(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT pg_catalog.pg_tablespace_location('pg_default')")
        row = cur.fetchone()
        assert row == (None,)


def test_cast_column_oid(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT amhandler::oid FROM pg_catalog.pg_am LIMIT 1")
        row = cur.fetchone()
        assert row[0] is None


def test_oid_parameter(server):
    """Parameters typed as OID should be accepted and decoded."""
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(
            "SELECT nspname FROM pg_catalog.pg_namespace WHERE oid = %s::oid",
            (11,),
        )
        row = cur.fetchone()
        assert row == ("pg_catalog",)


def test_name_cast_literal(server):
    """Casting literals to the NAME type should return text."""
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT '_RETURN'::name")
        row = cur.fetchone()
    assert row == ("_RETURN",)


def test_server_version_function(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT version()")
        row = cur.fetchone()
        assert "17.4.0" in row[0]


def test_quote_ident_and_translate(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT pg_catalog.quote_ident('tbl')")
        row = cur.fetchone()
        assert row == ('tbl',)

        cur.execute("SELECT pg_catalog.translate('abc','a','b')")
        row = cur.fetchone()
        assert row == ('bbc',)


def test_getdef_functions(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT pg_catalog.pg_get_viewdef(1)")
        row = cur.fetchone()
        assert row == (None,)

        cur.execute("SELECT pg_catalog.pg_get_function_arguments(1)")
        row = cur.fetchone()
        assert row == (None,)

        cur.execute("SELECT pg_catalog.pg_get_indexdef(1)")
        row = cur.fetchone()
        assert row == (None,)

        cur.execute("SELECT pg_catalog.pg_get_function_result(1)")
        row = cur.fetchone()
        assert row == (None,)

        cur.execute("SELECT pg_catalog.pg_get_function_sqlbody(1)")
        row = cur.fetchone()
        assert row == (None,)

def test_misc_missing_functions(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()

        cur.execute("SELECT pg_catalog.encode(NULL::bytea, 'escape')")
        row = cur.fetchone()
        assert row == (None,)

        cur.execute("SELECT pg_catalog.pg_get_triggerdef(1)")
        row = cur.fetchone()
        assert row == (None,)

        cur.execute("SELECT pg_catalog.upper('abc')")
        row = cur.fetchone()
        assert row == ('ABC',)

        cur.execute("SELECT pg_catalog.pg_get_ruledef(1)")
        row = cur.fetchone()
        assert row == (None,)

def test_encode_bytea_column(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(
            "SELECT pg_catalog.encode(T.tgargs, 'escape') "
            "FROM pg_catalog.pg_trigger T LIMIT 0"
        )
        assert cur.fetchall() == []

def test_pg_get_expr_int64(server):
    """pg_get_expr should accept BIGINT arguments produced by ::oid casts."""
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT pg_catalog.pg_get_expr('hello', 1::oid)")
        row = cur.fetchone()
        assert row == ("hello",)


def test_has_database_privilege(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT pg_catalog.has_database_privilege(1, 'CREATE')")
        row = cur.fetchone()
        assert row == (True,)

        cur.execute("SELECT pg_catalog.has_database_privilege('pgtry', 'CONNECT')")
        row = cur.fetchone()
        assert row == (True,)


def test_has_schema_privilege(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT pg_catalog.has_schema_privilege(1, 'CREATE')")
        row = cur.fetchone()
        assert row == (True,)

        cur.execute("SELECT pg_catalog.has_schema_privilege('public', 'USAGE')")
        row = cur.fetchone()
        assert row == (True,)


def test_pg_index_access_method(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(
            """
            select tab.oid               table_id,
                   tab.relkind           table_kind,
                   ind_stor.relname      index_name,
                   ind_head.indexrelid   index_id,
                   ind_stor.xmin         state_number,
                   ind_head.indisunique  is_unique,
                   ind_head.indisprimary is_primary,
                   false                 nulls_not_distinct,
                   pg_catalog.pg_get_expr(ind_head.indpred, ind_head.indrelid) as condition,
                   (select pg_catalog.array_agg(inhparent::bigint order by inhseqno)::varchar
                      from pg_catalog.pg_inherits where ind_stor.oid = inhrelid) as ancestors,
                   ind_stor.reltablespace tablespace_id,
                   ind_stor.relam        as access_method_id
            from pg_catalog.pg_class tab
                 join pg_catalog.pg_index ind_head
                      on ind_head.indrelid = tab.oid
                 join pg_catalog.pg_class ind_stor
                      on tab.relnamespace = ind_stor.relnamespace and ind_stor.oid = ind_head.indexrelid
            where tab.relnamespace = %s::oid
              and tab.relkind in ('r','m','v','p')
              and ind_stor.relkind in ('i','I')
              and pg_catalog.age(ind_stor.xmin) <= coalesce(nullif(greatest(pg_catalog.age(%s::varchar::xid), -1), -1), 2147483647)
            limit 1
            """,
            (11, 0),
        )
        row = cur.fetchone()
        assert row is not None


def test_pg_opclass_any(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(
            """
            select tab.oid               table_id,
                   tab.relkind           table_kind,
                   ind_stor.relname      index_name,
                   ind_head.indexrelid   index_id,
                   ind_stor.xmin         state_number,
                   ind_head.indisunique  is_unique,
                   ind_head.indisprimary is_primary,
                   false                 nulls_not_distinct,
                   pg_catalog.pg_get_expr(ind_head.indpred, ind_head.indrelid) as condition,
                   (select pg_catalog.array_agg(inhparent::bigint order by inhseqno)::varchar
                      from pg_catalog.pg_inherits where ind_stor.oid = inhrelid) as ancestors,
                   ind_stor.reltablespace tablespace_id,
                   opcmethod as access_method_id
            from pg_catalog.pg_class tab
                 join pg_catalog.pg_index ind_head
                      on ind_head.indrelid = tab.oid
                 join pg_catalog.pg_class ind_stor
                      on tab.relnamespace = ind_stor.relnamespace and ind_stor.oid = ind_head.indexrelid
                 left join pg_catalog.pg_opclass on pg_opclass.oid = ANY(indclass)
            where tab.relnamespace = %s::oid
              and tab.relkind in ('r','m','v','p')
              and ind_stor.relkind in ('i','I')
              and pg_catalog.age(ind_stor.xmin) <= coalesce(nullif(greatest(pg_catalog.age(%s::varchar::xid), -1), -1), 2147483647)
            limit 1
            """,
            (11, 0),
        )
        row = cur.fetchone()
        assert row is not None


def test_pg_get_keywords_schema(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT * FROM pg_catalog.pg_get_keywords()")
        assert cur.fetchall() == []


def test_tuple_equality_join(server):
    """Queries using tuple equality should execute successfully."""
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(
            "SELECT attrelid FROM pg_catalog.pg_attribute A \n"
            "LEFT JOIN pg_catalog.pg_attrdef D ON (A.attrelid, A.attnum) = (D.adrelid, D.adnum) \n"
            "LIMIT 1"
        )
        # no error and result schema present
        cur.fetchall()


def test_rewrite_multiple_correlated_aliases(server):
    sql = (
        "SELECT (SELECT adbin FROM pg_catalog.pg_attrdef WHERE adrelid = cls.oid "
        "AND adnum = attr.attnum) AS default "
        "FROM pg_catalog.pg_attribute AS attr "
        "JOIN pg_catalog.pg_type AS typ ON attr.atttypid = typ.oid "
        "JOIN pg_catalog.pg_class AS cls ON cls.oid = attr.attrelid "
        "JOIN pg_catalog.pg_namespace AS ns ON ns.oid = cls.relnamespace"
    )
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(sql)
        cur.fetchone()


def test_trigger_count_subqueries(server):
    sql = (
        "SELECT  rel.oid,"
        "        (SELECT count(*) FROM pg_catalog.pg_trigger WHERE tgrelid=rel.oid AND tgisinternal = FALSE) AS triggercount,"
        "        (SELECT count(*) FROM pg_catalog.pg_trigger WHERE tgrelid=rel.oid AND tgisinternal = FALSE AND tgenabled = 'O') AS has_enable_triggers,"
        "        (CASE WHEN rel.relkind = 'p' THEN true ELSE false END) AS is_partitioned,"
        "        nsp.nspname AS schema,"
        "        nsp.oid AS schemaoid,"
        "        rel.relname AS name,"
        "        CASE WHEN nsp.nspname like 'pg_%' or nsp.nspname = 'information_schema' THEN true ELSE false END as is_system "
        "FROM    pg_catalog.pg_class rel "
        "INNER JOIN pg_catalog.pg_namespace nsp ON rel.relnamespace= nsp.oid "
        "WHERE rel.relkind IN ('r','t','f','p') "
        "AND NOT rel.relispartition "
        "ORDER BY nsp.nspname, rel.relname"
    )
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(sql)
        cur.fetchall()





def test_error_logging():
    proc = subprocess.Popen([
        "cargo", "run", "--quiet", "--",
        "pg_catalog_data/pg_schema",
        "--default-catalog", "pgtry",
        "--default-schema", "public",
        "--host", "127.0.0.1",
        "--port", "5445",
    ], text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)

    for _ in range(12):
        try:
            with psycopg.connect("host=127.0.0.1 port=5445 dbname=pgtry user=dbuser password=pencil sslmode=disable"):
                break
        except Exception:
            time.sleep(5)
    else:
        proc.terminate()
        raise RuntimeError("server failed to start")

    try:
        with psycopg.connect("host=127.0.0.1 port=5445 dbname=pgtry user=dbuser password=pencil sslmode=disable") as conn:
            cur = conn.cursor()
            with pytest.raises(Exception):
                cur.execute("SELECT * FROM missing_table")
    finally:
        proc.terminate()
        try:
            out, _ = proc.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            out, _ = proc.communicate()

    assert "exec_error" in out
    assert "missing_table" in out
