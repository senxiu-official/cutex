#!/usr/bin/env bash
# r265 dev daemon: this checkout's DEBUG cutex (Agent Bus + Management) on a
# marked private home with the DSH component installed, for the desktop's
# CUTEX_DESKTOP_DEV_HOSTS. Never the live install: own HOME / TMPDIR, own
# ports, refuses an unmarked home and the live ports.
#
#   tests/r265-dev-daemon.sh [start]   build-check, prepare home, install DSH, start, print the dev-host JSON
#   tests/r265-dev-daemon.sh status
#   tests/r265-dev-daemon.sh stop      (same as tests/r265-dev-daemon-stop.sh)
#
# Environment (defaults):
#   R265_DEV_HOME   /home/ema/cutex-dev/r265-devhome   (created with a .cutex-test-private-home marker)
#   R265_MGMT_PORT  34970          R265_BUS_PORT 34960
#   CUTEX_BIN       <repo>/target/debug/cutex (run `cargo build --locked` first)
#   R265_NODE_BIN   /home/ema/cutex-dev/tools/node24/bin  (Node >= 24.9 for DSH)
#   R265_DSH_SOURCE <repo>  (a tree with .dsh-install: DSH 0.2.1-rc.3 node_modules)
# No provider key is configured here: add a Profile (and its key) in the desktop.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
home="${R265_DEV_HOME:-/home/ema/cutex-dev/r265-devhome}"
mgmt_port="${R265_MGMT_PORT:-34970}"
bus_port="${R265_BUS_PORT:-34960}"
bin="${CUTEX_BIN:-$repo/target/debug/cutex}"
node_bin="${R265_NODE_BIN:-/home/ema/cutex-dev/tools/node24/bin}"
dsh_source="${R265_DSH_SOURCE:-$repo}"
marker="$home/.cutex-test-private-home"
pids="$home/daemon.pids"

die() { echo "r265-dev-daemon: $*" >&2; exit 1; }

case "$home" in /|"$HOME"|"$HOME/") die "refusing home $home";; esac
for port in "$mgmt_port" "$bus_port"; do
  case "$port" in 24270|24260|25370|25360) die "port $port belongs to the live install";; esac
done

stop() {
  [[ -f "$pids" ]] || { echo "r265-dev-daemon: not running ($home)"; return 0; }
  [[ -f "$marker" ]] || die "$home is not a marked private home"
  # The daemon processes this script started.
  while read -r pid; do
    [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
  done < "$pids"
  sleep 1
  while read -r pid; do
    [[ -n "$pid" ]] && kill -9 "$pid" 2>/dev/null || true
  done < "$pids"
  # Runtimes the daemon started (DSH adapters / DSH) outlive it by design;
  # stop the processes that run with this private home only.
  local pid left=()
  for pid in $(pgrep -f -- "$home/" || true); do
    [[ "$pid" != "$$" ]] || continue
    # Runtimes get a HOME inside this home (…/.cutex/runtime/dsh/<agent>/home).
    local var
    while IFS= read -r -d '' var; do
      case "$var" in "HOME=$home"|"HOME=$home/"*|"CUTEX_TEST_PRIVATE_HOME=$home"|"CUTEX_TEST_PRIVATE_HOME=$home/"*) left+=("$pid"); break;; esac
    done < "/proc/$pid/environ" 2>/dev/null || true
  done
  if (( ${#left[@]} )); then
    kill "${left[@]}" 2>/dev/null || true
    sleep 1
    kill -9 "${left[@]}" 2>/dev/null || true
  fi
  rm -f "$pids"
  echo "r265-dev-daemon: stopped ($home)"
}

status() {
  if [[ -f "$pids" ]] && xargs -r kill -0 < "$pids" 2>/dev/null; then
    echo "running: management http://127.0.0.1:$mgmt_port, bus $bus_port, home $home"
  else
    echo "not running ($home)"
  fi
}

start() {
  [[ -x "$bin" ]] || die "no debug binary at $bin (cargo build --locked)"
  [[ -x "$node_bin/node" ]] || die "no Node at $node_bin/node"
  [[ -d "$dsh_source/.dsh-install/node_modules/@deepseek-ai/dsh" ]] || die "no DSH install at $dsh_source/.dsh-install"
  if [[ -f "$pids" ]] && xargs -r kill -0 < "$pids" 2>/dev/null; then die "already running (stop first)"; fi
  for port in "$mgmt_port" "$bus_port"; do
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then die "port $port is in use"; fi
  done
  if [[ -e "$home" && ! -f "$marker" ]]; then die "$home exists and is not a marked private home"; fi
  mkdir -p "$home" && chmod 700 "$home"
  [[ -f "$marker" ]] || printf 'r265 dev daemon private home\n' > "$marker"
  mkdir -p "$home/.cutex" "$home/tmp" "$home/workspace" "$home/install-base" "$home/logs"
  local config="$home/.cutex/config.json"
  if [[ ! -f "$config" ]]; then
    local bus_token mgmt_token
    bus_token="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
    mgmt_token="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
    (umask 077; printf '{"agent_bus_enabled":true,"agent_bus_port":%s,"agent_bus_token":"%s","management_port":%s,"management_api_token":"%s"}\n' \
      "$bus_port" "$bus_token" "$mgmt_port" "$mgmt_token" > "$config")
  fi
  [[ -f "$home/install-base/install.json" ]] || printf '{"schema":"cutex/install/v1","os":"linux","serviceManager":"fixture-no-services","channel":"main","createdAt":"%s","activeRelease":"r265-dev"}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$home/install-base/install.json"

  local -a env=(env -i "PATH=$node_bin:/usr/bin:/bin" "HOME=$home" "USERPROFILE=$home" "TMPDIR=$home/tmp" "TMP=$home/tmp" "TEMP=$home/tmp"
    "CUTEX_TEST_PRIVATE_HOME=$home" "CUTEX_TEST_FORBID_REAL_HOME=1" "CUTEX_INSTALL_BASE=$home/install-base"
    "CUTEX_DSH_NODE=$node_bin/node" "DSH_TELEMETRY_DISABLED=1")
  [[ -z "${XDG_RUNTIME_DIR:-}" ]] || env+=("XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR")

  # DSH component from this checkout (build.sh archive, installed by cutex).
  if [[ ! -f "$home/.cutex/runtime/dsh/deployment.json" || "${R265_REINSTALL_DSH:-0}" == 1 ]]; then
    local build="$home/component-build"
    rm -rf "$build" && mkdir -p "$build/out" "$build/tree"
    PATH="$node_bin:$PATH" bash "$repo/components/dsh-runtime/build.sh" linux-x64 dsh-r2 "$build/out" "$dsh_source" > "$home/logs/component-build.log" 2>&1 \
      || die "component build failed (see $home/logs/component-build.log)"
    tar -C "$build/tree" -xzf "$build/out/component-dsh-runtime-dsh-r2-linux-x64.tar.gz"
    (cd "$home/workspace" && "${env[@]}" "$bin" component install dsh-runtime --from-source "$build/tree") > "$home/logs/component-install.json" 2> "$home/logs/component-install.err" \
      || die "component install failed (see $home/logs/component-install.err)"
  fi

  : > "$pids"
  (cd "$home/workspace" && exec "${env[@]}" "$bin" agent serve --port "$bus_port") >> "$home/logs/bus.log" 2>&1 &
  echo $! >> "$pids"
  (cd "$home/workspace" && exec "${env[@]}" "$bin" management serve --port "$mgmt_port") >> "$home/logs/management.log" 2>&1 &
  echo $! >> "$pids"
  # The probe's bearer goes through a private header file, not argv.
  local header="$home/tmp/ready-probe.header"
  # (cutex rewrites config.json pretty-printed: parse it as JSON.)
  (umask 077; "$node_bin/node" -e 'const c = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8")); process.stdout.write(`Authorization: Bearer ${c.management_api_token}\n`)' "$config" > "$header")
  for _ in $(seq 1 120); do
    if curl -fsS -o /dev/null -H @"$header" "http://127.0.0.1:$mgmt_port/v2/sessions" 2>/dev/null; then
      rm -f "$header"
      echo "r265-dev-daemon: running (home $home; logs $home/logs)" >&2
      printf 'CUTEX_DESKTOP_DEV_HOSTS=%s\n' "[{\"id\":\"dev-r265\",\"name\":\"r265 dev (DSH)\",\"baseUrl\":\"http://127.0.0.1:$mgmt_port\",\"tokenFile\":\"$config\"}]"
      return 0
    fi
    sleep 0.5
  done
  rm -f "$header"
  stop
  die "Management did not become ready (see $home/logs)"
}

case "${1:-start}" in
  start) start;;
  stop) stop;;
  status) status;;
  *) die "usage: $0 [start|status|stop]";;
esac
