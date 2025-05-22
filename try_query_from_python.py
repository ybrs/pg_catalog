# Small manual test to connect to the running pg_catalog server and issue a query.
# Serves as a quick sanity check from Python.
# Kept for developer convenience.

import psycopg

conn = psycopg.connect("host=127.0.0.1 port=5444 dbname=pgtry password=pencil sslmode=disable")
cur = conn.cursor()
res = cur.execute("SELECT datname from pg_catalog.pg_database where datallowconn")
for row in res.fetchall():
    print(row)