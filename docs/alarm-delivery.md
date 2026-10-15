# Alarm delivery reliability

Session 1, branch `gpt/alarm-delivery`, based on `47cab2d8af3001fca1675e7f3541d72728d53172`.
This change is limited to task 01(a). No deployment, runtime installation, live-home
query, adapter edit, real model request, health scan or busy-wake policy change.

## Delivery contract

- `AlarmSystem` ordinary messages enter the durable Bus ledger before enqueue.
  The scheduler's `alarm-…` message ID is also the Bus message ID. Replays verify
  canonical content, reuse the record across Bus/runtime restarts and report
  `deduplicated`. Replays of accepted or terminal messages never reopen delivery.
  `fromCutexSessionId` stays null.
- `bus_bridge::alarm_inter_agent_params` is the shared stable system projection.
  Model text is `Message Type: ALARM\nPayload:\n<reminder>`, as in ExternalInput.
  It neither invents a sending agent nor depends on a recipient's display name.
  A fixed SHA-256 vector guards this projection.
- Claude receives source `{kind: "service", id: "Cutex Alarm"}`, event type
  `alarm`, and `after-turn` delivery. `claude_ingress::envelope` accepts this
  typed system projection. Native envelopes remain frozen and r7 known-ID
  status probing remains in place. Local inbox acceptance is not context
  persistence. Native timeline items are not used as context receipts; native
  receipt recovery remains authoritative. Legacy v2 alarm items retain their
  existing timeline-based context receipt semantics.
- The scheduler commits `queued` after Bus acceptance. A consumer racing that
  commit waits on `AlarmAwaitingQueueCommit`; exact content/target/provenance
  conflicts are `InvalidAlarmProvenance`. Store I/O and decode errors remain
  retryable rather than discarding reminders.
- Permanent legacy/ExternalInput failures with no durable record write
  `live_message_terminal` to the existing audit log before ACK. This avoids
  manufacturing a canonical record from an invalid live envelope. Audit-write
  failure retains the message. Durable failures continue to be quarantined.
- `caller_owner` excludes retired records (including retained `retired_at`)
  and records with a known dead runtime PID. Unknown PID is not proof of death.
  Remaining ambiguity names the candidate session IDs and points to
  `cutex human doctor --check` (provided by task 05). This facade correction
  only reaches agents using the new bundle.

## Interfaces and integration

No Management route, public schema export, capability or store version change.
`AgentBusSendResponse` accepts the existing camelCase `queueDurability`,
`deliveryState`, `requiredAckLevel` fields as aliases; the wire response is
unchanged. `alarm_list.receipt.delivery_state` now contains the acceptance state;
it is not a continuously refreshed Bus delivery-status query.

Claude v2 views add internal schema `cutex.alarm.v1`. The adapter already accepts
service sources, arbitrary event types and structured views; it is unchanged.
The existing Rust repository `canonical_message` accessor becomes public so the
binary's send path can preserve the original creation time on durable retries.

Task 07 should preserve the alarm branch of `claude_message_params`, the typed
alarm acceptance in `claude_ingress::envelope`, and the native-vs-legacy receipt
distinction in `record_agent_message_item`. `claude_message_is_settled` also
withdraws audited historical live-only alarms before any native submission.
Alarm delivery remains after-turn.
Task 06 target parsing is separate from this task's `caller_owner` filtering.

## Historical query (owner-run, read-only)

Previously fired alarms can say `queued` with no Bus durable record. There is no
migration or replay in this change. The supplied query compares scheduler IDs,
receipt Bus IDs and canonical external IDs, including historical random Bus IDs:

```powershell
python scripts/find-lost-alarms.py --cutex-state 'D:\owner-snapshot\.cutex'
```

Use a consistent owner-created snapshot when possible. The script requires an
explicit state directory and never connects to a daemon or changes files. It was
not run against live state.

## Validation

Evidence is retained under this worktree's `.local/alarm-evidence/`. Baseline
source was exported with `git archive` into `.local/alarm-baseline/`, with its
own target and private home. Branch targets are under this worktree's `target/`.
All Cargo runs use the marked private HOME and `CUTEX_TEST_PRIVATE_HOME`, jobs=2,
locked dependencies, debug builds, and the two handoff skips. Full-suite results
and fixture evidence are recorded in the final handoff below.

The loopback smoke is `tests/alarm_delivery_smoke.py`. It takes `--home` (a new
directory beneath USERPROFILE), `--evidence`, and `--envelope` (the Rust test's
`claude-alarm-envelope.json`). It runs the unchanged adapter and the existing
fake SDK/native-process fixture; it never imports the official SDK or reads
login credentials. It covers idle and busy after-turn delivery, native digest
parity, deduplication and the legacy input contract.

The requested daemon smoke was attempted on 25160/25170, but the baseline
`validate_agent_bus_port` rejects non-24xxx ports in default-home mode. Using an
independent instance would conflict with the required `CUTEX_TEST_PRIVATE_HOME`
(`CUTEX_HOME cannot be combined with CUTEX_TEST_PRIVATE_HOME`). Public port and
home rules are outside this task and remain unchanged. The failing startup log
and attempted script are retained in `daemon-smoke/`; no daemon ran on a live
port. Therefore the requested one-minute daemon scenarios, full Codex native
runtime delivery and actual Bus process restart are **not accepted**. Rust
isolated tests cover the scheduler, stable retry/redrive and ExternalInput
projection/frozen-envelope/receipt behavior separately.

### Results (Windows, 2026-10-11 Australia/Sydney)

| Run | Baseline | Branch |
| --- | --- | --- |
| `--lib` | 1533 passed / 11 failed / 13 ignored | 1539 passed / 10 failed / 13 ignored |
| `--bins` | 712 passed / 16 failed / 5 ignored / 1 filtered | 716 passed / 15 failed / 5 ignored / 1 filtered |
| `--tests` library rerun | 1534 passed / 10 failed / 13 ignored | 1539 passed / 10 failed / 13 ignored |
| `--tests` binary rerun | 714 passed / 14 failed / 5 ignored / 1 filtered | 716 passed / 15 failed / 5 ignored / 1 filtered |
| Integration targets | 3 passed / 2 failed / 2 ignored | same |
| Targeted `--lib alarm` | — | 9 passed |
| Targeted `--bin cutex alarm` | — | 9 passed |

No new failure name across the union of the required baseline runs. The alarm
durability test now passes with its original assertions intact. It runs in a
child process because the repository singleton otherwise retains another
test's deleted HOME. The baseline failures
`ordinary_digest_uses_stable_base_name_without_changing_display_name` and
`ordinary_send_commits_v2_ledger_before_returning_durable_pending` remain; their
Windows errors concern a removed private test directory (`os error 3`).

The pre-existing `ordinary_redrive_targets_only_current_durable_runtime_generation`
and `pending_message_survives_agent_and_daemon_restart_keyed_by_session` failed
in baseline `--bins`, happened to pass in its `--tests` rerun, and failed in the
branch rerun. `managed_lifecycle_and_open_starts_stopped` failed only in the first
baseline library run. These variations are not claimed as fixes. Integration
failures remain `retire_and_restore_json_lookup_failures_are_typed_process_envelopes`
and `retire_human_lookup_failure_keeps_human_presentation`.

Evidence relative to `.local/alarm-evidence/`:

- `baseline-{lib,bins,tests}.log`, `final-{lib,bins,tests}.log`,
  `test-comparison.json`: complete commands' output and failure-name comparison.
- `alarm-{lib,bin}-targeted.log`: 18 targeted passes in total.
- `codex-external-fixture.log`, `codex-alarm-envelope.json`,
  `codex-alarm-receipt.json`: isolated Codex-shaped native projection and bound
  receipt test, including wrong-owner rejection before committing delivery.
- `claude-alarm-envelope.json`, `claude-alarm-legacy-params.json`:
  Rust-produced system inputs, used directly by the Python protocol smoke.
- `adapter-smoke-final/adapter-smoke.json`: **passed**. Native idle acceptance
  is pending; an explicit context receipt follows. Replay starts no extra SDK
  query. Busy delivery has no receipt until the preceding turn completes.
  Legacy submit/status also completes with the service source. SDK input excerpt:

  ```text
  EXTERNAL_INPUT_JSON
  {"eventType":"alarm","source":{"id":"Cutex Alarm","kind":"service"},"text":"Message Type: ALARM\nPayload:\nreminder 私有"}
  ```

- `daemon-smoke/bus-1.log`, `smoke-driver.log`: requested-port startup rejection;
  `daemon-smoke/attempt-script.py` retains the attempted harness. Unaccepted
  scenarios are listed above, not counted as passing.
- `lost-query-test.json`: read-only historical query passed against synthetic
  new-ID, old random-ID, missing-record and future-alarm rows.

All fake SDK children were closed by the harness and the attempted Bus process
exited. No push, merge, package install or deployment was performed. Source
`claude-adapter/`, Cargo manifests/lockfile and public route/capability/schema
tables are unchanged. A task-local Python venv under `.local/alarm-venv/` holds
test dependencies only.
