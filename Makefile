download_postgres_binary:
	./download_postgresql.sh

run_postgresql:
	./run-postgres.sh

create_schema_yaml_files:
	python schema.py generate pg_catalog_data/pg_schema

create_schema_zip:
	zip -r pg_schema.zip pg_catalog_data/pg_schema


dev_server:
	RUST_LOG=info RUST_MIN_STACK=33554432 cargo run -- ./pg_schema.zip --default-catalog pgtry --default-schema pg_catalog --port 5444

# --- Dependency upgrade pipeline -------------------------------------------
# `make update-deps` upgrades deps, then compiles and runs the full test
# suite (Rust + Python). It stops at the first failure, so a bad upgrade
# never lands silently. Override the toolchain with `make CARGO=... update-deps`.
CARGO  ?= cargo
PYTHON ?= python3
VENV   ?= .venv

.PHONY: update-deps deps-upgrade build test test-rust test-python

update-deps: deps-upgrade build test
	@echo "update-deps: dependencies upgraded, compiled, and tested OK"

# Bump manifest requirements to the latest (incl. semver-incompatible) and
# refresh the lockfile. Requires cargo-edit (`cargo install cargo-edit`).
deps-upgrade:
	$(CARGO) upgrade --incompatible
	$(CARGO) update

build:
	$(CARGO) build --all-targets

test: test-rust test-python

test-rust:
	$(CARGO) test

# The pytest fixtures spawn the server via `cargo run`, so cargo must be on PATH.
test-python: $(VENV)/bin/python
	$(VENV)/bin/python -m pytest -q

# Create the venv only if it is missing.
$(VENV)/bin/python:
	$(PYTHON) -m venv $(VENV)
	$(VENV)/bin/pip install -r requirements.txt
