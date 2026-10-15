#!/usr/bin/env bash
set -euo pipefail

# Simple build script for cutex (Linux)
# Usage:
#   cd /path/to/cutex
#   bash build.sh

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found. If you installed Rust with rustup, run:"
  echo "  . \"$HOME/.cargo/env\""
  echo "in your shell and then re-run this script." >&2
  exit 1
fi

cargo build --release

BIN="$(pwd)/target/release/cutex"
if [ -x "$BIN" ]; then
  echo "Build succeeded. Binary: $BIN"
else
  echo "Build finished but binary not found at $BIN" >&2
  exit 1
fi
