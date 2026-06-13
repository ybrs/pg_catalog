# Generates YAML descriptions of PostgreSQL schemas by querying a real server.
# Connects using psycopg, extracts table definitions and rows, and writes them for use as test fixtures.
# Included so the Rust code can run with a known catalog snapshot.

import logging
import os
import sys
import datetime
import psycopg
import yaml

logging.basicConfig(
    level=getattr(logging, os.getenv("LOG_LEVEL", "INFO").upper(), logging.INFO)
)
logger = logging.getLogger(__name__)

PG_TYPE_MAPPING = {
    "int2": "int",
    "int4": "int",
    "int8": "bigint",
    "oid": "int",
    "name": "varchar(64)",
    "text": "varchar(256)",
    "varchar": "varchar(256)",
    "bpchar": "varchar(64)",
    "bool": "boolean",
    # Keep single/double precision distinct: float4 -> Float32 (FLOAT4 / OID 700),
    # float8 -> Float64 (FLOAT8 / OID 701) on the Rust side. Collapsing both to a
    # bare "float" made every float4 column (e.g. pg_class.reltuples) report as
    # float8 over the wire.
    "float4": "float4",
    "float8": "float8",
}

def map_pg_type(pg_type):
    if pg_type.startswith("_"):
        return "_text"
    return PG_TYPE_MAPPING.get(pg_type, "varchar(256)")

# Tables/views whose contents are runtime/session state (statistics, locks,
# activity, memory, prepared statements). Their rows change on every server
# start, so we emit them with an empty `rows:` to keep the generated catalog
# deterministic (their schema is still captured). Anything matching `pg_stat*`
# (which also covers `pg_statio*`) plus the names below.
VOLATILE_TABLE_NAMES = {
    "pg_locks",
    "pg_prepared_statements",
    "pg_cursors",
    "pg_backend_memory_contexts",
    "pg_shmem_allocations",
}

def is_volatile_table(name: str) -> bool:
    return name.startswith("pg_stat") or name in VOLATILE_TABLE_NAMES

def fetch_objects(conn, schema):
    with conn.cursor() as cur:
        cur.execute("""
            SELECT c.relname, c.relkind, d.description
            FROM pg_catalog.pg_class c
            JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
            LEFT JOIN pg_catalog.pg_description d ON d.objoid = c.oid AND d.objsubid = 0
            WHERE n.nspname = %s
              AND c.relkind IN ('r', 'v')
        """, (schema,))
        return cur.fetchall()

def safe_value(value):
    if isinstance(value, (int, float, bool)) or value is None:
        return value
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        return [safe_value(v) for v in value]
    if isinstance(value, (datetime.datetime, datetime.date)):
        return value.isoformat()
    return str(value)

def fetch_table_schema_and_rows(conn, schema, table):
    with conn.cursor() as cur:
        cur.execute("""
            SELECT column_name, udt_name
            FROM information_schema.columns
            WHERE table_schema = %s AND table_name = %s
            ORDER BY ordinal_position
        """, (schema, table))
        columns_info = cur.fetchall()

        schema_info = {}
        raw_pg_types = {}
        for colname, udt_name in columns_info:
            schema_info[colname] = map_pg_type(udt_name)
            raw_pg_types[colname] = udt_name

        cur.execute(f'SELECT * FROM "{schema}"."{table}"')
        colnames = [desc.name for desc in cur.description]
        rows = []
        for record in cur.fetchall():
            row = {}
            for key, value in zip(colnames, record):
                row[key] = safe_value(value)
            rows.append(row)

        return schema_info, raw_pg_types, rows

def fetch_view_definition(conn, schema, view):
    with conn.cursor() as cur:
        cur.execute("""
            SELECT pg_get_viewdef(%s::regclass, true)
        """, (f"{schema}.{view}",))
        result = cur.fetchone()
        return result[0] if result else None

def ensure_dir(path):
    if not os.path.exists(path):
        os.makedirs(path)

def generate(output_dir):
    ensure_dir(output_dir)

    for schema_name in ["pg_catalog", "information_schema"]:
        logger.info("processing schema: %s", schema_name)
        objects = fetch_objects(conn, schema_name)

        for objname, relkind, description in objects:
            logger.info("  processing object: %s", objname)
            entry = {}

            if relkind == "r":  # Table
                entry["type"] = "system_catalog" if schema_name == "pg_catalog" else "table"
                table_schema, raw_types, table_rows = fetch_table_schema_and_rows(conn, schema_name, objname)
                entry["schema"] = table_schema
                entry["pg_types"] = raw_types
                entry["rows"] = [] if is_volatile_table(objname) else table_rows
            elif relkind == "v":  # View
                entry["type"] = "view"
                view_sql = fetch_view_definition(conn, schema_name, objname)
                table_schema, raw_types, table_rows = fetch_table_schema_and_rows(conn, schema_name, objname)
                entry["view_sql"] = view_sql
                entry["schema"] = table_schema
                entry["pg_types"] = raw_types
                entry["rows"] = [] if is_volatile_table(objname) else table_rows

            if description:
                # TODO: description is not working
                entry["description"] = description

            wrapped = {
                "public": {
                    schema_name: {
                        objname: entry
                    }
                }
            }
            out_file = os.path.join(output_dir, f"{schema_name}__{objname}.yaml")
            with open(out_file, "w") as f:
                yaml.dump(wrapped, f, sort_keys=False, allow_unicode=True)

    logger.info("Saved schemas to %s", output_dir)

def find_schema_file(output_dir, table_name):
    for fname in os.listdir(output_dir):
        if fname.endswith(".yaml") and f"__{table_name}.yaml" in fname:
            return os.path.join(output_dir, fname)
    return None

def show(output_dir, specific_table=None):
    if specific_table:
        file = find_schema_file(output_dir, specific_table)
        if not file:
            logger.error("Table %s not found in %s", specific_table, output_dir)
            sys.exit(1)
        files = [file]
    else:
        files = [os.path.join(output_dir, f) for f in os.listdir(output_dir) if f.endswith(".yaml")]

    for fpath in files:
        with open(fpath, "r") as f:
            entry = yaml.safe_load(f)

        base = os.path.basename(fpath)
        schema_table = base.replace(".yaml", "").replace("__", ".")
        logger.info("%s:", schema_table)
        logger.info("  type: %s", entry.get('type', 'unknown'))
        logger.info("  rows: %s", len(entry.get('rows', [])))

        if "description" in entry:
            logger.info("  description: %s", entry['description'])

        if specific_table:
            logger.info("  schema:")
            for col, typ in entry.get("schema", {}).items():
                logger.info(
                    "    %s: %s (%s)",
                    col,
                    typ,
                    entry.get('pg_types', {}).get(col, ""),
                )

            if "view_sql" in entry:
                logger.info("\n  view_sql:")
                logger.info(entry["view_sql"])

            example_rows = entry.get("rows", [])[:2]
            if example_rows:
                logger.info("\n  example rows:")
                for row in example_rows:
                    logger.info("    %s", row)

if __name__ == "__main__":
    if len(sys.argv) < 3:
        logger.info("Usage:")
        logger.info("  %s generate output_dir", sys.argv[0])
        logger.info("  %s show output_dir [table_name]", sys.argv[0])
        sys.exit(1)

    cmd = sys.argv[1]

    # Connect as the fixed bootstrap superuser created by run-postgres.sh
    # (-U sysuser), so catalog ownership/ACLs read "sysuser" everywhere.
    conn = psycopg.connect("host=localhost port=5434 dbname=postgres user=sysuser")


    if cmd == "generate":
        output_dir = sys.argv[2]
        generate(output_dir)
    elif cmd == "show":
        output_dir = sys.argv[2]
        table_name = sys.argv[3] if len(sys.argv) > 3 else None
        show(output_dir, table_name)
    else:
        logger.error("Unknown command %s", cmd)
        sys.exit(1)
