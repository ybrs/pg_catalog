# Test Coverage Improvement Tasks

1. **Test `db_table.map_pg_type`** – Verify that various Postgres type strings (e.g. `int`, `bigint`, `bool`, `varchar(20)`) map to the correct Arrow `DataType`. Include an unknown type to ensure the default mapping is `Utf8`.
2. **Test `session.rename_columns`** – Create a `RecordBatch` with a simple schema and ensure `rename_columns` correctly renames columns based on the provided mapping without altering data or order.
3. **Test `session.parse_schema_file`** – Use a small YAML schema file to confirm that tables and schemas are loaded with the expected column types and row values.
~~4. **Test `server.arrow_to_pg_type`** – Check that Arrow `DataType` values map to the appropriate pgwire `Type` enums.~~
   - done
5. **Test `server.batch_to_field_info`** – Build a tiny `RecordBatch` and verify the generated `FieldInfo` array matches the batch schema and requested format.
6. **Test `ObservableMemTable.scan` logging** – Ensure that scanning the table records the correct table name, projection columns, and filter expressions in the log.
7. **Test `build_table` helper** – Validate that YAML table definitions are converted to `RecordBatch` objects with the right schema and values.

