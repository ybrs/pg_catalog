#!/bin/sh
# Verify this repository: format, lint, and every test.
#
# This is the single definition of what "verified" means here. The pre-commit
# hook calls it rather than keeping its own copy of the pipeline, so a check
# added here is a check the hook enforces, with no second list to keep in step.
#
# Nothing in it is conditional on which files changed: a docs-only edit runs the
# same steps as a rewrite of the view planner, and the example crates are built
# and tested whether or not anything under example*/ was touched. Run it by hand
# any time; it only reads the tree, never rewrites it (`cargo fmt --check`
# reports, the hook is what reformats).
#
# Expect roughly fifteen minutes. Exits non-zero on the first failure.
set -e

# Put the Rust toolchain on PATH. A hook or cron shell inherits almost no
# environment, and this container keeps the toolchain outside the usual home
# directory, so neither can be assumed present.
ensure_rust_toolchain() {
    if command -v cargo >/dev/null 2>&1; then
        return 0
    fi
    toolchain_bin=/tmp/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin
    if [ ! -x "$toolchain_bin/cargo" ]; then
        echo "run_all_tests: cargo not found on PATH or at $toolchain_bin" >&2
        exit 1
    fi
    PATH="$toolchain_bin:$PATH"
    CARGO_HOME=${CARGO_HOME:-/tmp/.cargo}
    RUSTUP_HOME=${RUSTUP_HOME:-/tmp/.rustup}
    export PATH CARGO_HOME RUSTUP_HOME
}

# Announce a step and time it, so a slow run says which part is slow rather
# than going quiet for minutes.
step() {
    step_name=$1
    shift
    echo
    echo "=== $step_name"
    step_started=$(date +%s)
    "$@"
    echo "--- $step_name ok (`expr $(date +%s) - $step_started`s)"
}

ensure_rust_toolchain
cd "$(dirname "$0")"

PYTHON=.venv/bin/python
if [ ! -x "$PYTHON" ]; then
    echo "run_all_tests: $PYTHON missing; create it with 'make test-python' or" >&2
    echo "               python3 -m venv .venv && .venv/bin/python -m pip install -r requirements.txt" >&2
    exit 1
fi

# -j 6: each test binary links the whole DataFusion tree, so the default job
# count runs enough linkers at once to thrash a 12-core box into swap.
JOBS=6

# The two example crates are separate cargo workspaces, so the root cargo
# commands do not reach them and each needs naming explicitly.
step "cargo fmt --check" cargo fmt --check
step "cargo fmt --check (example)" cargo fmt --check --manifest-path example/Cargo.toml
step "cargo fmt --check (example-lazycatalog)" cargo fmt --check --manifest-path example-lazycatalog/Cargo.toml

step "cargo clippy (pedantic)" \
    cargo clippy --all-targets -j $JOBS -- -D warnings -D clippy::pedantic
step "cargo clippy (pedantic, example)" \
    cargo clippy --all-targets -j $JOBS --manifest-path example/Cargo.toml -- -D warnings -D clippy::pedantic
step "cargo clippy (pedantic, example-lazycatalog)" \
    cargo clippy --all-targets -j $JOBS --manifest-path example-lazycatalog/Cargo.toml -- -D warnings -D clippy::pedantic

step "flake8" $PYTHON -m flake8 .

# 164 lib unit tests plus fifteen integration binaries.
step "cargo test" cargo test -j $JOBS

# The pytest fixtures spawn the server through `cargo run`, so cargo has to stay
# on PATH for this too.
step "pytest" $PYTHON -m pytest -q

# Both example crates ship a tests/test_example.py. pytest imports test modules
# by basename, so collecting the two directories in one run fails on the name
# collision - they have to be separate invocations.
step "pytest (example)" $PYTHON -m pytest -q example/tests
step "pytest (example-lazycatalog)" $PYTHON -m pytest -q example-lazycatalog/tests

echo
echo "run_all_tests: everything passed"
