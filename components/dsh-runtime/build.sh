#!/usr/bin/env bash
# Offline only. Input node_modules must already match the target OS/architecture.
# Usage: build.sh <linux-x64|windows-x64> <dsh-rN> <out-dir> <explicit-source-tree>
set -euo pipefail
platform="${1:?platform}"; version="${2:?version}"; out="${3:?output}"; source="${4:?explicit source tree}"
case "$platform" in linux-x64|windows-x64) ;; *) echo 'Unsupported platform' >&2; exit 2;; esac
source="$(cd "$source" && pwd -P)"
adapter="$source"
if [[ -f "$source/components/dsh-runtime/component.json" ]]; then adapter="$source/components/dsh-runtime"; fi
runtime="$adapter/runtime"
if [[ ! -d "$runtime" ]]; then runtime="$source/.dsh-install"; fi
# Node is a local build tool for JSON parsing; this executes no staged package.
node --input-type=module - "$adapter" "$runtime" "$version" <<'JS'
import { readFileSync } from 'node:fs';
const [adapter, runtime, version] = process.argv.slice(2);
const read = path => JSON.parse(readFileSync(path, 'utf8'));
const meta = read(`${adapter}/component.json`);
const dsh = read(`${runtime}/node_modules/@deepseek-ai/dsh/package.json`);
const ws = read(`${runtime}/node_modules/ws/package.json`);
const catalog = read(`${runtime}/node_modules/@deepseek-ai/dsh-session-format-catalog/package.json`);
if (meta.name !== 'dsh-runtime' || meta.version !== version || dsh.name !== '@deepseek-ai/dsh' || dsh.version !== '0.2.1-rc.3' || ws.name !== 'ws' || ws.version !== '8.22.0' || catalog.name !== '@deepseek-ai/dsh-session-format-catalog' || catalog.version !== '0.2.1-rc.3') throw Error('Component/package version mismatch');
JS
mkdir -p "$out"
out="$(cd "$out" && pwd -P)"
case "$out/" in "$source/"*) echo 'Output must be outside source tree' >&2; exit 2;; esac
stage="$(mktemp -d "$out/.dsh-pack-XXXXXXXX")"
# Npm's .bin links are not shipped: the component uses explicit JS entrypoints.
# All other links are refused, so the archive cannot follow external files.
if find "$adapter/src" "$runtime/node_modules" -type l ! -path '*/node_modules/.bin/*' -print -quit | grep -q .; then echo 'Package tree contains links' >&2; exit 2; fi
for file in "$adapter/component.json" "$adapter/package.json" "$adapter/package-lock.json" "$runtime/package.json" "$runtime/package-lock.json"; do [[ ! -L "$file" ]] || { echo 'Manifest links are forbidden' >&2; exit 2; }; done
cp "$adapter/component.json" "$adapter/package.json" "$adapter/package-lock.json" "$stage/"
cp -R "$adapter/src" "$stage/src"
mkdir -p "$stage/runtime" "$stage/node_modules"
cp "$runtime/package.json" "$runtime/package-lock.json" "$stage/runtime/"
# tar excludes npm command shims; no shell/npm entrypoint is used at runtime.
tar -C "$runtime" --hard-dereference --exclude='*/.bin' -cf - node_modules | tar -C "$stage/runtime" -xf -
cp -R "$stage/runtime/node_modules/ws" "$stage/node_modules/ws"
node_name=node; [[ "$platform" != windows-x64 ]] || node_name=node.exe
node_mode=system
[[ ! -L "$runtime/$node_name" ]] || { echo 'Bundled Node links are forbidden' >&2; exit 2; }
if [[ -f "$runtime/$node_name" && ! -L "$runtime/$node_name" ]]; then cp -p "$runtime/$node_name" "$stage/runtime/"; node_mode=bundled; fi
for file in src/server.mjs src/history-reader.mjs src/runtime.mjs src/rpc.mjs src/delivery.mjs src/dsh-plugin.mjs src/session-ops.mjs src/store.mjs src/reconcile.mjs src/nativeProjection.mjs src/toolItems.mjs runtime/node_modules/@deepseek-ai/dsh/lib/bin.js runtime/node_modules/@deepseek-ai/dsh-session-format-catalog/lib/index.js node_modules/ws/index.js; do [[ -f "$stage/$file" ]] || { echo "Missing $file" >&2; exit 2; }; done
archive="component-dsh-runtime-$version-$platform.tar.gz"
[[ ! -e "$out/$archive" ]] || { echo 'Archive already exists' >&2; exit 2; }
tar --hard-dereference -czf "$out/$archive" -C "$stage" component.json package.json package-lock.json src runtime node_modules
(cd "$out" && sha256sum "$archive" > "SHA256SUMS-component-dsh-runtime-$platform")
printf '{"name":"dsh-runtime","version":"%s","platform":"%s","dshVersion":"0.2.1-rc.3","nodeMinimum":"24.9.0","nodeMode":"%s"}\n' "$version" "$platform" "$node_mode" > "$out/BUILD-INFO-component-dsh-runtime-$platform.json"
printf '%s\n' "$out/$archive"
