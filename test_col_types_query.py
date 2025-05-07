import unittest
import psycopg

class TestPsycopgQueries(unittest.TestCase):
    def test_real_postgres(self):
        conn = psycopg.connect("host=127.0.0.1 port=5434 dbname=postgres sslmode=disable")
        cur = conn.cursor()
        cur.execute("SELECT reltype FROM pg_catalog.pg_class WHERE 1<>1 LIMIT 1")
        self.assertEqual([desc.name for desc in cur.description], ['reltype'])
        conn.close()

    def test_simple_query(self):
        conn = psycopg.connect("host=127.0.0.1 port=5433 dbname=postgres password=pencil sslmode=disable")
        cur = conn.cursor()
        cur.execute("SELECT reltype FROM pg_catalog.pg_class WHERE 1<>1 LIMIT 1")
        self.assertEqual([desc.name for desc in cur.description], ['reltype'])
        conn.close()

    def test_extended_query(self):
        conn = psycopg.connect("host=127.0.0.1 port=5433 dbname=postgres password=pencil sslmode=disable")
        cur = conn.cursor()
        cur.execute("SELECT reltype FROM pg_catalog.pg_class WHERE 1<>1 LIMIT 1 OFFSET %s", (0,))
        self.assertEqual([desc.name for desc in cur.description], ['reltype'])
        conn.close()

if __name__ == "__main__":
    unittest.main()
