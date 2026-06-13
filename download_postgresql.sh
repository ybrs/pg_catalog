#!/bin/bash
# Downloads a prebuilt PostgreSQL binary distribution for the CURRENT OS/arch.
# Only needed for testing and schema extraction.

set -e

PG_VERSION="17.4.0"

os="$(uname -s)"
arch="$(uname -m)"

# Normalize the arch to the names used by the release assets.
case "$arch" in
  arm64 | aarch64) arch="aarch64" ;;
  x86_64 | amd64)  arch="x86_64" ;;
  *) echo "unsupported architecture: $arch" >&2; exit 1 ;;
esac

# Map the OS to the target triple used by theseus-rs/postgresql-binaries.
case "$os" in
  Darwin) triple="${arch}-apple-darwin" ;;
  Linux)  triple="${arch}-unknown-linux-gnu" ;;
  *) echo "unsupported OS: $os" >&2; exit 1 ;;
esac

asset="postgresql-${PG_VERSION}-${triple}.tar.gz"
url="https://github.com/theseus-rs/postgresql-binaries/releases/download/${PG_VERSION}/${asset}"

echo "Detected ${os}/${arch} -> downloading ${asset}"
wget "$url" -O "$asset"
tar zxf "$asset"
rm -rf postgres-17
mv "postgresql-${PG_VERSION}-${triple}" postgres-17
rm -f "$asset"

# macOS marks downloaded binaries as quarantined; strip it so they can run.
if [ "$os" = "Darwin" ]; then
  xattr -d com.apple.quarantine postgres-17/lib/* 2>/dev/null || true
  xattr -d com.apple.quarantine postgres-17/bin/* 2>/dev/null || true
fi

echo "PostgreSQL ${PG_VERSION} ready in ./postgres-17"
