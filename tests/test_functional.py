# Integration tests that start the pg_catalog server and run basic queries over pgwire.
# Ensures the server behaves like PostgreSQL for fundamental cases.

import subprocess

import psycopg
import pytest

from conftest import SHARED_PORT, conn_str, load_yaml, pg_server, server  # noqa: F401

CONN_STR = conn_str(SHARED_PORT)

# Distinct ports for tests that need their own server process with special arguments.
ERROR_LOGGING_PORT = 5445
CAPTURE_OPTION_PORT = 5446

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


def test_text_array_return(server):
    """Arrays of text should be decoded as Python lists."""
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()

        cur.execute(
            "SELECT datacl FROM pg_catalog.pg_database WHERE datname = 'postgres'"
        )
        row = cur.fetchone()
        assert row == (None,)

        cur.execute(
            "SELECT datacl FROM pg_catalog.pg_database WHERE datname = 'template1'"
        )
        row = cur.fetchone()
        assert row[0] == ["=c/sysuser", "sysuser=CTc/sysuser"]

        cur.execute(
            "SELECT datacl FROM pg_catalog.pg_database WHERE datname = 'pgtry'"
        )
        row = cur.fetchone()
        assert row[0] == ["=Tc/dbuser", "dbuser=CTc/dbuser"]

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
        assert row == ("pytest",)

        cur.execute("SHOW application_name;")
        row = cur.fetchone()
        assert row == ("pytest",)


def test_show_datestyle(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SHOW datestyle")
        row = cur.fetchone()
        assert row == ("ISO, MDY",)


def test_show_search_path(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SHOW search_path")
        row = cur.fetchone()
        assert row == ('"$user", public',)


def test_current_user(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT current_database(), current_schema(), current_user")
        row = cur.fetchone()
    assert row == ("pgtry", "pg_catalog", "dbuser")


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
    """`regproc::oid` resolves the function name the column holds against pg_proc."""
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(
            "SELECT amhandler, amhandler::oid FROM pg_catalog.pg_am "
            "WHERE amname = 'btree'"
        )
        assert cur.fetchone() == ("bthandler", 330)


def test_join_pg_proc_on_regproc_column(server):
    """A join from a regproc column to pg_proc.oid finds the named function.

    This is how Npgsql (and so Power BI) loads the type list: it joins
    `pg_proc.oid = pg_type.typreceive`. The column holds the function's NAME here, so
    the comparison only works because the rewriter resolves it to an OID first.
    """
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(
            "SELECT p.proname FROM pg_catalog.pg_type AS a "
            "JOIN pg_catalog.pg_proc AS p ON p.oid = a.typreceive "
            "WHERE a.typname = 'bool'"
        )
        assert cur.fetchone() == ("boolrecv",)

        # Every type except the ones with no receive function ('-') joins to exactly
        # one row, so the join neither drops nor duplicates types.
        cur.execute(
            "SELECT (SELECT count(*) FROM pg_catalog.pg_type AS a "
            "        JOIN pg_catalog.pg_proc AS p ON p.oid = a.typreceive), "
            "       (SELECT count(*) FROM pg_catalog.pg_type WHERE typreceive <> '-')"
        )
        joined, with_receive_function = cur.fetchone()
        assert joined == with_receive_function


def test_catalog_query_written_in_upper_case(server):
    """Upper-case catalog names resolve, because PostgreSQL folds unquoted identifiers.

    Power BI asks for table sizes with `PG_TOTAL_RELATION_SIZE(C.OID) FROM PG_CLASS C
    JOIN PG_NAMESPACE N`. Matching the written spelling against the lower-case names the
    catalog registers finds nothing, which both leaves the tables unqualified and makes
    the router hand the query to the embedding engine, which has no pg_catalog.
    """
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(
            "SELECT PG_TOTAL_RELATION_SIZE(C.OID) AS TOTAL_BYTES "
            "FROM PG_CLASS C JOIN PG_NAMESPACE N ON (N.OID = C.RELNAMESPACE) "
            "WHERE N.NSPNAME = 'pg_catalog' AND C.RELNAME = 'pg_type'"
        )
        assert cur.fetchone() == (0,)

        cur.execute("SELECT RELNAME FROM PG_CATALOG.PG_CLASS WHERE RELNAME = 'pg_type'")
        assert cur.fetchone() == ("pg_type",)


def test_order_by_name_sorts_by_the_selected_column(server):
    """ORDER BY a bare name sorts by the SELECT list column of that name.

    PostgreSQL resolves an ORDER BY name against the output columns first. Npgsql loads
    enum labels with `SELECT pg_type.oid ... JOIN pg_enum ... ORDER BY oid`, where both
    joined tables carry an `oid`, so resolving against the input instead would fail the
    query as ambiguous.
    """
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(
            "SELECT pg_type.oid, enumlabel FROM pg_catalog.pg_enum "
            "JOIN pg_catalog.pg_type ON pg_type.oid = enumtypid "
            "ORDER BY oid, enumsortorder"
        )
        assert cur.fetchall() == []

        # An output column of the same name as an input column wins, as in PostgreSQL:
        # this sorts by relname, not by pg_class.oid.
        cur.execute(
            "SELECT relname AS oid FROM pg_catalog.pg_class ORDER BY oid LIMIT 3"
        )
        sorted_by_relname = [row[0] for row in cur.fetchall()]
        assert sorted_by_relname == sorted(sorted_by_relname)


def test_regproc_column_still_reads_as_a_function_name(server):
    """Selecting a regproc column returns the function name, as PostgreSQL renders it."""
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(
            "SELECT typreceive, typinput FROM pg_catalog.pg_type WHERE typname = 'bool'"
        )
        assert cur.fetchone() == ("boolrecv", "boolin")


def test_regproc_column_compared_with_qualified_function_name(server):
    """A schema-qualified function name matches the bare name the catalog stores.

    The PostgreSQL JDBC driver decides whether a type is an array with
    `typinput = 'pg_catalog.array_in'::regproc`, while this catalog stores `array_in`.
    """
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(
            "SELECT typinput = 'pg_catalog.array_in'::regproc AS is_array "
            "FROM pg_catalog.pg_type WHERE typname IN ('_int4', 'int4') "
            "ORDER BY typname"
        )
        assert cur.fetchall() == [(True,), (False,)]


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


def test_information_schema_tables(server):
    """Ensure information_schema views are available."""
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'pg_catalog' AND table_name = 'pg_type'"
        )
        row = cur.fetchone()
        assert row == ("pg_type",)


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

def test_rewrite_trigger_counts(server):
    sql = (
        "SELECT rel.oid, "
        "(SELECT count(*) FROM pg_trigger WHERE tgrelid=rel.oid AND tgisinternal = FALSE) AS triggercount, "
        "(SELECT count(*) FROM pg_trigger WHERE tgrelid=rel.oid AND tgisinternal = FALSE AND tgenabled = 'O') AS has_enable_triggers, "
        "(CASE WHEN rel.relkind = 'p' THEN true ELSE false END) AS is_partitioned, "
        "nsp.nspname AS schema, "
        "nsp.oid AS schemaoid, "
        "rel.relname AS name, "
        "CASE WHEN nsp.nspname like 'pg_%' or nsp.nspname = 'information_schema' THEN true ELSE false END as is_system "
        "FROM pg_class rel "
        "INNER JOIN pg_namespace nsp ON rel.relnamespace= nsp.oid "
        "WHERE rel.relkind IN ('r','t','f','p') "
        "AND NOT rel.relispartition "
        "ORDER BY nsp.nspname, rel.relname"
    )
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(sql)
        cur.fetchone()





def test_error_logging():
    with pg_server(ERROR_LOGGING_PORT, pipe_output=True) as proc:
        try:
            with psycopg.connect(conn_str(ERROR_LOGGING_PORT)) as conn:
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


def test_users_dummy_data(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT * FROM users ORDER BY id")
        rows = cur.fetchall()
        assert rows == [(1, "Alice"), (2, "Bob")]


def test_capture_option(tmp_path):
    capture_file = tmp_path / "captured.yaml"
    with pg_server(CAPTURE_OPTION_PORT, capture=capture_file):
        with psycopg.connect(conn_str(CAPTURE_OPTION_PORT)) as conn:
            cur = conn.cursor()
            cur.execute("SELECT 1 AS one")
            cur.fetchone()
            cur.execute(
                "SELECT nspname FROM pg_catalog.pg_namespace WHERE oid = %s::oid",
                (11,),
            )
            cur.fetchone()
            with pytest.raises(Exception):
                cur.execute("SELECT * FROM missing_table")

    data = load_yaml(capture_file)

    assert len(data) == 3
    assert data[0]["success"] is True
    assert data[1]["parameters"] == [11]
    assert data[2]["success"] is False


def test_postmaster_time_zone_lowercase(server):
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(
            "SELECT round(EXTRACT(EPOCH FROM pg_postmaster_start_time() AT TIME ZONE 'utc')) AS startup_time"
        )
        row = cur.fetchone()
        assert isinstance(int(row[0]), int)


def test_union_all_returns_all_branches(server):
    # Regression: the wire layer pushed one result set per RecordBatch, so a
    # UNION ALL (one batch per branch) reached the client as multiple result
    # sets and only the last was seen. All branches must now come back.
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT 1 AS a UNION ALL SELECT 2 AS a UNION ALL SELECT 3 AS a")
        vals = sorted(r[0] for r in cur.fetchall())
        assert vals == [1, 2, 3], vals


def test_table_constraints_view(server):
    # Exercises a multi-branch UNION view end to end (the wire fix) plus the
    # simplified nulls_distinct. Should list real constraints (PRIMARY KEY /
    # UNIQUE) and the synthesized NOT NULL CHECKs together.
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT constraint_type FROM information_schema.table_constraints")
        types = set(r[0] for r in cur.fetchall())
        assert {"PRIMARY KEY", "CHECK"}.issubset(types), types
        cur.execute(
            "SELECT count(*) FROM information_schema.table_constraints "
            "WHERE constraint_type = 'CHECK'"
        )
        assert cur.fetchone()[0] > 0


def test_element_types_view(server):
    # Exercises the multi-column IN -> EXISTS rewrite end to end (the visibility
    # filter is a 4-column IN-subquery DataFusion can't plan natively).
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT count(*) FROM information_schema.element_types")
        assert cur.fetchone()[0] > 0


def test_constraint_column_usage_executes(server):
    # The two `nspname`s in its derived table used to trip a DataFusion assertion;
    # the duplicate-column disambiguation rewrite must let it plan and run.
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT * FROM information_schema.constraint_column_usage")
        cols = [d.name for d in cur.description]
        assert "constraint_name" in cols and "column_name" in cols, cols


def test_user_mapping_options_executes(server):
    # Its LATERAL pg_options_to_table SRF was rewritten to the projection form;
    # it must plan and return the right columns (empty until FDW mappings exist).
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute("SELECT * FROM information_schema.user_mapping_options")
        cols = [d.name for d in cur.description]
        assert "option_name" in cols and "option_value" in cols, cols
