#!/bin/bash
# Downloads a prebuilt PostgreSQL binary distribution used by the helper scripts.
# Only needed for testing and schema extraction.

set -e
wget 'https://github.com/theseus-rs/postgresql-binaries/releases/download/17.4.0/postgresql-17.4.0-aarch64-apple-darwin.tar.gz'
tar zxvf postgresql-17.4.0-aarch64-apple-darwin.tar.gz
mv postgresql-17.4.0-aarch64-apple-darwin postgres-17
# xattr -d com.apple.quarantine postgres-17/lib/*
# xattr -d com.apple.quarantine postgres-17/bin/*
