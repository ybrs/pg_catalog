download_postgres_binary:
	./download_postgresql.sh

run_postgresql:
	./run-postgres.sh

create_schema_yaml_files:
	python schema.py generate pg_catalog_data/pg_schema