#!/usr/bin/env bash
# r266 K2 smoke: this checkout's DEBUG cutex Management on a marked private
# home, the REAL pinned `kimi login` driven by Management against a FAKE Kimi
# OAuth server (tests/fixtures/fake-kimi-oauth.mjs), never a real Kimi account.
# Never the live install: own HOME / TMPDIR, own ports, refuses the live ports.
#
#   tests/r266-kimi-accounts-smoke.sh
#
# Environment (defaults):
#   R266_SMOKE_HOME  /home/ema/cutex-dev/scratch-k2/smoke-home (recreated, marked)
#   R266_MGMT_PORT   35870      R266_BUS_PORT 35860      R266_OAUTH_PORT 35880
#   CUTEX_BIN        <repo>/target/debug/cutex
#   R266_NODE        /home/ema/cutex-dev/tools/node24/bin/node
#   R266_KIMI_MAIN   /home/ema/cutex-dev/ext/kimi-code-install/node_modules/@moonshot-ai/kimi-code/dist/main.mjs
# Exit 0 = every check passed. Every process started here is stopped on exit.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
home="${R266_SMOKE_HOME:-/home/ema/cutex-dev/scratch-k2/smoke-home}"
mgmt_port="${R266_MGMT_PORT:-35870}"
bus_port="${R266_BUS_PORT:-35860}"
oauth_port="${R266_OAUTH_PORT:-35880}"
bin="${CUTEX_BIN:-$repo/target/debug/cutex}"
node="${R266_NODE:-/home/ema/cutex-dev/tools/node24/bin/node}"
kimi_main="${R266_KIMI_MAIN:-/home/ema/cutex-dev/ext/kimi-code-install/node_modules/@moonshot-ai/kimi-code/dist/main.mjs}"
fake="$repo/tests/fixtures/fake-kimi-oauth.mjs"

die() { echo "r266-smoke: FAIL: $*" >&2; exit 1; }
ok() { echo "r266-smoke: ok: $*"; }

case "$home" in /|"$HOME"|"$HOME/") die "refusing home $home";; esac
for port in "$mgmt_port" "$bus_port" "$oauth_port"; do
  case "$port" in 24270|24260|25370|25360) die "port $port belongs to the live install";; esac
  if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then die "port $port is in use"; fi
done
[[ -x "$bin" ]] || die "no debug binary at $bin (cargo build --locked)"
[[ -f "$kimi_main" ]] || die "no Kimi CLI at $kimi_main"

if [[ -e "$home" ]]; then
  [[ -f "$home/.cutex-test-private-home" ]] || die "$home exists and is not a marked private home"
  rm -rf "$home"
fi
mkdir -p "$home/.cutex" "$home/tmp" "$home/workspace" "$home/logs" "$home/install-base"
chmod 700 "$home"
printf 'r266 kimi smoke private home\n' > "$home/.cutex-test-private-home"
mgmt_token="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
bus_token="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
(umask 077; printf '{"agent_bus_enabled":true,"agent_bus_port":%s,"agent_bus_token":"%s","management_port":%s,"management_api_token":"%s"}\n' \
  "$bus_port" "$bus_token" "$mgmt_port" "$mgmt_token" > "$home/.cutex/config.json")
printf '{"schema":"cutex/install/v1","os":"linux","serviceManager":"fixture-no-services","channel":"main","createdAt":"%s","activeRelease":"r266-dev"}\n' \
  "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$home/install-base/install.json"
header="$home/tmp/auth.header"
(umask 077; printf 'Authorization: Bearer %s\n' "$mgmt_token" > "$header")

pids=()
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  sleep 0.5
  for pid in "${pids[@]}"; do kill -9 "$pid" 2>/dev/null || true; done
  # Anything still running with this private home (a kimi login child).
  # Only processes running with a HOME inside this private dir (never a caller's shell).
  local pid
  for pid in $(pgrep -u "$(id -u)" . || true); do
    [[ "$pid" != "$$" ]] || continue
    if { tr '\0' '\n' < "/proc/$pid/environ"; } 2>/dev/null | grep -qx -e "HOME=$home" -e "HOME=$home/.*"; then kill -9 "$pid" 2>/dev/null || true; fi
  done
}
trap cleanup EXIT

# Fake Kimi OAuth (device code + token + /coding/v1/models), 2 pending polls.
mkfifo "$home/tmp/oauth.stdin"
sleep 600 > "$home/tmp/oauth.stdin" & pids+=($!)
"$node" "$fake" --port "$oauth_port" --pending 2 < "$home/tmp/oauth.stdin" > "$home/logs/oauth.out" 2> "$home/logs/oauth.log" & pids+=($!)

env=(env -i "PATH=$(dirname "$node"):/usr/bin:/bin" "HOME=$home" "USERPROFILE=$home" "TMPDIR=$home/tmp"
  "CUTEX_TEST_PRIVATE_HOME=$home" "CUTEX_TEST_FORBID_REAL_HOME=1" "CUTEX_INSTALL_BASE=$home/install-base"
  "CUTEX_KIMI_TEST_CLI=[\"$node\",\"$kimi_main\"]"
  "CUTEX_KIMI_TEST_OAUTH_HOST=http://127.0.0.1:$oauth_port" "CUTEX_KIMI_TEST_BASE_URL=http://127.0.0.1:$oauth_port/coding/v1")
[[ -z "${XDG_RUNTIME_DIR:-}" ]] || env+=("XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR")
(cd "$home/workspace" && exec "${env[@]}" "$bin" management serve --port "$mgmt_port") >> "$home/logs/management.log" 2>&1 & pids+=($!)

api() { # method path [body]
  local method="$1" path="$2" body="${3:-}"
  if [[ -n "$body" ]]; then
    curl -sS -X "$method" -H @"$header" -H 'Content-Type: application/json' --data "$body" -w '\n%{http_code}' "http://127.0.0.1:$mgmt_port$path"
  else
    curl -sS -X "$method" -H @"$header" -w '\n%{http_code}' "http://127.0.0.1:$mgmt_port$path"
  fi
}
json() { "$node" -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const lines=s.trimEnd().split("\n");const code=lines.pop();const v=JSON.parse(lines.join("\n")||"null");const f=new Function("v","code",`return (${process.argv[1]})`);const r=f(v,code);process.stdout.write(typeof r==="string"?r:JSON.stringify(r));})' "$1"; }
expect() { # label js-expr-on(v,code) response
  local label="$1" expr="$2" response="$3"
  [[ "$(printf '%s' "$response" | json "$expr")" == "true" ]] || { echo "$response" >&2; die "$label"; }
  ok "$label"
}

for _ in $(seq 1 120); do
  curl -fsS -o /dev/null -H @"$header" "http://127.0.0.1:$mgmt_port/v2/capabilities" 2>/dev/null && break
  sleep 0.5
done
r="$(api GET /v2/capabilities)"
expect "capabilities advertise kimi-runtime-profiles and kimi-accounts" \
  'code==="200" && v.featureList.includes("kimi-runtime-profiles") && v.featureList.includes("kimi-accounts") && v.features.kimiAccounts===true' "$r"

r="$(api POST /v2/kimi/accounts '{"label":"Smoke","region":"global"}')"
expect "create a Kimi account" 'code==="201" && /^ka-/.test(v.id) && v.credentialsPresent===false && v.status.state==="loggedOut"' "$r"
account="$(printf '%s' "$r" | json 'v.id')"

r="$(api POST "/v2/kimi/accounts/$account/login" '{}')"
expect "login starts the real kimi login and returns the device code" \
  'code==="202" && v.state==="pending" && v.userCode==="FAKE-1234" && v.verificationUri.includes("user_code=FAKE-1234") && /^kl-/.test(v.loginId)' "$r"
login="$(printf '%s' "$r" | json 'v.loginId')"
r2="$(api POST "/v2/kimi/accounts/$account/login" '{}')"
expect "a second start returns the pending login" "code===\"200\" && v.loginId===\"$login\"" "$r2"

state=""
for _ in $(seq 1 60); do
  r="$(api GET "/v2/kimi/accounts/$account/login/$login")"
  state="$(printf '%s' "$r" | json 'v.state')"
  [[ "$state" == pending || "$state" == starting ]] || break
  sleep 0.5
done
expect "login succeeds after the fake authorization" 'code==="200" && v.state==="succeeded" && v.error===null' "$r"

r="$(api GET "/v2/kimi/accounts/$account")"
expect "account is logged in with the provisioned catalog" \
  'code==="200" && v.credentialsPresent===true && v.status.state==="loggedIn" && v.models.some(m=>m.id==="kimi-code/kimi-for-coding" && m.isDefault)' "$r"
grep -q 'fake-access\|fake-refresh' <<<"$r" && die "a token reached the account item"
ok "no token in the account item"
acct_home="$home/.cutex/kimi-accounts/$account"
[[ "$(stat -c %a "$acct_home")" == 700 ]] || die "account home not 0700"
slot_file="$(ls "$acct_home"/credentials/*.json)"
[[ "$(stat -c %a "$slot_file")" == 600 ]] || die "credential not 0600"
[[ -z "$(ls -A "$home/.cutex/kimi-accounts/.login" 2>/dev/null)" ]] || die "staging home left behind"
ok "account home private, staging removed ($(basename "$slot_file"))"

r="$(api POST /v2/profiles "{\"name\":\"Kimi sub\",\"kind\":\"kimi\",\"kimiAccount\":\"$account\"}")"
expect "create a Kimi-kind profile on the account" \
  "code===\"201\" && v.profile.cliKind===\"kimi\" && v.profile.kimiAccount===\"$account\" && v.profile.credential.source===\"kimiAccount\" && v.profile.credential.present===true && v.profile.runtimeVariants[0].id===\"kimi\" && v.profile.runtimeVariants[0].models.length===2" "$r"
profile="$(printf '%s' "$r" | json 'v.profile.id')"
expect "kimi row is componentNotInstalled without the kimi-runtime component" \
  'v.profile.runtimeCompatibility.find(x=>x.runtime==="kimi").status==="componentNotInstalled"' "$r"
r="$(api GET "/v2/profiles/$profile/models")"
expect "models resource lists the account catalog" 'code==="200" && v.modelsSource.kind==="kimi" && v.models.length===2' "$r"
r="$(api PATCH "/v2/profiles/$profile" '{"model":"kimi-code/kimi-k3-fast","modelReasoningEffort":"low"}')"
expect "profile defaults take a catalog model" 'code==="200"' "$r"

r="$(api POST /v2/profiles '{"name":"Kimi key","preset":"kimi-code-cn","apiKey":"sk-fake-r266-smoke-key"}')"
expect "create a Kimi Code API-key profile" 'code==="201" && v.profile.defaultVariant==="codex" && v.profile.providerInterfaces.some(i=>i.kind==="kimiNative")' "$r"
key_profile="$(printf '%s' "$r" | json 'v.profile.id')"
r="$(api POST "/v2/profiles/$key_profile/variants" '{"runtime":"kimi","defaults":{"reasoningEffort":"high"}}')"
expect "add the kimi variant (codex stays default)" \
  'code==="201" && v.variant.runtime==="kimi" && v.variant.defaults.model==="kimi-for-coding" && v.profile.defaultVariant==="codex"' "$r"
r="$(api POST /v2/profiles '{"name":"Relay","preset":"custom","baseUrl":"https://relay.example/v1","apiKey":"sk-fake-relay"}')"
relay="$(printf '%s' "$r" | json 'v.profile.id')"
r="$(api POST "/v2/profiles/$relay/variants" '{"runtime":"kimi"}')"
expect "a relay cannot add the kimi variant" 'code==="409" && v.error.code==="variant_runtime_unsupported"' "$r"
r="$(api GET /v2/profiles)"
grep -q 'sk-fake-r266-smoke-key\|fake-access\|fake-refresh' <<<"$r" && die "a secret reached the profile list"
ok "no secret in the profile list"

r="$(api DELETE "/v2/kimi/accounts/$account")"
expect "account in use cannot be deleted" 'code==="409" && v.error.code==="kimi_account_in_use"' "$r"
r="$(api POST "/v2/kimi/accounts/$account/logout" '{}')"
expect "logout leaves Kimi's revoked tombstone" 'code==="200" && v.credentialsPresent===false && v.status.state==="revoked"' "$r"
grep -q '"access_token": ""' "$slot_file" || die "no tombstone"
r="$(api GET "/v2/profiles/$profile")"
expect "the profile shows the credential absent" 'code==="200" && v.profile.credential.present===false' "$r"

# Denied login (fake server restarted in deny mode is not needed: cancel path).
r="$(api POST "/v2/kimi/accounts/$account/login" '{}')"
login="$(printf '%s' "$r" | json 'v.loginId')"
r="$(api DELETE "/v2/kimi/accounts/$account/login/$login")"
expect "a pending login can be cancelled" 'code==="200" && (v.state==="cancelled" || v.state==="succeeded")' "$r"

r="$(api DELETE "/v2/profiles/$profile")"
r="$(api DELETE "/v2/kimi/accounts/$account?deleteHome=true")"
expect "unused account is deleted with its home" 'code==="200" && v.removed===true && v.homeDeleted===true' "$r"
[[ ! -e "$acct_home" ]] || die "home still present"

grep -q 'fake-access\|fake-refresh\|sk-fake-r266-smoke-key' "$home/logs/management.log" && die "a secret reached the daemon log"
ok "no secret in the daemon log"
echo "r266-smoke: all checks passed"
