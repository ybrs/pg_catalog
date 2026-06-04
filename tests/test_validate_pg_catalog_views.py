from pathlib import Path

import psycopg

from test_functional import CONN_STR, server  # noqa: F401
from validate_pg_catalog_views import (
    ViewDefinition,
    collect_view_definitions,
    run_view_query,
)


def test_collect_view_definitions_finds_pg_tables():
    views = collect_view_definitions(Path("pg_catalog_data/pg_schema"))
    qualified_names = {view.qualified_name for view in views}
    assert "pg_catalog.pg_tables" in qualified_names


def test_run_view_query_success(server):  # noqa: F811
    with psycopg.connect(CONN_STR, autocommit=True) as conn:
        view = ViewDefinition(
            catalog="pgtry",
            schema="pg_catalog",
            name="one",
            sql="SELECT 1",
            source_file=Path("dummy.yaml"),
        )
        result = run_view_query(conn, view)
    assert result.status == "success"


def test_run_view_query_missing_function(server):  # noqa: F811
    with psycopg.connect(CONN_STR, autocommit=True) as conn:
        view = ViewDefinition(
            catalog="pgtry",
            schema="pg_catalog",
            name="missing_func",
            sql="SELECT missing_function_that_should_not_exist()",
            source_file=Path("dummy.yaml"),
        )
        result = run_view_query(conn, view)
    assert result.status == "missing_function"
    assert result.error is not None
    assert "does not exist" in result.error.lower()
