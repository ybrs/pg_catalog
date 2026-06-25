"""Shared pgwire-server startup machinery for the Python test suite.

Every test that needs a live pg_catalog server starts it the same way: through the
embedded Arrow IPC artifact (the exact fast-load path we ship), advertising catalog
`pgtry` and schema `public` over the PostgreSQL wire protocol on `127.0.0.1`. This
module owns that single startup path so the six test sites that used to copy-paste it
share one implementation.

The read-only tests share ONE session-scoped server (the `server` fixture). Tests that
need their own process with special arguments - capturing queries to a file, or piping
the server's log output to assert on it - use `pg_server(...)` directly with a distinct
port.
"""

import contextlib
import subprocess
import time

import psycopg
import pytest

from yaml_loader import load_yaml  # noqa: F401  (re-exported for the test modules)

# Single port for the shared, session-scoped read-only server. Capture/error tests pick
# their own distinct ports when they call `pg_server` directly.
SHARED_PORT = 5451


def conn_str(port):
    """Build the psycopg connection string for a server listening on `port`.

    Centralizes the user/password/database that every test connects with, so no test
    has to repeat the full DSN.
    """
    return f"host=127.0.0.1 port={port} dbname=pgtry user=dbuser password=pencil sslmode=disable"


def _server_command(port, capture=None):
    """The `cargo run` argv that launches the server through the embedded IPC artifact.

    The first positional argument is the empty string, which `parse_schema` reads as
    "use the embedded Arrow IPC catalog" - the shipped fast path - rather than parsing
    YAML off disk. When `capture` is given, the server records every executed query to
    that file via `--capture`.
    """
    command = [
        "cargo", "run", "--quiet", "--",
        "",
        "--default-catalog", "pgtry",
        "--default-schema", "public",
        "--host", "127.0.0.1",
        "--port", str(port),
    ]
    if capture is not None:
        command += ["--capture", str(capture)]
    return command


def _wait_until_ready(proc, port):
    """Block until the server on `port` accepts a connection, or raise on failure.

    Polls a real psycopg connect roughly every 0.25s for ~15s. Fails immediately if the
    process has already exited (so a crashed server surfaces at once instead of after the
    full timeout), and terminates the process before raising on timeout.
    """
    for _ in range(60):
        if proc.poll() is not None:
            raise RuntimeError(
                f"server process exited with code {proc.returncode} before becoming ready"
            )
        try:
            with psycopg.connect(conn_str(port)):
                return
        except Exception:
            time.sleep(0.25)
    proc.terminate()
    raise RuntimeError("server failed to start")


def start_pg_server(port, capture=None, pipe_output=False):
    """Start the server on `port`, wait until it is ready, and return the process.

    Always loads the embedded IPC catalog (see `_server_command`). When `capture` is
    given, executed queries are written to that file. When `pipe_output` is true, the
    server's stdout and stderr are merged into a pipe so callers can read its log output
    (used by the error-logging test); otherwise output is inherited.

    The caller owns the returned `Popen` and is responsible for terminating it; prefer the
    `pg_server` context manager, which does that automatically.
    """
    stdout = subprocess.PIPE if pipe_output else None
    stderr = subprocess.STDOUT if pipe_output else None
    proc = subprocess.Popen(
        _server_command(port, capture=capture),
        text=True,
        stdout=stdout,
        stderr=stderr,
    )
    _wait_until_ready(proc, port)
    return proc


@contextlib.contextmanager
def pg_server(port, capture=None, pipe_output=False):
    """Run a server on `port` for the duration of the `with` block, then stop it.

    Yields the running process and, on exit, terminates it - escalating to kill if it does
    not stop within five seconds. Use this for tests that need their own process with
    special arguments (`capture`, `pipe_output`) on a distinct port.
    """
    proc = start_pg_server(port, capture=capture, pipe_output=pipe_output)
    try:
        yield proc
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


@pytest.fixture(scope="session")
def server():
    """One shared, session-scoped server on `SHARED_PORT` for all read-only tests.

    Starting a single process and letting every read-only test connect to it - instead of
    one server per test module - keeps the suite fast.
    """
    with pg_server(SHARED_PORT) as proc:
        yield proc
