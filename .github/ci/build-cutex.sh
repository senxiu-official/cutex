#!/usr/bin/env bash
# Build the cutex release archive of one platform:
#
#   cutex-<release>-<platform>.tar.gz   cutex[.exe] and cutex-mcp[.exe] at the archive root
#
# This is the archive a cutex release manifest names under `cutex.artifacts.<platform>` and that
# `cutex upgrade stage` unpacks into `releases/<release>/` (src/upgrade/stage.rs: ustar, files only, no
# directories or links). The binaries carry the release identity (src/build_info.rs): `cutex --version`
# prints `<version> (<release>, <commit7>)` and the channel is `release`.
#
#   bash .github/ci/build-cutex.sh <platform> <release> <out-dir>
#
# <platform>: linux-x64 | windows-x64 (Git Bash on Windows, MSVC on PATH as on GitHub's runners).
# <release>: the release label, [A-Za-z0-9._-], e.g. 0.1.0 (the manifest's tag is cutex-<release>).
# Runs in a clean checkout and builds an LF `git archive` export of HEAD (the committed bytes: the
# embedded schemas are hashed byte for byte) under BUILD_ROOT/cutex. The binary keeps that path in one
# place (a development fallback built from CARGO_MANIFEST_DIR), so CI uses a neutral BUILD_ROOT such
# as /build. Environment (optional): BUILD_ROOT (default <checkout>/../build-root), CARGO_HOME
# (remapped to /cargo), RUST_TOOLCHAIN (default 1.95.0), LEAK_POLICY fail (default) | warn.
set -euo pipefail

platform="${1:?platform: linux-x64 | windows-x64}"
release="${2:?release label}"
out="${3:?out dir}"
case "$platform" in
  linux-x64) target=x86_64-unknown-linux-gnu; exe="" ;;
  windows-x64) target=x86_64-pc-windows-msvc; exe=".exe" ;;
  *) echo "unknown platform $platform" >&2; exit 2 ;;
esac
if ! [[ "$release" =~ ^[A-Za-z0-9._-]{1,64}$ ]] || [ "$release" = . ] || [ "$release" = .. ]; then
  echo "invalid release label: $release" >&2
  exit 2
fi

# Git Bash on Windows: directories given as C:\... become /c/... (GitHub passes RUNNER_TEMP that way).
unixpath() { if command -v cygpath > /dev/null 2>&1; then cygpath -u "$1"; else printf '%s' "$1"; fi; }
out="$(unixpath "$out")"
root="$(cd "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
cd "$root"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
toolchain="${RUST_TOOLCHAIN:-1.95.0}"
mkdir -p "$out"
out="$(cd "$out" && pwd -P)"

winpath() { if [ "$platform" = windows-x64 ]; then cygpath -w "$1"; else printf '%s' "$1"; fi; }
mixpath() { if [ "$platform" = windows-x64 ]; then cygpath -m "$1"; else printf '%s' "$1"; fi; }

commit="$(git rev-parse HEAD)"
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
  echo "the checkout has local changes; build from a clean checkout" >&2
  exit 1
fi
build_root="$(unixpath "${BUILD_ROOT:-$root/../build-root}")"
mkdir -p "$build_root"
build_root="$(cd "$build_root" && pwd -P)"
src="$build_root/cutex"
rm -rf "${src:?}"
mkdir -p "$src"
git -c core.autocrlf=false archive --format=tar HEAD | tar -x -C "$src"

# Release identity (src/build_info.rs). The build time is the commit time, so a rebuild of the same
# commit gets the same value.
export CUTEX_RELEASE="$release" CUTEX_COMMIT="$commit" CUTEX_CHANNEL=release
export CUTEX_BUILD_TIME="$(git log -1 --format=%cI)"
mtime="$(git log -1 --format=%ct)"

# No build paths in the binaries (see the cute-codex build script for the reasoning). The
# target-specific variable adds to rustflags from config files instead of replacing them.
tvar="CARGO_TARGET_$(printf '%s' "$target" | tr 'a-z-' 'A-Z_')_RUSTFLAGS"
sysroot="$(rustc "+$toolchain" --print sysroot)"
remap="--remap-path-prefix=$(winpath "$src")=/build/cutex --remap-path-prefix=$(winpath "$cargo_home")=/cargo --remap-path-prefix=$sysroot=/rust"
if [ "$platform" = windows-x64 ]; then
  export "$tvar=$remap -Clink-arg=/PDBALTPATH:%_PDB%"
  trim="/d1trimfile:$(winpath "$src")\\ /d1trimfile:$(winpath "$cargo_home")\\"
  export CFLAGS="${CFLAGS:+$CFLAGS }$trim" CXXFLAGS="${CXXFLAGS:+$CXXFLAGS }$trim"
else
  export "$tvar=$remap"
  map="-ffile-prefix-map=$src=/build/cutex -ffile-prefix-map=$cargo_home=/cargo"
  export CFLAGS="${CFLAGS:+$CFLAGS }$map" CXXFLAGS="${CXXFLAGS:+$CXXFLAGS }$map"
fi
# Symbols and debug info stripped at link time (both platforms; on Windows no PDB is written).
export CARGO_PROFILE_RELEASE_STRIP=symbols CARGO_PROFILE_RELEASE_DEBUG=0

(cd "$src" && cargo "+$toolchain" build --locked --release --bin cutex --bin cutex-mcp)

stage="$build_root/cutex-stage-$platform"
rm -rf "${stage:?}"
mkdir -p "$stage"
cp "$src/target/release/cutex$exe" "$src/target/release/cutex-mcp$exe" "$stage/"
line="$("$stage/cutex$exe" --version)"
case "$line" in
  *"($release, ${commit:0:7})"*) echo "$line" ;;
  *) echo "cutex --version does not name $release / ${commit:0:7}: $line" >&2; exit 1 ;;
esac

# Leak check: no path of the checkout, cargo's home, the toolchain or the user's home in the binaries.
leaks=0
for f in "$stage"/*; do
  for p in $(for d in "$root" "$cargo_home" "$sysroot" "$HOME"; do winpath "$d"; echo; mixpath "$d"; echo; echo "$d"; done | sort -u); do
    [ -n "$p" ] && [ "$p" != / ] || continue
    if grep -qaF -- "$p" "$f"; then
      echo "::warning::$(basename "$f") contains the build path $p ($(grep -caF -- "$p" "$f") lines)"
      leaks=1
    fi
  done
done
if [ "$leaks" = 1 ] && [ "${LEAK_POLICY:-fail}" = fail ]; then
  echo "build paths found in the binaries (LEAK_POLICY=fail)" >&2
  exit 1
fi

name="cutex-$release-$platform.tar.gz"
tar -C "$stage" --format=ustar --owner=0 --group=0 --numeric-owner --mtime="@$mtime" -cf - "cutex$exe" "cutex-mcp$exe" | gzip -n -9 > "$out/$name"
rustc_version="$(rustc "+$toolchain" --version)"
cat > "$out/BUILD-INFO-$platform.json" <<EOF
{
  "release": "$release",
  "commit": "$commit",
  "channel": "release",
  "builtAt": "$CUTEX_BUILD_TIME",
  "platform": "$platform",
  "target": "$target",
  "rustc": "$rustc_version",
  "versionLine": "$line"
}
EOF
(cd "$out" && sha256sum "$name" > "SHA256SUMS-$platform")
echo "built $out/$name"
cat "$out/SHA256SUMS-$platform"
