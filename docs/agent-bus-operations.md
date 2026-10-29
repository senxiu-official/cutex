# Agent Bus operations

## Uncertain active-message follow-up settlement

Capability `agent-bus-message-settlement-v1` adds an optional `settlement` to
message snapshots, stuck-message rows and `AdmissionNextStep` rows. Delivery
facts stay separate. `outcome_unknown` says the adapter-assigned actual turn
ended without a successful context-persisted receipt; neither output nor an
empty receipt alone is enough. Closing ends owner follow-up, never claims
delivery, non-delivery, successful handling, withdrawal or absence of effects.

The initial collector supports ordinary active Claude messages on the audited
`cute-claude-app-server` 0.8.5 interface. It does not start/recover an offline
runtime. Passive notes, other runtimes without sufficient evidence, protected
Task/Job/control/delegation internals, pending/running input, missing actual
binding and conflicting/reverted history have no close eligibility. For busy
input the explicit `contextTurnId` must identify the actual terminal turn;
`turnId` can be only a reservation. Native and local payload digests differ
and are independently checked. The adapter's attempt UUID is bookkeeping for
the assigned turn, not a provider request ID. List reads never probe runtimes.

The projection is:

```json
{
  "state": "outcome_unknown",
  "canClose": true,
  "evidenceToken": "opaque-server-issued-token",
  "evidence": {
    "runtime": "claude", "runtimeGeneration": 7,
    "threadId": "adapter-thread", "actualTurnId": "actual-turn",
    "terminalStatus": "failed", "observedAt": "2026-10-27T00:00:00Z"
  },
  "closure": null
}
```

Evidence has a 180-second lease. The observer refreshes bounded observations
through controlled reconciliation; stale evidence does not permit closure.
Optional background collection runs after the ordinary observer delivery/poll
pass and only while the settlement capability is advertised. Agent and message
cursors rotate fairly, with at most four messages and sixteen candidate visits
per phase. Native RPCs share a 250ms allowance, at most 75ms per message;
explicit owner collection keeps its independent 15-second proof budget.
Repository reads, identity fences and sidecar writes still use blocking IO, so
these limits are not a hard wall-clock bound for the entire observer pass.
Sidecar write failure is logged without aborting ordinary delivery or inventing
evidence. Failed or expired collection cannot authorize a close.
Closed state survives expiry, offline runtimes and later valid delivery.
Closure contains the authenticated actor, `closedAt`, reason and request ID.

`POST /v2/agent-bus/stuck-messages/close` uses existing owner Management
authentication and full device scope; agent credentials and non-owner/scoped
device credentials cannot use it. The body accepts exactly:

```json
{
  "messageId": "message-id",
  "expectedEvidenceToken": "opaque-server-issued-token",
  "requestId": "unique-owner-operation-id",
  "reason": "Owner ended follow-up; outcome remains unknown"
}
```

Byte limits are message ID 512, token 256, request ID 128 and reason 1024;
all must be nonblank. The server records the trimmed reason but compares the
exact request for idempotency. `200 {messageId,changed,settlement}` means the
audit close is durable. Same request key/payload/authenticated actor replays
with `changed:false`, including after restart or when the runtime is offline.
Reusing a key for another payload, message or actor conflicts. Invalid input
is 400, unauthorized is 403, unknown message is 404, stale/token/state/receipt
or ineligible conflicts are 409, unavailable observation/storage is 503.

Closure rechecks current facts on an already-running generation-fenced handle
and repeats token/identity/lease checks under the business-store write lock.
Runtime reads and the daemon commit are not one atomic snapshot. A receipt
observed before close rejects it; a later receipt updates original delivery
facts and retains the closure audit. The desktop refreshes conflicts and
retains the same request key across uncertain-write recovery; it must not
silently submit a new operation. It shows “关闭跟进，不代表送达或处理成功” with
an inline confirmation, and only offers the action with the capability and
server `canClose:true`.

### Persistence and rollback compatibility

All data uses `runtime/management-v2/management-state.sqlite3`, existing
`hot_store_meta` / `hot_store_rows` tables and SQLite WAL transactions.

- Rebuildable observations use namespace `agent_bus_settlement_evidence`,
  collection `evidence`, row key message ID, row value `{version:1,record}`.
  Namespace metadata has hot-store `format_version:1`. Record contains a
  server token and versioned, content-free native/local/actual-turn evidence;
  closure/request/audit are null and `pendingSessions` is empty. Unknown
  versions/fields and close data in this namespace are rejected. The original
  `agent_bus` message bytes, generation and format version do not change on
  automatic observation. Older readers may ignore the namespace; it has no
  delivery, retry, discard or admission authority by itself.
- Only an authenticated owner close writes `settlement` in the original
  `agent_bus` namespace, `message/<messageId>` row, and changes `state/version`
  to **8 in the same transaction**. That record holds the complete evidence,
  exact request, immutable audit (actor/time/reason/original state/identities,
  request and evidence digests) and per-session notification outbox. Existing
  r263/r264 readers reject version 8 instead of silently dropping closure or
  redriving its original message. Closed audit/request rows are not aged out.
- Close excludes the item from admission and resubmission but leaves receipt
  reconciliation available. `cutex/agentBus/messageSettlementUpdated` carries
  `{messageId,requestId,settlement}` to both session streams after commit.
  Per-session outbox acknowledgment repairs partial publication after restart
  or reads; a crash between publication and acknowledgment may repeat an event
  with the same request ID, never another business close. Consumers apply the
  projection idempotently and retain it when delivery events arrive later.

Observation changes also publish `messageSettlementUpdated` after the sidecar
commit, with `requestId` equal to the server evidence token (or a server
notification UUID when clearing it). `settlement:null` explicitly clears only
an unclosed observation. A known closure is never erased by null/stale replay
or a later delivery event. Observation notifications are rebuildable: a lost
publication is repaired on the next controlled refresh or authoritative
bootstrap/list read. Both sent and received bootstrap rows expose the same
projection. Actual owner closes use the durable per-session outbox above.

Kit guards may allow only the exact observation namespace/version described
above. `AgentBusMessageRepository::validate_settlement_observations_read_only`
validates that sidecar. `validate_settlement_rollback_read_only` refuses bus
format 8/closure before rollback writes; legacy export also refuses it. Thus
automatic observation preserves pre-close rollback eligibility, while an
actual close requires keeping the new reader. Back up the complete SQLite/WAL
state before any rollback attempt. There is no implicit conversion of a close
to discard or removal of audit to make an older writer accept it.

Operator notes for the Agent Bus daemon (`cutex agent serve`): where its token
comes from, its audit log, and how the owner settles messages that cannot be
delivered.

## 1. Bus token: never on the command line

`--token` puts the bus root token on the command line, where every local user
can read it (`ps`, `/proc/<pid>/cmdline`, Task Manager / `Get-CimInstance
Win32_Process`). The bus now takes its token, in this order:

| Order | Source | Notes |
|---|---|---|
| 1 | `--token VALUE` | **Deprecated**, still accepted for one release; prints a warning at start. |
| 2 | `--token-file PATH` | First non-empty line, trimmed, at least 16 characters. Regular file or a link to one (systemd `LoadCredential=`). Unix: refused if group / other have any permission bit (`chmod 600`). Windows: warns unless the file has cutex's owner-only protected DACL. |
| 3 | `CUTEX_AGENT_BUS_SERVE_TOKEN` | Environment of the bus process; cleared from the process environment right after it is read. This is how cutex itself starts the bus (management / CLI auto-start, instance mode). |
| 4 | `config.json` `agent_bus_token` | **The default.** The file is owner-private (0600 / owner ACL) and every client (agents, management, CLI) already reads the token from it. |

So the recommended service definition passes **no token at all**: `cutex agent
serve --port 24260`. The start log says which source was used
(`agent bus token source: config.json agent_bus_token`) and never prints the
token. If a token from 1–3 differs from `config.json`'s, the bus warns: local
agents and cutex clients authenticate with the `config.json` token.

Older releases (r216 and earlier) already fall back to `config.json` when
`--token` is absent, so the service definitions can be changed before or after
the binary upgrade.

### Hand-written service definitions

`cutex service install` (signed release installs) writes units / a supervisor
that pass no token. If you wrote your own systemd unit or service wrapper that
reads `agent_bus_token` and passes it as `--token`, drop that argument:

```bash
mkdir -p ~/.config/systemd/user/cutex-agent-bus.service.d
cat > ~/.config/systemd/user/cutex-agent-bus.service.d/60-token-from-config.conf <<'EOF'
[Service]
# The bus reads agent_bus_token from ~/.cutex/config.json itself; nothing on argv.
ExecStart=
ExecStart=/path/to/cutex agent serve --port 24260
EOF
systemctl --user daemon-reload
systemctl --user restart cutex-agent-bus      # agents reconnect; durable messages are redriven
# Check: no token on the command line, bus healthy.
pgrep -af 'cutex agent serve' | grep -c -- '--token'    # expect 0
curl -fsS http://127.0.0.1:24260/ && echo
```

## 2. Audit log rotation

`runtime/agent-bus-audit.jsonl` records sends, polls returning messages, ACKs,
forwards, live withdrawals and owner discards. Before this change it grew
without bound (hundreds of MB on a busy host, mostly identical `polled`
lines every few seconds for messages no runtime ACKs).

`config.json`:

```json
"agent_bus_audit": {
  "maxFileMb": 64,
  "keepFiles": 4,
  "gzip": true,
  "coalescePollsSecs": 600
}
```

All fields optional (defaults shown; out-of-range values fall back to the
default; read when the bus starts).

- **Rotation by size**: when the live file would pass `maxFileMb` (1–4096) it is
  renamed to `agent-bus-audit-<UTC yyyymmddThhmmss.mmmZ>.jsonl` under the lock
  file `agent-bus-audit.lock` (several processes append to it). The rename is
  O(1); the write path never reads the file.
- **Compression / retention**: a background thread gzips rotated files
  (streaming, `.jsonl.gz`, written to `.gz.tmp-<pid>` then renamed) and deletes
  all but the newest `keepFiles` (0–100) rotations. Leftovers of a crash
  (`.gz.tmp-*`, a plain file next to its finished `.gz`) are cleaned on the
  next pass. `gzip: false` keeps rotated files plain.
- **Poll coalescing**: a poll returning exactly the same message ids for the
  same runtime as its last written poll, within `coalescePollsSecs` (0–86400;
  0 = write every poll), is not written. The next written poll of that runtime
  carries `suppressed_repeats: N`. An ACK by the runtime ends the run.
- New audit files are created 0600 on Unix (content previews are private).
- **First start after the upgrade**: the bus checks the live file before its
  first write; an oversized file is renamed out at once
  and compressed in the background (streaming; it is never loaded into
  memory). It then counts as one of the `keepFiles` rotations and is deleted
  once four newer rotations exist. Expect one CPU core busy for some seconds
  while it is compressed, and roughly the old size again on disk until the
  `.gz` is complete.
- `cutex agent log [--agent X] [--limit N]` reads the live file and, when that
  holds fewer than N matching lines, the rotated ones (gz included), newest
  first.

Rollback to r216: the old bus appends to `agent-bus-audit.jsonl` again and
ignores rotated files and the `agent_bus_audit` block (an old binary that
rewrites `config.json` drops that block; defaults apply again after the next
upgrade).

## 3. Owner discard of stuck bus messages

A message is "stuck" when it stays undelivered: its durable record in
`runtime/management-v2/agent-bus-message-state.json` is `pending` /
`stale_held`, or a registered runtime keeps polling it without ACK. Until now
the only way out was an offline script editing that file, and even then
the bus kept the message in its in-memory queue until the runtime registered
again.

Capability `agent-bus-stuck-messages` (Management v2, root credential incl.
device / client tokens):

- `GET /v2/agent-bus/stuck-messages?minAgeSecs=3600&agentId=&limit=200`
  → `{messages, total, liveQueue: {available, error?}, minAgeSecs, generatedAt}`.
  Durable `pending` / `stale_held` records at least `minAgeSecs` old, plus
  entries of the bus's in-memory runtime queues that old (`durable: false`,
  `state: live_only` when the repository has no record). Oldest first. Each row:
  `messageId, state, durable, to {id, name}, from {id, name}, ageSecs,
  createdAtEpochSecs, senderKind, controlType, protectedControl,
  nativeSubmissionPresent, nativeLastObserved, semanticSha256, error,
  liveQueues [{agentId, agentName}], reasons [..], contentPreview` (200 chars).
  `agentId` filters by recipient or sender session.
- `POST /v2/agent-bus/stuck-messages/discard`
  `{messageId, reason, expectedSemanticSha256?, allowProtected?}` →
  `{messageId, discarded: true, changed, durable: {changed, previousState,
  state, toCutexSessionId} | null, liveWithdrawn: [{agentId}],
  liveWithdrawError?}`.
  1. The durable record becomes terminal: `discarded` when nothing was
     submitted to the native runtime yet (the existing never-redriven
     tombstone), otherwise `quarantined` (Cutex cannot retract what the
     native runtime may already hold). `error = {source: "human", code:
     "owner_message_discarded", message: reason, details: {previousState,
     previousError, semanticSha256, nativeSubmissionPresent, discardedAt}}`.
     Agent-sourced messages get `cutex/agentBus/messageFailed` on both
     sessions' streams once.
  2. The bus removes the id from every runtime queue (`POST
     /api/messages/withdraw`, root bus token; audit event `withdrawn`).
  3. An `owner_discarded` fact is appended to the audit log.

  Refusals: 400 `invalid_request`, 404 `message_not_found`, 409
  `message_digest_mismatch` / `message_not_discardable` (delivered, failed,
  …) / `message_protected` (Job / control messages need
  `allowProtected: true`: check the job first), 503
  `agent_bus_state_unavailable` / `agent_bus_unavailable`.

  Idempotent: a second call answers `changed: false` (when nothing was left to
  withdraw) and retries the live withdrawal. If the bus was unreachable
  (`liveWithdrawError`), nothing is lost: no redrive picks the record up again,
  and the agent's bridge ACKs an owner-discarded message on its next poll
  (Codex bridge: `suppress_native_delivery`; Claude runtime: state not
  `pending`). A message with no durable record (`live_only`) is only withdrawn;
  its producer may queue it again. Messages an older offline cleanup script quarantined
  (`{source: human, code: stale_message_discarded}`) count as owner-discarded
  too: the bridge ACKs them and a discard answers `changed: false`.

CLI (talks to the Management route; the record change and its event happen in
the management process):

```
cutex human bus-messages list [--min-age 1h] [--agent ID] [--limit 200] [--json]
cutex human bus-messages discard MESSAGE_ID --reason TEXT [--expected-sha256 HEX] [--allow-protected]
```

`--min-age` takes `30m`, `1h`, `2d`, seconds, or `0` for all.

The list also includes `inbox_accepted` messages whose native context receipt
is still missing. An explicit owner discard can quarantine these records,
preserving their frozen input and receipt evidence. It does not retract an
input already held by the runtime or claim that the model read it. Protected
task/control records still require `--allow-protected`; use
`--expected-sha256` to refuse a changed record.

Older owner tools stay: `cutex human external-input analyze|release|discard`
(pre-native `stale_held` only).

## Idle delegation results and owner input (r263)

Successful delegation results for an idle session batch for at most five seconds
from the oldest pending delivery, including a cold cache. Shorter configured
windows and disabled batching still apply. New arrivals and scheduler restarts
cannot extend that cap. Questions, failures and turn-completion bypasses remain
immediate; foreground-busy next-step timing is unchanged.

When a native-capable Claude session is idle and the owner sends a new input,
already-arrived delegation results are staged as authenticated passive external
context before that owner turn starts. Their source, structured view, native
identity and receipt remain separate from the owner's text. Pending or uncertain
acceptance is not an acknowledgement; only the native context receipt identifies
the consuming turn. A passive receipt never owns the turn admission slot.
Paused/numbered work keeps its existing ordering, and unsupported runtimes retain
the previous conservative path. A clearly oversized/full inbox can still prevent
combined context; the owner's instruction remains deliverable and results remain
available for explicit reading rather than being replayed.

## 4. Wake timing for a busy recipient (r217h1 / r217h2, typed in r220)

A wake (`after_turn` bus message: agent_tell wakes, owner wakes, alarms, Job
completions, watchdogs) or delegation results for a **busy** Codex recipient
that takes native `soon` wait for its running turn to end; once they have been
pending `wake_busy_wait_secs` they are read at the recipient's next step
instead (native `soon`, no turn of their own). An idle recipient still gets a
turn at once; Claude runtimes and natives without `soon` keep waiting for the
turn end. Capability `wake-busy-wait`.

`config.json`:

```json
"agent_bus": { "wake_busy_wait_secs": 120 }
```

Absent (or not a number) = 120; `0` = next step at once; `null` or negative =
never early (the r217 after-turn behaviour; saved back as `null`). Read per
delivery: a change applies without a restart.

`GET|PATCH /v2/settings`: `agentBus {wakeBusyWaitSecs: 0-86400 | null}`
(a negative PATCH value is stored as `null`), `agentBusDefault`,
`agentBusRange`.

Log (no content), stderr of the process that runs the delivery (management
journal): `cutex-wake-read {json}` per wake / delegation result read (`path`
`idle_turn` | `idle_exclusive` | `turn_end` | `early_next_step`, `waitedMs`,
`wakeBusyWaitSecs`, `producer`), and for an early read
`cutex-wake-counterfactual {json}` at that turn's end (`wouldHaveWaitedMs`).
