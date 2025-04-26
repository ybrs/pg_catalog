import psycopg
import yaml
import sys
import os
import datetime

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
    "float4": "float",
    "float8": "float",
}

def map_pg_type(pg_type):
    return PG_TYPE_MAPPING.get(pg_type, "varchar(256)")

def fetch_objects(conn, schema):
    with conn.cursor() as cur:
        cur.execute("""
            SELECT c.relname, c.relkind
            FROM pg_catalog.pg_class c
            JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = %s
              AND c.relkind IN ('r', 'v')
        """, (schema,))
        return cur.fetchall()

def safe_value(value):
    if isinstance(value, (int, float, bool)) or value is None:
        return value
    if isinstance(value, (str, )):
        return value
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
                if isinstance(value, (datetime.datetime, datetime.date)):
                    row[key] = value.isoformat()
                else:
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
    conn = psycopg.connect("host=localhost port=5434 dbname=postgres")

    ensure_dir(output_dir)

    for schema_name in ["pg_catalog", "information_schema"]:
        print(f"processing schema: {schema_name}")
        objects = fetch_objects(conn, schema_name)

        for objname, relkind in objects:
            print(f"  processing object: {objname}")
            entry = {}

            if relkind == "r":  # Table
                entry["type"] = "system_catalog" if schema_name == "pg_catalog" else "table"
                table_schema, raw_types, table_rows = fetch_table_schema_and_rows(conn, schema_name, objname)
                entry["schema"] = table_schema
                entry["pg_types"] = raw_types
                entry["rows"] = table_rows
            elif relkind == "v":  # View
                entry["type"] = "view"
                view_sql = fetch_view_definition(conn, schema_name, objname)
                table_schema, raw_types, table_rows = fetch_table_schema_and_rows(conn, schema_name, objname)
                entry["view_sql"] = view_sql
                entry["schema"] = table_schema
                entry["pg_types"] = raw_types
                entry["rows"] = table_rows

            out_file = os.path.join(output_dir, f"{schema_name}__{objname}.yaml")
            with open(out_file, "w") as f:
                yaml.dump(entry, f, sort_keys=False, allow_unicode=True)

    print(f"Saved schemas to {output_dir}")

def find_schema_file(output_dir, table_name):
    for fname in os.listdir(output_dir):
        if fname.endswith(".yaml") and f"__{table_name}.yaml" in fname:
            return os.path.join(output_dir, fname)
    return None

def show(output_dir, specific_table=None):
    if specific_table:
        file = find_schema_file(output_dir, specific_table)
        if not file:
            print(f"Table {specific_table} not found in {output_dir}")
            sys.exit(1)
        files = [file]
    else:
        files = [os.path.join(output_dir, f) for f in os.listdir(output_dir) if f.endswith(".yaml")]

    for fpath in files:
        with open(fpath, "r") as f:
            entry = yaml.safe_load(f)

        base = os.path.basename(fpath)
        schema_table = base.replace(".yaml", "").replace("__", ".")
        print(f"{schema_table}:")
        print(f"  type: {entry.get('type', 'unknown')}")
        print(f"  rows: {len(entry.get('rows', []))}")

        if specific_table:
            print("  schema:")
            for col, typ in entry.get("schema", {}).items():
                print(f"    {col}: {typ} ({entry.get('pg_types', {}).get(col, '')})")

            if "view_sql" in entry:
                print("\n  view_sql:")
                print(entry["view_sql"])

            example_rows = entry.get("rows", [])[:2]
            if example_rows:
                print("\n  example rows:")
                for row in example_rows:
                    print(f"    {row}")

if __name__ == "__main__":
    if len(sys.argv) < 3:
        print(f"Usage:")
        print(f"  {sys.argv[0]} generate output_dir")
        print(f"  {sys.argv[0]} show output_dir [table_name]")
        sys.exit(1)

    cmd = sys.argv[1]
    if cmd == "generate":
        output_dir = sys.argv[2]
        generate(output_dir)
    elif cmd == "show":
        output_dir = sys.argv[2]
        table_name = sys.argv[3] if len(sys.argv) > 3 else None
        show(output_dir, table_name)
    else:
        print(f"Unknown command {cmd}")
        sys.exit(1)
