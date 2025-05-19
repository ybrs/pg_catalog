import unittest
import psycopg
import subprocess
import time


def _connect_with_retry(conninfo: str, attempts: int = 8, delay: int = 5):
    """Attempt to connect multiple times before giving up."""
    last_err = None
    for _ in range(attempts):
        try:
            return psycopg.connect(conninfo)
        except Exception as exc:  # pragma: no cover - best effort for connection
            last_err = exc
            time.sleep(delay)
    raise RuntimeError(f"could not connect: {last_err}")

class TestPsycopgQueries(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.procs = []
        for port in (5434, 5444):
            proc = subprocess.Popen([
                "cargo",
                "run",
                "--quiet",
                "--",
                "pg_catalog_data/pg_schema",
                "--default-catalog",
                "pgtry",
                "--default-schema",
                "public",
                "--host",
                "127.0.0.1",
                "--port",
                str(port),
            ], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            conninfo = f"host=127.0.0.1 port={port} dbname=postgres password=pencil sslmode=disable"
            for _ in range(8):
                try:
                    with psycopg.connect(conninfo):
                        break
                except Exception:
                    time.sleep(5)
            else:
                proc.terminate()
                raise RuntimeError(f"server on port {port} failed to start")
            cls.procs.append(proc)

    @classmethod
    def tearDownClass(cls):
        for proc in cls.procs:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
    def test_real_postgres(self):
        conn = _connect_with_retry(
            "host=127.0.0.1 port=5434 dbname=postgres user=dbuser password=pencil sslmode=disable"
        )
        cur = conn.cursor()
        cur.execute("SELECT reltype FROM pg_catalog.pg_class WHERE 1<>1 LIMIT 1")
        self.assertEqual([desc.name for desc in cur.description], ['reltype'])
        conn.close()

    def test_simple_query(self):
        conn = _connect_with_retry("host=127.0.0.1 port=5444 dbname=postgres password=pencil sslmode=disable")
        cur = conn.cursor()
        cur.execute("SELECT reltype FROM pg_catalog.pg_class WHERE 1<>1 LIMIT 1")
        self.assertEqual([desc.name for desc in cur.description], ['reltype'])
        conn.close()

    def test_extended_query(self):
        conn = _connect_with_retry("host=127.0.0.1 port=5444 dbname=postgres password=pencil sslmode=disable")
        cur = conn.cursor()
        cur.execute("SELECT reltype FROM pg_catalog.pg_class WHERE 1<>1 LIMIT 1 OFFSET %s", (0,))
        self.assertEqual([desc.name for desc in cur.description], ['reltype'])
        conn.close()

if __name__ == "__main__":
    unittest.main()
