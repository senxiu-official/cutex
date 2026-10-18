#!/usr/bin/env bash
# Build the release archive of the optional component `claude-runtime` for one platform
# (components/README.md; contract v2 §2):
#
#   bash components/claude-runtime/build.sh <platform> <version> <out-dir>
#
# <platform>: linux-x64 | windows-x64 | darwin-arm64 (the archive is built the same way on any host: the wheels
# are downloaded for the target platform). <version>: must equal component.json `version` and the
# `release` in src/cute_claude/features.json (claude-rN); a mismatch fails the build.
# Writes to <out-dir>:
#   component-claude-runtime-<version>-<platform>.tar.gz   adapter source (component.json, README.md,
#                     LICENSE, pyproject.toml, src/, scripts/connect_tui.py) + wheels/ (that platform's
#                     vendored dependencies) + requirements.lock (runtime only, --hash per wheel)
#                     + THIRD_PARTY_NOTICES.md (the wheels' licenses and license texts)
#   SHA256SUMS-component-claude-runtime-<platform>
#   BUILD-INFO-component-claude-runtime-<platform>.json     name, version, commit, platform
# No venv and no test dependencies; ustar, regular files only, owner 0, a fixed mtime,
# `gzip -n`. The source comes from an LF `git archive` export of HEAD. The wheels come from PyPI,
# except claude-agent-sdk, which is built CLI-free from its pinned sdist (scripts/vendor_wheels.py),
# so a rebuild of the same commit reproduces the archive's sha256.
# Needs: git, GNU tar, gzip, sha256sum, Python 3.11+ with pip (PYTHON, default python3) and PyPI.
set -euo pipefail

platform="${1:?platform: linux-x64 | windows-x64 | darwin-arm64}"
version="${2:?version (claude-rN)}"
out="${3:?out dir}"
case "$platform" in linux-x64 | windows-x64 | darwin-arm64) ;; *) echo "claude-runtime is built for linux-x64, windows-x64 and darwin-arm64 (got $platform)" >&2; exit 2 ;; esac
unixpath() { if command -v cygpath > /dev/null 2>&1; then cygpath -u "$1"; else printf '%s' "$1"; fi; }
out="$(unixpath "$out")"
python="${PYTHON:-}"
if [ -z "$python" ]; then
  for candidate in python3 python; do
    if command -v "$candidate" > /dev/null 2>&1; then python="$candidate"; break; fi
  done
fi
[ -n "$python" ] || { echo "no python3 on PATH (set PYTHON)" >&2; exit 2; }
here="$(cd "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
root="$(git -C "$here" rev-parse --show-toplevel)"
rel="components/claude-runtime"
cd "$root"
if [ -n "$(git status --porcelain --untracked-files=no -- "$rel")" ]; then
  echo "$rel has local changes; build from a clean checkout" >&2
  exit 1
fi
mkdir -p "$out"
out="$(cd "$out" && pwd -P)"
commit="$(git rev-parse HEAD)"
# A fixed mtime (not the commit time): the archive bytes depend only on the shipped files, so a
# later commit that leaves them unchanged reproduces an already-published version's sha256.
mtime=1735689600
work="$(mktemp -d)"
trap 'rm -rf "${work:?}"' EXIT
git -c core.autocrlf=false archive --format=tar HEAD "$rel" | tar -x -C "$work"
src="$work/$rel"

# The version is the component's own identity (contract §0.5).
read -r declared release < <("$python" -c 'import json,sys
c = json.load(open(sys.argv[1])); f = json.load(open(sys.argv[2]))
assert c["name"] == "claude-runtime", c["name"]
print(c["version"], f["release"])' "$src/component.json" "$src/src/cute_claude/features.json")
# Windows Python ends its lines with CRLF; the callers' $(...) may carry a \r too.
version="${version%$'\r'}"; declared="${declared%$'\r'}"; release="${release%$'\r'}"
if [ "$version" != "$declared" ] || [ "$version" != "$release" ]; then
  echo "version mismatch: build $version, component.json $declared, features.json release $release" >&2
  exit 1
fi

stage="$work/stage"
mkdir -p "$stage/scripts"
cp "$src/component.json" "$src/README.md" "$src/LICENSE" "$src/pyproject.toml" "$stage/"
cp -R "$src/src" "$stage/src"
cp "$src/scripts/connect_tui.py" "$stage/scripts/"
find "$stage" \( -name __pycache__ -o -name '*.pyc' -o -name '*.egg-info' \) -prune -exec rm -rf {} +
"$python" "$src/scripts/vendor_wheels.py" --platform "$platform" --requirements "$src/requirements-runtime.txt" --out "$stage"
"$python" "$src/scripts/wheel_notices.py" --wheels "$stage/wheels" --out "$stage/THIRD_PARTY_NOTICES.md"
for f in src/cute_claude/__main__.py src/cute_claude/features.json scripts/connect_tui.py requirements.lock LICENSE THIRD_PARTY_NOTICES.md; do
  [ -f "$stage/$f" ] || { echo "archive would lack $f" >&2; exit 1; }
done
if find "$stage" -type l | grep -q .; then echo "links in the archive tree" >&2; exit 1; fi
if grep -qiE '^(pytest|pytest-asyncio|iniconfig|pygments)==' "$stage/requirements.lock"; then
  echo "test dependencies in requirements.lock" >&2; exit 1
fi

# Leak check: no path of the checkout, the work dir or the user's home in the shipped text files.
leaks=0
for p in "$root" "$work" "$HOME"; do
  [ -n "$p" ] && [ "$p" != / ] || continue
  if grep -rqaF --exclude='*.whl' -- "$p" "$stage"; then
    echo "::warning::the archive contains the build path $p"
    leaks=1
  fi
done
if [ "$leaks" = 1 ] && [ "${LEAK_POLICY:-fail}" = fail ]; then
  echo "build paths found in the archive (LEAK_POLICY=fail)" >&2
  exit 1
fi

name="component-claude-runtime-$version-$platform.tar.gz"
(cd "$stage" && find . -type f -printf '%P\n' | LC_ALL=C sort > "$work/files")
tar -C "$stage" --format=ustar --owner=0 --group=0 --numeric-owner --mode='u+rwX,go+rX,go-w' --mtime="@$mtime" \
  --no-recursion -cf - -T "$work/files" | gzip -n -9 > "$out/$name"
cat > "$out/BUILD-INFO-component-claude-runtime-$platform.json" <<JSON
{
  "name": "claude-runtime",
  "version": "$version",
  "commit": "$commit",
  "platform": "$platform"
}
JSON
(cd "$out" && sha256sum "$name" > "SHA256SUMS-component-claude-runtime-$platform")
echo "built $out/$name ($(wc -l < "$work/files" | tr -d ' ') files)"
cat "$out/SHA256SUMS-component-claude-runtime-$platform"
