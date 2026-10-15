#!/usr/bin/env bash
set -Eeuo pipefail

if [[ $# -ne 1 ]]; then
  printf 'usage: %s BUNDLE_DIR\n' "$0" >&2
  exit 2
fi

bundle=$1
if [[ ! -d "$bundle" ]]; then
  printf 'release bundle directory not found: %s\n' "$bundle" >&2
  exit 1
fi

required=(cutex cute-codex codex-code-mode-host)
for name in "${required[@]}"; do
  artifact="$bundle/$name"
  if [[ ! -f "$artifact" || ! -x "$artifact" || -L "$artifact" ]]; then
    printf 'release bundle requires a regular executable: %s\n' "$artifact" >&2
    exit 1
  fi
done

"$bundle/cutex" --version >/dev/null
"$bundle/cute-codex" --version >/dev/null
"$bundle/codex-code-mode-host" --help >/dev/null

printf 'release bundle complete: cutex + cute-codex + codex-code-mode-host\n'
