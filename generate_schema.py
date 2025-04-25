import psycopg
import yaml
import sys

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
              AND c.relkind IN ('r', 'v')  -- r = table, v = view
        """, (schema,))
        return cur.fetchall()

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
        for colname, udt_name in columns_info:
            mapped_type = map_pg_type(udt_name)
            schema_info[colname] = mapped_type

        cur.execute(f'SELECT * FROM "{schema}"."{table}"')
        colnames = [desc.name for desc in cur.description]
        rows = []
        for record in cur.fetchall():
            row = {}
            for key, value in zip(colnames, record):
                row[key] = value
            rows.append(row)

        return schema_info, rows

def fetch_view_definition(conn, schema, view):
    with conn.cursor() as cur:
        cur.execute("""
            SELECT pg_get_viewdef(%s::regclass, true)
        """, (f"{schema}.{view}",))
        result = cur.fetchone()
        return result[0] if result else None

def main():
    if len(sys.argv) < 2:
        print(f"Usage: {sys.argv[0]} output.yaml")
        sys.exit(1)

    output_file = sys.argv[1]

    conn = psycopg.connect("host=localhost port=5434 dbname=postgres")

    root = {"public": {}}

    for schema_name in ["pg_catalog", "information_schema"]:
        print(f"processing schema: {schema_name}")
        objects = fetch_objects(conn, schema_name)
        schema_data = {}
        for objname, relkind in objects:
            print(f"  processing object: {objname}")
            entry = {}

            if relkind == "r":  # Table
                if schema_name == "pg_catalog":
                    entry["type"] = "system_catalog"
                else:
                    entry["type"] = "table"
                table_schema, table_rows = fetch_table_schema_and_rows(conn, schema_name, objname)
                entry["schema"] = table_schema
                entry["rows"] = table_rows
            elif relkind == "v":  # View
                entry["type"] = "view"
                view_sql = fetch_view_definition(conn, schema_name, objname)
                entry["view_sql"] = view_sql
                # Still try to fetch columns
                table_schema, table_rows = fetch_table_schema_and_rows(conn, schema_name, objname)
                entry["schema"] = table_schema
                entry["rows"] = table_rows

            schema_data[objname] = entry

        root["public"][schema_name] = schema_data

    with open(output_file, "w") as f:
        yaml.dump(root, f, sort_keys=False, allow_unicode=True)

    print(f"Saved schema to {output_file}")

if __name__ == "__main__":
    main()
