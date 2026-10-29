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

Claude Code starts a turn by itself when a background task finishes: a `run_in_background` Bash
command, and since claude-r23 a background subagent (its task notification). That turn has no adapter
input: the reader reports its messages only as `claude/runtime/event` (`whileIdle: true`, kind and
status only, no content), nothing reaches the timeline, and the PreToolUse hook denies every tool
(`No active turn accepts tools. …`, `subscription.NO_TURN_TOOLS`). Attributing it to a synthetic
system turn would need the adapter to own a turn Claude Code started (input, receipts, interrupt,
usage) and is not done; tool admission is not loosened for an unattributed turn. The preset system
prompt append says so (`config.RUNTIME_LIMITS`); the full reasoning is in "Claude Code's notification
turn" below.

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
close or a supervised reconnect fails held inputs as never submitted. A task notification that arrives
while an adapter turn runs (claude-r23) opens the 10 s start window at that turn's Result
(`RuntimeOwner.notified_in_turn`): Claude Code may start its own turn right after that Result, before
any frame of it reaches the adapter, so the next input waits for it the same way. If Claude Code folded
the notification into the running turn instead, the next input only waits out the 10 s window.

## Background subagents (claude-r23)

Owner decisions of 2026-10-19 and 2026-10-21; behaviour of Claude Code 2.1.301 / SDK 0.2.166 as
recorded by the probe of 2026-10-21 (cutex-desktop `docs/research/claude-background-subagents-probe-result.md`,
replayed by `tests/test_background_subagents.py` from `tests/fixtures/background_subagent_probe.json`).
Capability: `input.backgroundSubagents` and `features.backgroundSubagents`.

- Admission. A main-agent `Agent` / `Task` call with `run_in_background: true`, or without the
  argument (newer Claude Code then runs it in the background), is admitted; `run_in_background: false`
  stays a foreground subagent. The spawn is remembered by its tool-use id with its origin turn; the
  `task_started` frame (`tool_use_id`, `is_backgrounded`) names the child (task id = agent id) and
  moves it to `SdkToolBridge.background`, which `revoke()` keeps. A subagent cannot start a
  background subagent.
- Child tools. From then on the child's PreToolUse is admitted by agent id and native session, with
  or without a cutex turn: native tools and host-selected MCP tools run as usual (write / exec
  switches, MCP tool filters and plan mode's read-only rule apply), never written to a parent
  timeline. Denied in a background child: cutex tools (`mcp__cutex__*`), `Agent` / `Task`,
  `AskUserQuestion`. Its PostToolUse never reads notes or busy inputs (only a main-agent tool id the
  live turn admitted can: `callback_valid`).
- Approvals. Claude Code asks `can_use_tool` mid-run for a background child (`agent_id` set). Outside
  full access the answer is a deny with `BACKGROUND_NEEDS_APPROVAL` (no turn can show the card; use the
  main agent or a foreground subagent). Under full access a safety check is declined and recorded as a
  safety block on the origin turn (`thread/safetyCheck/blocked`); an owner's one-time grant lets the
  same call through once, as in a turn.
- Transcript. The run (`subagent_runs.background = 1`) stays open after its turn ends; its stream
  messages (with `parent_tool_use_id`) keep arriving with no adapter turn active and go to the
  transcript only. It closes on the task's terminal status (task_updated / task_notification, or
  SubagentStop): completed, failed, or interrupted for stopped / lost.
- Notice. A terminal background `local_agent` task adds `notifyParent` (`noticeId`, `taskId`,
  `taskType`, `status` completed | failed | stopped | lost, `description`, `summary` = its final reply
  up to 4000 characters with `summaryTruncated`, `originTurnId`, `toolUseId`, `transcript`, `reason`)
  to that `claude/task/updated`. It waits for the task_notification (which carries the summary) up to
  3 s after a task_updated / SubagentStop, then goes out without a summary; at most once per task.
  A main-agent background shell command (`local_bash`, `run_in_background`) gets the same, at most
  one per 60 s per thread; later ones are coalesced into one notice (`noticeId` `shells:<first>`,
  `tasks`). Commands a subagent runs never notify. cutex turns each notice into one Agent Bus message
  to this agent (source "Claude Task", deduplicated by `noticeId`): read at the next step of a running
  turn, or as a new visible turn.
- Lost. A Claude Code restart (supervised reconnect, model / effort change) or runtime close ends
  running background tasks with the process: status `lost`, notice at once, and kept for the next
  `thread/resume` too (an observer that was reconnecting). Runs an earlier adapter
  process left open are closed at start and their notices kept in `Store.recovered_events`, replayed
  on the next `thread/resume` (cutex's observer resumes on connect) until the thread's next turn.
  Background shell commands lost by an adapter restart are not reported (only subagents are stored).
- Stop. `claude/tasks/stop` (task id = agent id) stops a running one (SDK `stop_task`); it ends
  `stopped`. Interrupting the parent turn does not stop background tasks, as in Claude Code.
- Usage (G9). A background run's messages carry model usage; it is summed per API message id (Claude
  Code repeats a message's usage on each content block) and added to the origin turn as a
  `modelRequests` entry of kind `background-task` when the run ends, then `thread/tokenUsage/updated`
  (with the thread's last context occupancy) carries the larger `total`, which cutex's usage ledger
  counts once. An origin turn still running takes it with its Result. Not counted: Claude Code's
  notification turn (its Result is not attributed to any turn) and a run that never ends.

Live check: `scripts/smoke_background_subagent.py --model haiku` (Claude Code 2.1.301, SDK 0.2.166,
Linux, 2026-10-21) passed: the subagent read, slept and wrote for 26.5 s after its turn ended, a second
turn of the main agent completed meanwhile, `notifyParent` arrived completed with its reply, its
transcript closed and the parent timeline kept none of its tools.

## Claude Code's notification turn (claude-r23)

What it is. When a background task ends, Claude Code emits task_updated / task_notification and then
starts a turn of its own (`system/init`, an assistant reply, a Result; about 2-3 s in the probe, also
after a stop). cutex never submitted it: no adapter input, turn id, receipt or interrupt belongs to it.

Why its tools are denied. No cutex turn owns it, so there is nothing to attach a tool call to: no
timeline the desktop shows, no approval card (approvals need a turn id), no input receipt, no queue
or steer ownership (a note or busy input claimed there would be read by nobody), no interrupt. The
hook denies every main-agent tool with `NO_TURN_TOOLS`.

What the model is told. `RUNTIME_LIMITS` in the system prompt append: background subagents are
allowed and preferred for long work; a finished one (or background shell command) arrives as a
visible message from "Claude Task", act on it then; Claude Code's own turn for the notification cannot
use tools and is not seen, reply in one line and use no tools. A tool call there is denied with: "No
active turn accepts tools. This is Claude Code's own turn for a background task notification: the
user does not see it and it cannot use tools. Reply in one line and end the turn; cutex sends you the
result as a visible message, act on it there."

If the model ignores it. Each tool call is denied with that message (no effect runs); its text stays
out of every timeline and is never a reply. A new input waits for the turn (G5 above) up to 120 s,
then is submitted anyway.

Cost. One small invisible model call per finished background task (also per stop), on the same
account. Its usage is not attributed to any turn.

Alternatives rejected.
- Adopting it as a visible turn: the adapter would have to own a turn Claude Code started (input,
  receipts, interrupt, usage), and it races cutex's next input; that undoes the r18 decision.
- `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1`: per the probe it turns background work off entirely
  (`run_in_background` subagents become foreground, Bash rejects the argument), not only this turn.
- Answering it with tools allowed: tool effects without owner, receipts or approvals.

Order and duplicates with the cutex message. Claude Code emits the notification, then its own turn;
the adapter emits `notifyParent` with the notification, cutex queues the "Claude Task" message and
submits it. An idle parent: the message's turn waits for the notification turn's Result (G5: the
notification opened the start window), so it runs after it, with only its own reply. A busy parent:
the message is read at the running turn's next main-agent tool step (`soon`), or runs as the next
turn, which waits for a notification turn that follows the running one (`notified_in_turn`). The
model sees the result twice (in the notification turn and in the cutex message); only the cutex
message is a visible turn where it can act. cutex sends one message per `noticeId`, also when the
adapter repeats the notice (a resume replay).

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
