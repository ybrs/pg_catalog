import argparse
from dataclasses import dataclass
from pathlib import Path
from typing import Dict, Iterable, Iterator, List, Optional

import psycopg
import yaml


DEFAULT_CONN_STR = (
    "host=127.0.0.1 port=5444 dbname=pgtry user=dbuser password=pencil sslmode=disable"
)


@dataclass(frozen=True)
class ViewDefinition:
    catalog: str
    schema: str
    name: str
    sql: str
    source_file: Path

    @property
    def qualified_name(self) -> str:
        return f"{self.schema}.{self.name}"


@dataclass
class ViewResult:
    view: ViewDefinition
    status: str  # "success", "missing_function", "error"
    error: Optional[str] = None


def collect_view_definitions(schema_dir: Path) -> List[ViewDefinition]:
    views: List[ViewDefinition] = []
    for path in sorted(schema_dir.glob("pg_catalog__*.yaml")):
        with path.open("r", encoding="utf-8") as fh:
            data = yaml.safe_load(fh)
        views.extend(_extract_views_from_yaml(data, path))
    return views


def _extract_views_from_yaml(data: Dict, source: Path) -> Iterator[ViewDefinition]:
    stack: List[tuple[List[str], Dict]] = [([], data)]
    while stack:
        prefix, node = stack.pop()
        if not isinstance(node, dict):
            continue
        node_type = node.get("type")
        view_sql = node.get("view_sql")
        if node_type == "view" and view_sql:
            if len(prefix) >= 3:
                catalog, schema, name = prefix[-3:]
            else:
                # Fallback if the structure is unexpected.
                catalog = prefix[0] if prefix else "unknown_catalog"
                schema = prefix[1] if len(prefix) >= 2 else "unknown_schema"
                name = prefix[-1] if prefix else source.stem
            yield ViewDefinition(
                catalog=catalog,
                schema=schema,
                name=name,
                sql=view_sql,
                source_file=source,
            )
            continue
        for key, value in node.items():
            stack.append((prefix + [key], value))


def run_view_query(conn: psycopg.Connection, view: ViewDefinition) -> ViewResult:
    sql = view.sql.strip()
    if not sql:
        return ViewResult(view=view, status="error", error="empty view_sql")

    try:
        with conn.cursor() as cur:
            cur.execute(sql)
        return ViewResult(view=view, status="success")
    except psycopg.errors.UndefinedFunction as exc:
        return ViewResult(view=view, status="missing_function", error=str(exc).strip())
    except Exception as exc:  # noqa: BLE001
        return ViewResult(view=view, status="error", error=str(exc).strip())


def generate_report(results: Iterable[ViewResult]) -> str:
    results = list(results)
    total = len(results)
    success = sum(1 for r in results if r.status == "success")
    missing_function = sum(1 for r in results if r.status == "missing_function")
    failures = sum(1 for r in results if r.status == "error")

    lines = [
        f"Processed {total} views",
        f"  Success: {success}",
        f"  Missing functions: {missing_function}",
        f"  Other failures: {failures}",
    ]

    if missing_function:
        lines.append("\nViews failing due to missing functions:")
        for result in results:
            if result.status == "missing_function":
                lines.append(
                    f"- {result.view.qualified_name} ({result.view.source_file.name}): {result.error}"
                )

    if failures:
        lines.append("\nViews failing with other errors:")
        for result in results:
            if result.status == "error":
                lines.append(
                    f"- {result.view.qualified_name} ({result.view.source_file.name}): {result.error}"
                )

    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Validate pg_catalog view definitions against a running dev server."
    )
    parser.add_argument(
        "--schema-dir",
        type=Path,
        default=Path("pg_catalog_data/pg_schema"),
        help="Directory containing pg_catalog__*.yaml files",
    )
    parser.add_argument(
        "--conn-str",
        default=DEFAULT_CONN_STR,
        help="Connection string for the running dev server",
    )
    args = parser.parse_args()

    views = collect_view_definitions(args.schema_dir)
    if not views:
        raise SystemExit("No view definitions found.")

    with psycopg.connect(args.conn_str, autocommit=True) as conn:
        results = [run_view_query(conn, view) for view in views]

    print(generate_report(results))


if __name__ == "__main__":
    main()
