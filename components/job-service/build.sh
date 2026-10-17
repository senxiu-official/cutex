#!/usr/bin/env bash
# Build the job-service component archive of one platform (components/README.md, Layout):
#
#   component-job-service-<version>-<platform>.tar.gz   cutex-job-service at the archive root
#   SHA256SUMS-component-job-service-<platform>
#   BUILD-INFO-component-job-service-<platform>.json     {name, version, commit, platform}
#
#   bash components/job-service/build.sh <platform> <version> <out-dir>
#
# <version> must equal component.json and Cargo.toml.
# Same rules as the cutex archive (.github/ci/build-cutex.sh): builds an LF `git archive` export of
# HEAD's components/job-service under BUILD_ROOT, no build paths in the binary (remap + leak check),
# stripped, ustar with fixed owner and mtime (last commit of the binary's sources), `gzip -n`. Linux
# only (the Job Service is Linux-only).
# Environment (optional): BUILD_ROOT (default <checkout>/../build-root), CARGO_HOME, RUST_TOOLCHAIN
# (default 1.95.0), LEAK_POLICY fail (default) | warn.
set -euo pipefail

platform="${1:?platform: linux-x64}"
version="${2:?component version}"
out="${3:?out dir}"
case "$platform" in
  linux-x64) target=x86_64-unknown-linux-gnu ;;
  *) echo "job-service is built for linux-x64 only (got $platform)" >&2; exit 2 ;;
esac
if ! [[ "$version" =~ ^[A-Za-z0-9._-]{1,64}$ ]] || [ "$version" = . ] || [ "$version" = .. ]; then
  echo "invalid version: $version" >&2
  exit 2
fi
root="$(cd "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
cd "$root"
declared="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' components/job-service/component.json)"
declared="${declared%$'\r'}"
crate="$(sed -n 's/^version = "\(.*\)"$/\1/p' components/job-service/Cargo.toml | head -n1)"
if [ "$version" != "$declared" ] || [ "$version" != "$crate" ]; then
  echo "version $version != component.json $declared / Cargo.toml $crate" >&2
  exit 2
fi
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
toolchain="${RUST_TOOLCHAIN:-1.95.0}"
mkdir -p "$out"
out="$(cd "$out" && pwd -P)"
if [ -n "$(git status --porcelain --untracked-files=no -- components/job-service)" ]; then
  echo "components/job-service has local changes; build from a clean checkout" >&2
  exit 1
fi
build_root="${BUILD_ROOT:-$root/../build-root}"
mkdir -p "$build_root"
build_root="$(cd "$build_root" && pwd -P)"
src="$build_root/job-service"
rm -rf "${src:?}"
mkdir -p "$src"
git -c core.autocrlf=false archive --format=tar HEAD components/job-service | tar -x -C "$src" --strip-components=2
# The archive mtime is the time of the last commit that changed the binary's inputs (src/, Cargo.toml,
# Cargo.lock), not HEAD's: a later release that leaves them unchanged (README, tests or this script may
# change) reproduces the already-published archive byte for byte (sign-and-upload.sh refuses a
# published component version whose bytes changed). Needs the history of those paths: a shallow clone
# falls back to HEAD's time with a warning (fine for e2e builds, never for a release build).
if [ "$(git rev-parse --is-shallow-repository)" = true ]; then
  echo "::warning::shallow clone: the archive mtime is HEAD's commit time (not reproducible across releases)"
  mtime="$(git log -1 --format=%ct)"
else
  mtime="$(git log -1 --format=%ct -- components/job-service/src components/job-service/Cargo.toml components/job-service/Cargo.lock)"
fi
commit="$(git rev-parse HEAD)"

sysroot="$(rustc "+$toolchain" --print sysroot)"
tvar="CARGO_TARGET_$(printf '%s' "$target" | tr 'a-z-' 'A-Z_')_RUSTFLAGS"
export "$tvar=--remap-path-prefix=$src=/build/job-service --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$sysroot=/rust"
map="-ffile-prefix-map=$src=/build/job-service -ffile-prefix-map=$cargo_home=/cargo"
export CFLAGS="${CFLAGS:+$CFLAGS }$map" CXXFLAGS="${CXXFLAGS:+$CXXFLAGS }$map"
export CARGO_PROFILE_RELEASE_STRIP=symbols CARGO_PROFILE_RELEASE_DEBUG=0
(cd "$src" && cargo "+$toolchain" build --locked --release --bin cutex-job-service)

stage="$build_root/job-service-stage-$platform"
rm -rf "${stage:?}"
mkdir -p "$stage"
cp "$src/target/release/cutex-job-service" "$stage/"
leaks=0
for p in $(printf '%s\n' "$root" "$cargo_home" "$sysroot" "$HOME" "$src" | sort -u); do
  [ -n "$p" ] && [ "$p" != / ] || continue
  if grep -qaF -- "$p" "$stage/cutex-job-service"; then
    echo "::warning::cutex-job-service contains the build path $p"
    leaks=1
  fi
done
if [ "$leaks" = 1 ] && [ "${LEAK_POLICY:-fail}" = fail ]; then
  echo "build paths found in the binary (LEAK_POLICY=fail)" >&2
  exit 1
fi
name="component-job-service-$version-$platform.tar.gz"
sums="SHA256SUMS-component-job-service-$platform"
info="BUILD-INFO-component-job-service-$platform.json"
chmod 0755 "$stage/cutex-job-service"
tar -C "$stage" --format=ustar --owner=0 --group=0 --numeric-owner --mtime="@$mtime" -cf - cutex-job-service | gzip -n -9 > "$out/$name"
(cd "$out" && sha256sum "$name" > "$sums")
python3 - "$out/$info" "$version" "$commit" "$platform" <<'PY'
import json, sys
path, version, commit, platform = sys.argv[1:]
with open(path, "w") as f:
    json.dump({"name": "job-service", "version": version, "commit": commit, "platform": platform}, f, indent=2)
    f.write("\n")
PY
echo "built $out/$name"
cat "$out/$sums"
