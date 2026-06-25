#!/bin/bash
# Convenience script to launch a local PostgreSQL instance used for generating schema snapshots.
# Initializes a data directory if needed and then starts the bundled server.


set -e

POSTGRES_DIR="./postgres-17"
DATA_DIR="./postgres-data"
PORT=5434
LOG_DIR="./pg_log"
LOG_FILENAME="postgresql-%Y-%m-%d_%H%M%S.log"

# Check if data directory exists, if not, initialize it.
# Force UTF8 encoding so the extracted catalog is consistent across machines:
# without it initdb inherits the shell locale, which yields SQL_ASCII on a
# C-locale host (e.g. CI) and UTF8 on a typical desktop.
if [ ! -d "$DATA_DIR" ]; then
  echo "Initializing new database at $DATA_DIR"
  # -U sysuser: the bootstrap superuser owns every catalog object, so its name
  # ends up in ownership/ACL rows (e.g. template1.datacl). Pin it to a fixed
  # "sysuser" so the extracted catalog is identical no matter who runs this
  # (instead of leaking the OS account name into the committed stubs).
  "$POSTGRES_DIR/bin/initdb" -D "$DATA_DIR" --encoding=UTF8 -U sysuser
fi

mkdir -p "$LOG_DIR"

# Start postgres
echo "Starting PostgreSQL from $POSTGRES_DIR on port $PORT"

"$POSTGRES_DIR/bin/postgres" -D "$DATA_DIR" -p $PORT \
  -c logging_collector=on \
  -c log_destination=stderr \
  -c log_directory="$LOG_DIR" \
  -c log_filename="$LOG_FILENAME" \
  -c log_statement=all \
  -c log_min_duration_statement=0 \
  -c log_error_verbosity=verbose
