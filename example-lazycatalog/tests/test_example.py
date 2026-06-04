import os
import subprocess

EXAMPLE_DIR = os.path.join(os.path.dirname(__file__), "..")


def run_example(query: str) -> str:
    """Run the example binary with `query` and return its stdout."""
    result = subprocess.run(
        ["cargo", "run", "--quiet", "--", query],
        cwd=EXAMPLE_DIR,
        text=True,
        capture_output=True,
        check=True,
    )
    return result.stdout


def test_sqlite_data_query():
    """A plain data query is routed to SQLite."""
    out = run_example("SELECT name FROM users ORDER BY id")
    assert "Alice" in out and "Bob" in out


def test_lazy_tables_in_pg_class():
    """The live SQLite tables appear in pg_class via the lazy source."""
    out = run_example(
        "SELECT relname FROM pg_catalog.pg_class "
        "WHERE relname IN ('users','orders') ORDER BY relname"
    )
    assert "users" in out and "orders" in out


def test_lazy_catalog_join():
    """pg_class ⋈ pg_namespace ⋈ pg_attribute resolves to the table's columns."""
    out = run_example(
        "SELECT a.attname FROM pg_catalog.pg_class c "
        "JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace "
        "JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid "
        "WHERE c.relname = 'users' ORDER BY a.attnum"
    )
    assert "id" in out and "name" in out


def test_information_schema_columns():
    """information_schema.columns reflects the live columns of a table."""
    out = run_example(
        "SELECT column_name, data_type FROM information_schema.columns "
        "WHERE table_name = 'orders' ORDER BY ordinal_position"
    )
    assert "status" in out and "text" in out


def test_builtins_survive_merge():
    """Built-in pg_type rows still resolve alongside the lazy user rows."""
    out = run_example("SELECT oid FROM pg_catalog.pg_type WHERE typname = 'int4'")
    assert "23" in out
