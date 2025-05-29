# Task 200

We want to improve startup time by compiling YAML catalog data to a single binary
representation that the server can load very quickly.

## Binary format
- 8 byte magic header `PGCAT\0\1` to allow future upgrades
- `u32` table count
- For each table:
  - `u8` length + catalog name bytes
  - `u8` length + schema name bytes
  - `u8` length + table name bytes
  - `u64` length of encoded Arrow stream
  - Arrow IPC stream containing the table schema and all rows
- Tables are stored sequentially.  The header acts as an index so the loader can
  map (catalog, schema, table) to a `RecordBatchReader` without scanning the
  entire file.

## Implementation plan
1. Add a new CLI using `clap` with two subcommands:
   - `serve` (current behaviour) to start the pgwire server.
   - `compile <yaml_dir> <binary_file>` to read all YAML files, build
     `RecordBatch` objects and write them in the binary format.
   - `serve` accepts an optional `--binary-file <path>` argument. When supplied,
     the server loads data from that file instead of YAML. If not supplied it
     falls back to the YAML directory argument as today.
2. Implement serializers and deserializers for the format. Use Arrow's IPC
   writer/reader to encode each table.
3. Update `get_base_session_context` to accept either a YAML path or parsed
   binary data. When `--binary-file` is passed it should ignore the YAML path.
4. Add unit tests exercising `compile` + load round trips and validate that the
   resulting session context exposes the same tables.
5. Extend functional tests to run the server with `--binary-file` to ensure the
   pgwire behaviour is unchanged.
6. Document usage of the new commands in `README.md`.

---
Implemented Task 200: added binary catalog compiler and loader using Arrow IPC. Added clap-based CLI with serve and compile commands, unit tests for round trip, functional test running server with --binary-file, and documented CLI usage.
