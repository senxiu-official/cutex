# Agent Bus operations

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

Older owner tools stay: `cutex human external-input analyze|release|discard`
(pre-native `stale_held` only).

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
