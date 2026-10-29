"""Log-extraction helper behavior: formatting and deduplication.

Covers `extract_queries_from_postgres_logs.extract_queries_from_log`, which
turns raw `PostgreSQL` log text into the normalized query fixtures
(`queries.yaml`) the catalog tests draw on.
"""

from extract_queries_from_postgres_logs import extract_queries_from_log


def test_extract_formats_and_deduplicates_queries(tmp_path):
    """Two case-variants of one logged query deduplicate to a single entry."""

    log = tmp_path / "postgres.log"
    log.write_text(
        "2026-01-01 12:00:00.000 statement: select id from items where id = 1\n"
        "2026-01-01 12:00:01.000 statement: SELECT id FROM items WHERE id = 1\n"
    )

    queries = extract_queries_from_log(log)

    assert len(queries) == 1
    assert queries[0]["pretty_query"] == "SELECT id\nFROM items\nWHERE id = 1"
