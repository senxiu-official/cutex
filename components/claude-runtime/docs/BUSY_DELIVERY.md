# Claude busy wake and delegation result delivery

Task 07, based on Cutex `47cab2d`, extends the r8 external-input path.

Before accepting a new Agent Bus wake, Cutex uses the shared `wake_timing` /
`wake_busy_wait()` policy. A busy recipient holds an unfrozen message until the
configured deadline. An idle recipient receives it immediately. After the
deadline, a subscription adapter advertising `externalInputDeliveries: soon`
accepts a frozen native `soon` envelope. Its digest never changes on retries.
Existing r7 inbox identities and existing frozen r8 envelopes keep their route;
r7 and r8 adapters without the new capability retain after-turn delivery.

The adapter enqueues `soon` in the existing durable FIFO and adds a `busy_inputs`
record in the same transaction. A main-agent PostToolUse callback must match the
native session, current bridge binding and a PreToolUse-admitted tool id. Child,
unknown, revoked and repeated callbacks cannot claim a wake. Claiming atomically
removes the input from the FIFO and records its consuming turn. The queued
receipt's reserved `turnId` remains stable; the legacy result receipt exposes
`contextTurnId` separately. Source, framing, asks, attempt and structured view
remain external data. No owner `turn/start` or `turn/steer` route is used.
The claim and hook return run without yielding; a turn end that wins before the
claim leaves the input untouched for its FIFO fallback. Timeline notifications
for an injected wake are published with the later context acknowledgement.

A hook offer and ordinary item output do **not** count as context persistence.
The existing r8 successful SDK Result remains the acknowledgement boundary.
Native inputs then receive their ordinary stable context receipt; legacy result
status sets `contextPersisted`. Interrupted, crashed or unreturned offers stay
unknown/held, with no automatic requeue. A restart turns unfinished `injecting`
claims into `uncertain`. An input not claimed by a hook still starts exactly one
ordinary FIFO turn after the current turn. This includes long turns with no
tools. Receipt retention uses the consuming turn; reverting that turn removes
its proof of current context while retaining the identity and audit receipt.

Delegation results retain the metadata-bearing `claude/input/submit` protocol.
Only a controller-submitted ANSWER/QUESTION with service source `cutex-asks`,
ask ids, the stable `cutex-ask-soon-` identity, and `admission: next_step` may use
this path. ASK admission, owner input, Plan, pause and delegation authorization
remain unchanged. `input.durableNextStepResults` gates Cutex's use of this path.
On a runtime generation change, the asks scheduler queries the same durable
message id on the new generation. Timeout, missing inbox and unknown status
never authorize generating a new result identity.

`cutex-wake-read` keeps `idle_turn`, `turn_end` and `early_next_step`; delegation
results retain their existing log paths. The consuming turn determines whether
a queued soon input actually took the early path or fell back to a new turn.
An early receipt also feeds `cutex-wake-counterfactual`. As in r8, the read log
timestamp reflects confirmed persistence, which can be later than the hook
offer. Timing observations affect diagnostics only, never delivery decisions.

The new SQLite table is an additive schema-2 migration. Neither manifests nor
release versions are changed in this task. Before deployment, integrate the
AlarmSystem route from task 01 and the account/runtime fixes from task 04. When
integrating the separate owner-steer branch, preserve the terminal ordering:
close tool-context admission before awaiting terminal receipt publication.

The opt-in native probe uses the installed SDK's bundled CLI explicitly, a
private home and a fake Anthropic provider on loopback port 25970. It uses no
real credentials and prints only marker evidence. With a Read tool, two model
requests produce one turn and the second request contains the wake marker.
Without tools, two requests produce two turns and exactly one fallback delivery.
This verifies Windows SDK/CLI scheduling against a simulated model, not real
model behavior or the complete desktop/management deployment.

```powershell
$env:CUTEX_BUSY_DELIVERY_NATIVE_SMOKE = '1'
python -m pytest -q -s tests/test_busy_delivery_native.py
```

Run with the repository's required private HOME / CUTEX_TEST_PRIVATE_HOME and
marker. The probe is skipped by default, and refuses an occupied port by failing
the bind. It closes only the runtime and provider it starts.

## Owner steer (2026-10-17)

`turn/steer` reuses this path for the owner's own input: a `soon` row with message id
`steer-<clientUserMessageId>` and a user source. At the claiming PostToolUse it is injected like a
wake, and the adapter also writes the user message into the running turn (item started / completed,
`steered: true`); `acknowledge` does not emit it again. Unclaimed at turn end, the FIFO runs it as
the next turn. A resend with the same client id is deduplicated by the queue.

### Interrupt and unread steers (claude-r17, claude-r18)

A steer the turn never read is dropped (input state `interrupted`) when the owner interrupts: since
claude-r18 at the `turn/interrupt` request, before the abort reaches Claude Code (claude-r17: only
when the turn ended `interrupted` / `failed`). The desktop's interrupt & send reads the stopped turn
and resends every steer it did not record (a new client id, merged with the composer text), so the
adapter must not deliver it as well: before claude-r17 the stale `soon` row was injected into the
resent turn at its next PostToolUse, or ran as one more turn after it (owner notes, claude-r16). A
hook between the request and the abort, and a turn that completes anyway, now find nothing to
inject. If the interrupt request itself fails, the dropped steers are queued again. A steer with
images (no `busy_inputs` row, never a hook) carries `metadata.steer` and is dropped the same way.
A steer already read by a hook stays read: the desktop sees its `steered` item and does not resend.

## Passive notes at turn start (claude-r21)

A passive note (`deliveryMode: passive`, native `delivery: passive`) never starts a turn. It is read
into context once, at the earliest of: the start of the next turn of any kind, or the next
main-agent PostToolUse of a running turn. `Store.claim` takes the pending notes when it claims a
turn (`notes.state = injected`, `turn_id` = that turn) and records their `external_event` items,
which the turn emits at its start; cutex reads them as the delivery receipt (`messageDelivered`),
native envelopes are context-persisted with the turn's successful Result. A later PostToolUse finds
nothing left to inject.

- An agent's or service's turn: the note frames precede its input in its one verbatim prompt
  (unchanged).
- An owner turn, including the owner's wake (`cutex/agentBus/wake`: "Read and handle the unread
  notes other agents left for you.") and a FIFO-run steer: the SDK user message gets one text block
  per note (`Store.turn_note_frames`) followed by the owner's text as the **last** text block (then
  images). Claude Code reads only the last text block as typed input, so the owner's `@path`
  mentions still expand and the notes' text is passed on as written. The timeline shows the
  `userMessage` first, then the note items. With no note pending the message is unchanged.
- `/compact` reads no notes; they wait for the next turn.

Before claude-r21 an owner turn skipped pending notes to keep its text as written: they came only
after its first tool call, and a turn without tool calls never saw them (owner's 唤醒 on hachimi,
2026-10-19). Codex consumes passive ExternalInput at a turn's first input boundary, so both runtimes
now behave alike. Opt-in check against the bundled CLI and a loopback model:
`test_real_cli_owner_wake_reads_notes_in_its_first_request` (CLI 2.1.299, SDK 0.2.166, Linux): the
note is in the first and only request, the owner's `@path` expands, the note's does not.

## Autonomous turns (claude-r18 decision)

Claude Code starts a turn by itself when a `run_in_background` Bash command finishes (its task
notification). That turn has no adapter input: the reader reports its messages only as
`claude/runtime/event` (`whileIdle: true`, kind and status only, no content), nothing reaches the
timeline, and the PreToolUse hook denies every tool (`No active turn accepts tools`). Attributing it
to a synthetic system turn would need the adapter to own a turn Claude Code started (input, receipts,
interrupt, usage) and is not done; tool admission is not loosened for an unattributed turn. Instead
the preset system prompt append ends with a runtime-limits note (`config.RUNTIME_LIMITS`): such a
turn cannot use tools and is not seen by the user, so the model checks the result in the next turn the
user starts. The same note says background subagents (`Agent` / `Task` with `run_in_background`) are
refused and to use foreground ones; the hook refusal stays.

Inputs wait for it (claude-r21, G5). Claude Code queues an input that arrives while its own turn runs
and answers it after that turn's Result, so submitting at once would hand the rest of the autonomous
turn (its text and its Result) to the new adapter turn: wrong reply text and a premature completion.
The runtime owner therefore tracks the autonomous turn (`RuntimeOwner.autonomous_until`): with no
adapter turn active, a stream frame (`stream_event`, `assistant`, `user`, not a subagent's) opens or
extends a 3 s quiet window (`AUTONOMOUS_QUIET_SECONDS`), a task notification a 10 s start window
(`AUTONOMOUS_START_SECONDS`, the turn's first frame follows the model's first-token latency), and any
other non-rate-limit frame extends an open window; the autonomous Result closes it. A `Run` that
arrives while the window is open is held in order, not refused, and submitted when the window closes,
or after at most 120 s (`AUTONOMOUS_WAIT_SECONDS`, logged to stderr) anyway. The owner keeps reading
while an input waits; `turn/interrupt` of a held input fails it as interrupted before submission, and
close or a supervised reconnect fails held inputs as never submitted. Not covered: a notification that
arrives during an adapter turn and makes Claude Code start its own turn right after that turn's Result,
before any frame of it reaches the adapter.

## Validation (Windows, 2026-10-11)

| Check | Baseline `47cab2d` | Task 07 |
| --- | --- | --- |
| Adapter pytest | 380 passed, 6 skipped | 394 passed, 10 skipped |
| Rust library | 1534 passed, 10 failed, 13 ignored | 1535 passed, same 10 failed, 13 ignored |
| Rust cutex binary | 714 passed, 14 failed, 5 ignored | 717 passed, 13 failed, 5 ignored |
| Rust integration | 3 passed, 2 failed, 2 ignored | Same |
| Opt-in SDK/CLI loopback | Not present | 4 passed |

The new Rust timing, metadata/receipt/digest and durable restart tests pass.
The complete Rust suites remain red on pre-existing Windows path, manifest,
notification, profile and TUI cases. The parallel binary run additionally hit
`ordinary_redrive_targets_only_current_durable_runtime_generation` with a
removed temporary stream-state directory. Serial rerun of its unchanged module
passed that case (13 passed, 3 failed); all three remaining failures also appear
in the baseline. This is not a claim that the full suite is green.

Both revisions used `--locked` debug builds with private marked homes and the
handoff's two prescribed skips. Baseline `--tests` stopped at the existing lib
failures, so the integration targets and cutex-mcp were also run explicitly with
`--no-fail-fast`; the final task run used `--lib --bins --tests --no-fail-fast`.
The native probe used SDK `0.2.164`, CLI `2.1.292`, SHA-256
`eb95bb65955f8b1702e800815f9a2c0388a5de0f196c354c7ee1dd5cb9a2ba23`.
All four wake/result × tool/no-tool cases verified marker visibility in the
second provider request, confirmed context receipts, and one or two turns as
appropriate. No Linux, real model, full management daemon or desktop acceptance
is claimed.
