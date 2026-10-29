#!/usr/bin/env bash
# r266 K2: token-refresh race of agents sharing one Kimi account, with the REAL
# pinned Kimi CLI against the FAKE Kimi OAuth server (rotating refresh tokens,
# delayed refresh answers to widen the race). Offline; private dirs only.
#
# Shared layout (cutex's choice, KimiLaunch::install_into): every agent home has
# `credentials/` and `oauth/` as directory links to the account home's.
# Control layout (rejected): every agent home has its own COPY of the credential.
#
#   tests/r266-kimi-refresh-race.sh
#
# Environment: R266_RACE_DIR (default /home/ema/cutex-dev/scratch-k2/race, recreated),
# R266_OAUTH_PORT (35881), R266_NODE, R266_KIMI_MAIN (as in r266-kimi-accounts-smoke.sh).
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
dir="${R266_RACE_DIR:-/home/ema/cutex-dev/scratch-k2/race}"
port="${R266_OAUTH_PORT:-35881}"
node="${R266_NODE:-/home/ema/cutex-dev/tools/node24/bin/node}"
kimi_main="${R266_KIMI_MAIN:-/home/ema/cutex-dev/ext/kimi-code-install/node_modules/@moonshot-ai/kimi-code/dist/main.mjs}"
agents="${R266_RACE_AGENTS:-4}"

die() { echo "r266-race: FAIL: $*" >&2; exit 1; }
ok() { echo "r266-race: ok: $*"; }
case "$port" in 24270|24260|25370|25360) die "live port";; esac
if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then die "port $port is in use"; fi
case "$dir" in /|"$HOME"|"$HOME/") die "refusing $dir";; esac
if [[ -e "$dir" ]]; then [[ -f "$dir/.r266-race" ]] || die "$dir is not ours"; rm -rf "$dir"; fi
mkdir -p "$dir/tmp" "$dir/cwd" && touch "$dir/.r266-race" && chmod 700 "$dir"
git -C "$dir/cwd" init -q

pids=()
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  # Only processes running with a HOME inside this private dir (never a caller's shell).
  local pid
  for pid in $(pgrep -u "$(id -u)" . || true); do
    [[ "$pid" != "$$" ]] || continue
    if { tr '\0' '\n' < "/proc/$pid/environ"; } 2>/dev/null | grep -qx -e "HOME=$dir" -e "HOME=$dir/.*"; then kill -9 "$pid" 2>/dev/null || true; fi
  done
}
trap cleanup EXIT

start_server() {
  mkfifo "$dir/tmp/oauth-$1.stdin"
  sleep 600 > "$dir/tmp/oauth-$1.stdin" & pids+=($!)
  "$node" "$repo/tests/fixtures/fake-kimi-oauth.mjs" --port "$port" --pending 0 --refresh-delay 1500 \
    < "$dir/tmp/oauth-$1.stdin" > "$dir/tmp/oauth-$1.out" 2> "$dir/tmp/oauth-$1.log" & server_pid=$!
  pids+=($server_pid)
  for _ in $(seq 1 50); do (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && return 0; sleep 0.1; done
  die "fake server did not start"
}
stop_server() { kill "$server_pid" 2>/dev/null || true; wait "$server_pid" 2>/dev/null || true; sleep 0.3; }
stats() { curl -fsS "http://127.0.0.1:$port/stats"; }

kimi_env() { # home
  echo env -i "PATH=$(dirname "$node"):/usr/bin:/bin" "HOME=$1" "KIMI_CODE_HOME=$1" "TMPDIR=$dir/tmp" \
    KIMI_CODE_WATCH=0 KIMI_DISABLE_TELEMETRY=1 KIMI_DISABLE_CRON=1 KIMI_CODE_NO_AUTO_UPDATE=1 KIMI_CODE_REGION_MARKER=off
}
login() { # home
  mkdir -p "$1" && chmod 700 "$1"
  $(kimi_env "$1") KIMI_CODE_OAUTH_HOST="http://127.0.0.1:$port" KIMI_CODE_BASE_URL="http://127.0.0.1:$port/coding/v1" \
    "$node" "$kimi_main" login < /dev/null > "$dir/tmp/login.log" 2>&1 || die "kimi login failed"
}
expire() { # credential file: keep tokens, expire the access token now
  "$node" -e 'const f=process.argv[1];const fs=require("fs");const t=JSON.parse(fs.readFileSync(f,"utf8"));t.expires_at=Math.floor(Date.now()/1000)-5;fs.writeFileSync(f,JSON.stringify(t,null,2)+"\n",{mode:0o600})' "$1"
}
run_agents() { # mode(shared|copy) account_home -> sets results
  local mode="$1" account="$2" i
  local slot; slot="$(basename "$(ls "$account"/credentials/*.json)")"
  for i in $(seq 1 "$agents"); do
    local home="$dir/$mode-agent-$i"
    mkdir -p "$home" && chmod 700 "$home"
    cp "$account/config.toml" "$home/config.toml"
    cp "$account/device_id" "$home/device_id"
    if [[ "$mode" == shared ]]; then
      mkdir -p "$account/oauth"
      ln -s "$account/credentials" "$home/credentials"
      ln -s "$account/oauth" "$home/oauth"
    else
      mkdir -p "$home/credentials" && cp "$account/credentials/$slot" "$home/credentials/$slot"
    fi
  done
  local apids=()
  for i in $(seq 1 "$agents"); do
    local home="$dir/$mode-agent-$i"
    (cd "$dir/cwd" && set +e; $(kimi_env "$home") timeout 90 "$node" "$kimi_main" -p "say hi" > "$dir/tmp/$mode-$i.out" 2> "$dir/tmp/$mode-$i.err"; echo $? > "$dir/tmp/$mode-$i.code") &
    apids+=($!)
  done
  wait "${apids[@]}" || true
}

# --- shared directories (cutex) ---
start_server shared
login "$dir/account-shared"
slot_file="$(ls "$dir/account-shared"/credentials/*.json)"
expire "$slot_file"
run_agents shared "$dir/account-shared"
s="$(stats)"; echo "r266-race: shared stats $s"
fails=0
for i in $(seq 1 "$agents"); do
  code="$(cat "$dir/tmp/shared-$i.code")"
  if [[ "$code" != 0 ]] || ! grep -q FAKE-KIMI-REPLY "$dir/tmp/shared-$i.out"; then fails=$((fails+1)); sed -n 1,5p "$dir/tmp/shared-$i.err" >&2; fi
  [[ -L "$dir/shared-agent-$i/credentials" ]] || die "agent $i credentials link was replaced"
done
[[ "$fails" == 0 ]] || die "$fails of $agents shared agents failed"
"$node" -e 'const s=JSON.parse(process.argv[1]); if (s.refreshes!==1 || s.rejectedRefreshes!==0) process.exit(1)' "$s" \
  || die "shared agents did not coalesce into one refresh: $s"
grep -q '"access_token": "fake-access-2"' "$slot_file" || die "account credential is not the rotated token"
[[ -z "$(ls "$dir/account-shared/oauth" | grep '\.lock$' || true)" ]] || die "refresh lock left behind"
ok "$agents concurrent agents on shared dirs: all answered, exactly 1 refresh, 0 rejected, links intact"
# cutex logout = Kimi's revoked tombstone in the shared file: an agent of the
# account stops asking the model and never refreshes it back.
"$node" -e 'const f=process.argv[1];const fs=require("fs");const t=JSON.parse(fs.readFileSync(f,"utf8"));fs.writeFileSync(f,JSON.stringify({access_token:"",refresh_token:"",expires_at:0,scope:t.scope,token_type:t.token_type,expires_in:0},null,2)+"\n",{mode:0o600})' "$slot_file"
before="$(stats)"
(cd "$dir/cwd" && $(kimi_env "$dir/shared-agent-1") timeout 60 "$node" "$kimi_main" -p "say hi" > "$dir/tmp/logout.out" 2> "$dir/tmp/logout.err") && die "an agent answered after logout"
after="$(stats)"
grep -q FAKE-KIMI-REPLY "$dir/tmp/logout.out" && die "model answered after logout"
grep -q '"access_token": ""' "$slot_file" || die "the tombstone was overwritten"
[[ "$before" == "$after" ]] || die "logout: the agent still talked to the server ($before -> $after)"
ok "after logout (tombstone) an agent refuses without any refresh or model request"
stop_server

# --- copies (rejected strategy, control) ---
start_server copy
login "$dir/account-copy"
expire "$(ls "$dir/account-copy"/credentials/*.json)"
run_agents copy "$dir/account-copy"
s="$(stats)"; echo "r266-race: copy stats $s"
tomb=0
for i in $(seq 1 "$agents"); do
  f="$(ls "$dir/copy-agent-$i"/credentials/*.json)"
  grep -q '"access_token": ""' "$f" && tomb=$((tomb+1))
done
"$node" -e 'const s=JSON.parse(process.argv[1]); if (s.rejectedRefreshes<1) process.exit(1)' "$s" \
  || die "control did not reproduce the race (no rejected refresh): $s"
ok "control (copied credentials): $(echo "$s") — $tomb of $agents agent copies tombstoned; the account copy is stale"
stop_server
echo "r266-race: all checks passed"
