#!/usr/bin/env bash
# Pick Pythons from the GitHub runner's tool cache for .github/workflows/component-e2e.yml:
#
#   E2E_PYTHON       newest 3.12 (builds the archive, runs the e2e helper); its directory goes first
#                    on PATH, so cutex's own discovery (Linux python3.12, Windows `py -3` / `python`)
#                    finds a Python >= 3.11 for the Claude runtime's venv
#   E2E_OLD_PYTHON   newest 3.10 or 3.9, for the python_too_old check (empty if the image has none)
#
# Written to $GITHUB_ENV / $GITHUB_PATH. Git Bash on Windows.
set -euo pipefail

tc="${RUNNER_TOOL_CACHE:?not on a GitHub runner}"
if command -v cygpath > /dev/null 2>&1; then tc="$(cygpath -u "$tc")"; fi
pick() {
  local d
  d="$(ls -d "$tc"/Python/"$1".*/x64 2> /dev/null | sort -V | tail -1 || true)"
  printf '%s' "$d"
}
exe_in() {
  if [ "${RUNNER_OS:-}" = Windows ]; then cygpath -w "$1/python.exe"; else printf '%s' "$1/bin/python3"; fi
}

new="$(pick 3.12)"
[ -n "$new" ] || { echo "::error::no Python 3.12 in the runner's tool cache ($tc/Python)"; ls "$tc/Python" || true; exit 1; }
old="$(pick 3.10)"
[ -n "$old" ] || old="$(pick 3.9)"

echo "E2E_PYTHON=$(exe_in "$new")" >> "$GITHUB_ENV"
if [ -n "$old" ]; then
  echo "E2E_OLD_PYTHON=$(exe_in "$old")" >> "$GITHUB_ENV"
else
  echo "::notice::no Python 3.10 / 3.9 in the tool cache: the python_too_old phase is skipped"
fi
if [ "${RUNNER_OS:-}" = Windows ]; then
  { cygpath -w "$new"; cygpath -w "$new/Scripts"; } >> "$GITHUB_PATH"
else
  echo "$new/bin" >> "$GITHUB_PATH"
fi
"$(exe_in "$new")" --version
if [ -n "$old" ]; then "$(exe_in "$old")" --version; fi
if command -v py > /dev/null 2>&1; then echo "py launcher: $(py -3 -c 'import sys; print(sys.executable, sys.version.split()[0])' 2>&1 || true)"; fi
