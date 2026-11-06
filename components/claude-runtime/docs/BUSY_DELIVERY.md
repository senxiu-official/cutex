# Claude busy wake and delegation result delivery

The selected signed Claude component implements the external-input delivery contract described here. Host and runtime capabilities gate each path.

Before accepting a new Agent Bus wake, Cutex uses the shared `wake_timing` /
`wake_busy_wait()` policy. A busy recipient holds an unfrozen message until the
configured deadline. An idle recipient receives it immediately. After the
deadline, a subscription adapter advertising `externalInputDeliveries: soon`
accepts a frozen native `soon` envelope. Its digest never changes on retries.
Existing inbox identities and frozen envelopes keep their route. Adapters without the required capability retain after-turn delivery.

The adapter enqueues `soon` in the existing durable FIFO and adds a `busy_inputs`
record in the same transaction. A main-agent PostToolUse callback must match the
native session, current bridge binding and a PreToolUse-admitted tool id. Child,
unknown, revoked and repeated callbacks cannot claim a wake. Claiming atomically
removes the input from the FIFO and records its consuming turn. The queued
receipt's reserved `turnId` remains stable; the legacy result receipt exposes
`contextTurnId` separately. Source, framing, asks, attempt and structured view
remain external data. No owner `turn/start` or `turn/steer` route is used.
The claim and hook return run without yielding; a turn end that wins before the
claim leaves the input untouched for its FIFO fallback. The
timeline item of an injected wake is published right after the claiming hook
returns (scheduled, not awaited), like an owner steer; earlier adapters waited for
the context acknowledgement. The item is not a context receipt.

A hook offer and ordinary item output do **not** count as context persistence.
In the current adapter, a newly appended, complete native main-chain context attachment can
confirm its exact offered inputs before the turn ends. The existing successful
SDK Result remains a fallback acknowledgement boundary.
Native inputs then receive their ordinary stable context receipt; legacy result
status sets `contextPersisted`. Unconfirmed interrupted, crashed or unreturned offers stay
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
An early receipt also feeds `cutex-wake-counterfactual`. For legacy delivery, the read log
timestamp reflects confirmed persistence, which can be later than the hook
offer. Timing observations affect diagnostics only, never delivery decisions.

The SQLite table is an additive schema-2 migration. Keep terminal ordering: close tool-context admission before awaiting terminal receipt publication. Release compatibility and recovery follow the signed backend migration metadata.

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

### Interrupt and unread steers

A steer the turn never read is dropped (input state `interrupted`) when the owner interrupts: since
the adapter at the `turn/interrupt` request, before the abort reaches Claude Code (earlier adapters only
when the turn ended `interrupted` / `failed`). The desktop's interrupt & send reads the stopped turn
and resends every steer it did not record (a new client id, merged with the composer text), so the
adapter must not deliver it as well: in earlier adapters the stale `soon` row was injected into the
resent turn at its next PostToolUse, or ran as one more turn after it (observed with earlier adapters). A
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
after its first tool call, and a turn without tool calls never saw them (observed in earlier turn-start delivery). Codex consumes passive ExternalInput at a turn's first input boundary, so both runtimes
now behave alike. Opt-in check against the bundled CLI and a loopback model:
`test_real_cli_owner_wake_reads_notes_in_its_first_request` (CLI 2.1.299, SDK 0.2.166, Linux): the
note is in the first and only request, the owner's `@path` expands, the note's does not.

## Autonomous turns

Claude Code starts a turn by itself when a background task finishes: a `run_in_background` Bash
command or a background subagent (its task notification). That turn has no adapter
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
close or a supervised reconnect fails held inputs as never submitted. A background task's
notification that arrives while an adapter turn runs opens the 10 s start window at that
turn's Result (`RuntimeOwner.notified_in_turn`): Claude Code may start its own turn right after that
Result, before any frame of it reaches the adapter, so the next input waits for it the same way
(`heldByNotification`).

Two kinds of in-turn notification no longer arm it:

- A foreground task's. Claude Code 2.1.314 registers a foreground Bash command that runs 2 s or more
  as a `local_bash` task (`task_started` with `is_backgrounded: false`) and sends `task_notification`
  when it ends in the foreground; its result goes back to the model inside the running turn, so no turn
  follows. A task counts as foreground only when its `task_started` said `is_backgrounded: false`, no
  `task_updated` patch moved it to the background (`is_backgrounded: true`) and the adapter did not
  start it as a background command or subagent (`RuntimeTasks.background_notice`). A notification of
  a task the adapter never saw start counts as background.
- A background task's that Claude Code folded into the running turn. Claude Code drains queued
  notifications into the model request after a tool round. A main-chain user frame with tool results
  after the notification (`RuntimeOwner.notice_drained`), then a main-chain model frame (`stream_event`
  or `assistant`) of that next request clears `notified_in_turn`. A model frame alone does not: it may
  belong to the request that was already streaming when the notification came, and if that request is
  the turn's last, Claude Code starts its own turn after the Result. A replayed user message (no tool
  result) is no tool round either.

A background notification after the turn's last tool round keeps the 10 s window. If Claude Code
folded such a notification into the running turn anyway, the next input only waits out the window.

## Background subagents

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

## Claude Code's notification turn

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
  receipts, interrupt, usage), and it races cutex's next input; that would violate the managed-turn boundary.
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

## Validation

Release acceptance is recorded by the GitHub Actions build and upgrade gates. Historical private probe counts are not a release result. Use an isolated marked home for adapter tests and report the exact public candidate and SDK/CLI versions. Hook offers, native context proof and interrupted uncertain outcomes need separate assertions.

## Owner insert progress

Adapter 0.8.7 emits durable `claude/input/stageChanged` notifications with
`{threadId, turnId, messageId, clientId, stage}`. Stages are monotonic for an
insert: `registered`, `offered`, `contextPersisted`. Only owner inserts with a
client ID receive these events. Registration and hook offers are committed
synchronously with the inbox transaction; live publication follows the same
per-thread order. A random publication token committed with each journal event
prevents an abandoned transaction from publishing a later retry that reused its
SQLite sequence number. Existing `claude/input/contextPersisted` events remain
and additionally carry the client ID (null for inputs without one).

Stages can skip: `offered` specifically proves the authenticated tool-gap
context return. If the current turn finishes before a tool gap, the queued
insert runs as the next ordinary turn and advances directly from `registered`
to `contextPersisted`, carrying the actual consuming turn ID. A failed or
interrupted turn never manufactures a successful context receipt. Reconnects
can replay the durable stage journal without reoffering input.

While a terminal SDK callback is closing input admission, `turn/steer` returns
RPC error -32009 with `data.reason = "turnFinishing"` before parsing/enqueueing
input. Cutex is responsible for retaining that message for the next turn and
must not call it steered. A foreground turn held behind an autonomous native
notification publishes `heldByNotification` in active flags; thread reads use
the same projection, and result/quiet-window/interrupt/close release the flag.

Terminal settlement, `turn/completed`, idle and input status now precede the
observational context-metric refresh. That refresh stays serialized with the
worker: the next input may be accepted while it waits, but the next native query
still waits for the read. Metric results are bound to the captured owner, native
connection generation and turn, and are discarded if that binding changes.
Existing sessions retain their runtime pins; a changed payload requires a new component identity.

## Prompt native context receipts

Earlier adapters exposed progress but still called busy-input acknowledgement only from the
successful SDK Result callback. Native PostToolUse context had already been
written to the session transcript, so contextPersisted could lag offered by the
rest of a long turn. SDK callback hooks do not emit hook_response events in the
pinned CLI, and a tool-result UserMessage precedes the context attachment;
neither callback return nor that UserMessage is a persistence receipt.

The current adapter watches only sessions with pending authenticated offers. It captures the
current native owner, connection generation, bridge binding, exact claimed
message IDs, hook/tool identity, full returned context and transcript EOF before
the hook returns. A bounded incremental reader requires a complete new main-
chain attachment for that session, matching PostToolUse hookName/toolUseID,
exact returned context, and a parent native user/tool-result record for the same
call. It then emits the existing per-message contextPersisted event and stage.
No public event shape changes. Persistence here means a complete native record
is readable; it does not claim a filesystem fsync or successful completion of
the agent task.

One watcher per pending thread polls at 50 ms and stops when no offers remain.
Reads are limited to 256 KiB chunks and 16 MiB lines; a terminal flush drains
its captured EOF up to 32 MiB. Ownership changes, unavailable/replaced/truncated
files, incomplete or oversized evidence and the five-minute observation bound
never manufacture a receipt. Inputs are retained; successful terminal fallback
remains available. Admission closes before terminal/recovery/close awaits,
while the old binding remains available to verify already-written proof.

A receipt confirms only its named message, never every other offer in the same
turn. Successful Results separately acknowledge each consumed passive note,
preserving their prior terminal behavior. A later failed/interrupted turn does
not erase a context receipt already backed by native proof; an unconfirmed
offer remains unconfirmed. Existing runtime pins stay unchanged; this requires
a new immutable component identity, never a replacement for an existing archive.

### Busy `soon` items at injection

Earlier adapters published the `external_event` item of a busy `soon` external input (wake,
approval notice, `soon` delegation result) only in `acknowledge`, at the
successful terminal Result, while an owner steer's `userMessage` was published
when its hook claimed it. The current adapter publishes every claimed item the same way: one
task per claim, scheduled after the claim and before the hook returns, never
awaited by the hook, chained per thread so items keep claim order.
`acknowledge_messages` no longer emits items; before it emits a
`contextPersisted` it waits for the thread's pending item publication, so a
receipt never precedes its item. Item ids, shapes and turn ids are unchanged
(`functionCallOutput` / `external_event`, id = message id, the consuming turn).
cutex can move the record to its timeline at the item (state `accepted`,
`turnId`), while `inContext` still comes only from the receipt. An offer whose
turn then fails or is interrupted keeps its already published item; its input
stays unconfirmed, as before.


## 057 owner carries (claude-rNEXT)

Optional canonical `ownerCarries` on owner start/steer reserves the complete
batch with the owner in one transaction. Reserved notes cannot be claimed by
ordinary passive-note delivery. A busy owner and its carries enter the same
authenticated hook; FIFO fallback keeps the group together. Owner text remains
the final SDK text block for @path expansion. Native identities/views and exact
context receipts survive; interruption never converts unknown carries into
pending passive notes. See [OWNER_CARRIES.md](OWNER_CARRIES.md) for the wire,
idempotency, limits, receipt boundaries and integration rules.
