#!/bin/bash

set -e

POSTGRES_DIR="./postgres-17"
DATA_DIR="./postgres-data"
PORT=5434
LOG_DIR="./pg_log"
LOG_FILENAME="postgresql-%Y-%m-%d_%H%M%S.log"

# Check if data directory exists, if not, initialize it
if [ ! -d "$DATA_DIR" ]; then
  echo "Initializing new database at $DATA_DIR"
  "$POSTGRES_DIR/bin/initdb" -D "$DATA_DIR"
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
