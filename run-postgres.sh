#!/bin/bash

set -e

POSTGRES_DIR="./postgres-17"
DATA_DIR="./postgres-data"
PORT=5434

# Check if data directory exists, if not, initialize it
if [ ! -d "$DATA_DIR" ]; then
  echo "Initializing new database at $DATA_DIR"
  "$POSTGRES_DIR/bin/initdb" -D "$DATA_DIR"
fi

# Start postgres
echo "Starting PostgreSQL from $POSTGRES_DIR on port $PORT"

"$POSTGRES_DIR/bin/postgres" -D "$DATA_DIR" -p $PORT
