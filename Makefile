download_postgres_binary:
	./download_postgresql.sh

run_postgresql:
	./run-postgres.sh

create_schema_yaml_files:
	python schema.py generate pg_catalog_data/pg_schema

create_schema_zip:
	zip -r pg_schema.zip pg_catalog_data/pg_schema

# Regenerate the embedded fast-load artifact (Arrow IPC) from the YAML catalog
# zip. Run this whenever pg_catalog_data/postgres-schema-nightly.zip changes;
# the startup path loads this instead of parsing YAML (~50x faster cold start).
create_schema_ipc:
	cargo run --release --bin gen_schema_ipc

# Full one-shot pipeline: download+start PostgreSQL, extract the catalog to YAML,
# rebuild the YAML zip, and rebuild the embedded Arrow IPC artifact.
regenerate-catalog:
	./regenerate-catalog.sh


dev_server:
	RUST_LOG=info RUST_MIN_STACK=33554432 cargo run -- ./pg_schema.zip --default-catalog pgtry --default-schema pg_catalog --port 5444

# --- Dependency upgrade pipeline -------------------------------------------
# `make update-deps` upgrades deps, then compiles and runs the full test
# suite (Rust + Python). It stops at the first failure, so a bad upgrade
# never lands silently. Override the toolchain with `make CARGO=... update-deps`.
CARGO       ?= cargo
PYTHON      ?= python3
VENV        ?= .venv
EXAMPLE_DIR ?= example

.PHONY: update-deps deps-upgrade build test test-rust test-python \
        example-deps-upgrade example-build example-test

update-deps: deps-upgrade build test
	@echo "update-deps: dependencies upgraded, compiled, and tested OK"

# Bump manifest requirements to the latest (incl. semver-incompatible) and
# refresh the lockfile, for both the root crate and the example crate.
# Requires cargo-edit (`cargo install cargo-edit`).
deps-upgrade: example-deps-upgrade
	$(CARGO) upgrade --incompatible
	$(CARGO) update

example-deps-upgrade:
	$(CARGO) upgrade --incompatible --manifest-path $(EXAMPLE_DIR)/Cargo.toml
	$(CARGO) update --manifest-path $(EXAMPLE_DIR)/Cargo.toml

build: example-build
	$(CARGO) build --all-targets

example-build:
	$(CARGO) build --all-targets --manifest-path $(EXAMPLE_DIR)/Cargo.toml

test: test-rust test-python example-test

test-rust:
	$(CARGO) test

# The pytest fixtures spawn the server via `cargo run`, so cargo must be on PATH.
test-python: $(VENV)/bin/python
	$(VENV)/bin/python -m pytest -q

# The example ships its own pytest suite that drives `cargo run` in example/.
example-test: $(VENV)/bin/python
	$(VENV)/bin/python -m pytest -q $(EXAMPLE_DIR)/tests

# Create the venv only if it is missing.
$(VENV)/bin/python:
	$(PYTHON) -m venv $(VENV)
	$(VENV)/bin/pip install -r requirements.txt
