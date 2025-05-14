import psycopg

conn = psycopg.connect("host=127.0.0.1 port=5444 dbname=pgtry password=pencil sslmode=disable")
cur = conn.cursor()
res = cur.execute("SELECT datname from pg_catalog.pg_database where datallowconn")
for row in res.fetchall():
    print(row)