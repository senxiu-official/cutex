#!/usr/bin/env bash
# Source-only adapter build; no downloads, npm or vendor/Node execution.
# Usage: build.sh <linux-x64|windows-x64> <kimi-r3> <out-dir>
set -euo pipefail
[ "$#" = 3 ] || { echo 'Expected platform, version and output directory' >&2; exit 2; }
here="$(cd "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
builder="$here/../adapter_build.py"
out="$3"
if command -v cygpath >/dev/null 2>&1; then
  builder="$(cygpath -w "$builder")"
  out="$(cygpath -w "$out")"
fi
"${PYTHON:-python3}" "$builder" kimi-runtime "$1" "$2" "$out"
