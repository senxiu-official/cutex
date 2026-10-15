# User input delivery and retries (`cutex/userInput/submit`)

Two independent dedupe layers protect a user input from reaching the native thread twice.

## 1. `requestId` (Management v2 request idempotency)

`src/management/v2/request_idempotency.rs`. Every mutating request is reserved in one SQLite
`BEGIN IMMEDIATE` transaction before it runs (`forwarded` row = in flight). The same `requestId`
sent again (concurrently or later):

| stored row | answer |
|---|---|
| `forwarded`, this process | `202 request_in_progress` (retryable, `retryAfterMs: 250`) |
| `forwarded` by a dead process, or `outcome_unknown` | `409 request_outcome_unknown` (`resyncRequired`) |
| `completed` | the stored response is replayed |
| other body | `409 idempotency_conflict` |

Capacity (default 20,000 rows): only `completed` rows are evicted to make room. When the store is
full of unresolved rows, outcome-unknown rows (and rows of dead processes) older than 1 h are evicted
with a warning; otherwise the new request is refused with a retryable
`503 idempotency_capacity_exceeded` (`details.retryAfterMs`, counts of unresolved / in-flight rows).
So a burst of unresolved requests refuses new ones for at most 1 h, never for the 7-day TTL.
Test: `concurrent_same_request_id_is_forwarded_once`.

## 2. `clientUserMessageId` (the input itself)

A client retry normally uses a **new** `requestId` with the same `clientUserMessageId`, so layer 1
does not help. Cutex used to dedupe only by scanning the native history for a `userMessage` with that
`clientId`. Right after native accepts `turn/start` / `turn/steer`, `thread/read` can already list the
turn but not yet the `userMessage`. A retry in that window was sent again. Concurrent retries
returned `started, steered, steered` and left three copies of the input in native (Kutex C-010).

Each `clientUserMessageId` identity (`runtime/management-v2/user-input-state.json`, per session) now
records its native **delivery**. Every native user-input request (submit and queue flush) goes
through `send_recorded`:

1. **Reserve.** Compare-and-set `none → sending {threadId, since}` under the user-input store lock,
   before the request leaves. If something is already recorded, nothing is sent.
2. **Send.**
   - Accepted → `accepted {threadId, turnId, acceptedAt}`.
   - Transport failure or timeout → `unknown {threadId, since}`.
   - Definite refusal → the record is cleared, so a retry may send again.
   - Native accepted but the record could not be written → the request fails with
     `outcome_unknown` (stage `post_submit`, not retryable). The `sending` record stays.

A retry (same id, same input and origin) whose input is not visible in the history resolves the
record against the thread it just read:

| record | thread | result |
|---|---|---|
| `accepted`, turn still listed (or unverifiable) | — | `deduplicated` with that `turnId`, nothing sent |
| `accepted`, turn gone (history revert) | — | record cleared, sent again |
| any, recorded for another native thread | — | record cleared, sent again |
| `sending` / `unknown` | a turn is running, or the record is < 30 s old | `202 user_input_in_progress`, nothing sent |
| `sending` / `unknown` | idle, without the input, record ≥ 30 s old | record cleared, sent again (warning logged) |

An accepted input always appears in an idle thread's history unless it was reverted. That is why
"idle and absent after the grace period" counts as proof that the earlier send did not land.

`user_input_in_progress`: HTTP 202, `retryable: true`,
`details {clientUserMessageId, delivery: "sending"|"unknown", retryAfterMs: 2000}`. It is **not**
recorded for `requestId` replay (the request reservation is released), so a retry with the same
`requestId` runs again.

**Queue.**
- A flush that finds the front item already `accepted` (for example, native took it but the event
  append failed) removes the item without sending it again.
- An unresolved front item stops the flush; the next turn end or idle event decides.
- Editing a queue item whose delivery is recorded fails with `409 queue_input_already_submitted`
  (not retryable, `resyncRequired`). Remove it and resubmit with a new `clientUserMessageId`.

**Unchanged.**
- The same id with different input or origin is still `409 idempotency_conflict`.
- A fork is another session with its own identities.
- Identities have no capacity limit or eviction, so the delivery record cannot be "full". Unresolved
  records block only their own id, and each resolves on the next retry or flush.

**Limits.**
- If cutex crashes after native accepted the input but before `accepted` is written, the record
  stays `sending`. A retry then waits until the history shows the input or proves it absent.
- A raw native `turn/start` sent through `/app-server/requests` bypasses this layer and is deduped
  by `requestId` only.

Tests (`src/cli_app/app_server_user_input.rs`):
- `concurrent_retries_of_one_client_message_reach_native_once`
- `accepted_delivery_dedupes_late_visible_retries_across_reopen`
- `accepted_delivery_keeps_payload_and_origin_conflicts`
- `accepted_delivery_does_not_survive_revert_other_thread_or_fork`
- `unknown_delivery_blocks_resend_until_the_history_settles_it`
- `in_flight_reservation_admits_one_sender_and_refusal_releases_it`
- `unrecorded_acceptance_is_outcome_unknown`
- `accepted_queue_flush_is_not_resent_after_an_event_failure`

Server (`src/management/v2/server.rs`): `user_input_delivery_errors_have_client_statuses`.

## 3. Shared admission order (`admission-order`)

`queue/list` retains owner `items`, `revision`, `pause` and `asksPaused`, adding
`admission: {revision, orderRevision, items, nextStep, currentTurn}`. Bootstrap's `cutexState.admission`
has the same shape. `items` contains only owner inputs and queued local-target asks;
its one-based positions number exclusive turns. Review-pending asks are excluded.
Approval inserts an ask using its original creation time. Held and paused rows retain
keys and display positions, but admission skips them. `targetBusy` is recomputed from
runtime and order facts; it must not become a permanent barrier. Woken continuations
keep the §2.6 priority over ordinary queued work; their live keys and displayed positions
are placed ahead of ordinary rows, rather than allocated a new tail position.

| `nextStep.deliveryKind` | Waiting source |
|---|---|
| answer, question | Non-passive ask results not claimed by a wait |
| bus | Wake-worthy delegation state changes, non-awaited job completions, agent start messages |
| alarm | Due alarms already queued as canonical system bus messages |
| tell | Non-passive canonical agent messages (tell/send use the same transport) |

Ask-store ridealongs (answer, question and delegation-status `bus`) also carry `askId`, the
delegation they belong to, so a client can show and open it; other sources omit it. `label` is
unchanged (the ask's title, else an English fallback such as `Answer`), so older clients keep working.

These ridealongs have no numbered position and cannot be moved. Idle delivery precedes
numbered admission, except while an owner stop holds them (below). After a ridealong-started turn finishes, waiting owner work gets an
opportunity before further ridealongs (while preserving numbered asks before that owner).
The owner may waive that once ("let the delegation results go first this time", owner decision
2026-10-12): `cutex/userInput/queue/yield` (params `{}`, idempotent, capability
`admission-owner-yield`) drops a pending owner opportunity, restarts the 30-second liveness wait
below, and arms a one-shot yield shown as `admission.ownerYielded: true` (omitted otherwise). The
next completion of a tracked turn of any kind consumes it; if that turn was ridealong-started it
grants no new owner opportunity, so waiting ridealongs keep their idle head start; the turn after
it grants one again as usual. An owner turn start also clears it. Result: `{"admission": …}`
(the re-listed snapshot); the change also publishes `cutex/userInput/queueReordered`. The yield is
in memory only, like the owner opportunity itself: a daemon restart drops both.
If ridealongs have blocked waiting owner input for 30 seconds while idle, admission
retries owner flush and lets remaining ridealongs join according to their delivery timing.
The timeout does not override a native turn or an admission lease. Held bus records are skipped. Busy wakes wait `agentBus.wakeBusyWaitSecs`: `afterTurn`
with `nextStepAt` while waiting, `nextStep` after the deadline. Zero is immediate;
negative/null waits after the turn indefinitely. Alarms always use `afterTurn` without
a deadline. A frozen native envelope reports its actual frozen timing.

Delegation results (answer / question / delegation status) of a busy requester follow the same
busy wait, counted from its **oldest** pending result: all pending results go together, so every
row shows that one `nextStepAt`. This holds also while the requester's running turn holds its
admission lease, i.e. the turn was started by an earlier results event or is a delegation it
handles (owner observation 2026-10-12, an owner requester agent: results that arrived 4 s into a results turn
waited for the whole turn). The next-step event does not take that lease: it is recorded beside it
in the ask store (`nextStepLeases`, one per session, same record shape as a lease, holder `event`,
no turn) and reconciled like a next-step lease (read → acknowledged and the record dropped; absent
or a volatile runtime restart → pending again; unknown past the grace → passive plus
`cutex/ask/deliveryFailed`). The running turn's lease, its release at turn end, owner priority,
restart restoration and fork/revert refusal are unchanged. If that lease is released (turn end,
interrupt, yield) before the runtime read the results, the record takes the freed admission slot
at once, exactly as a next-step event sent without a lease holds it until read. A lease not yet
bound to a turn (a submit in flight, an unread next-step event) still makes later results wait,
as do pauses, stop holds and `delegation_wait`. Rows of results already sent at the next step show
`nextStep` with `nextStepAt` = the time they were sent. Whether the runtime takes next-step input
at all (Codex `soon`, Claude durable next-step context) is not in the snapshot; a runtime without
it reads the results after the turn.

**Owner stop holds everything** (owner decision 2026-10-12, "停止 = everything waits").
`cutex/turn/stop` pauses the owner queue (`pause.reason: "interrupted"`, only when items are
queued) and, before the interrupt and even with an empty queue, records a durable stop hold in
the ask store: an `asksPaused` pause with `origin: "ownerStop"` and `stopHold: true` (a stop on an
existing explicit `ownerPause` keeps that origin and sets `stopHold`). While it exists:

- no ridealong starts a turn by itself: ANSWER / QUESTION results, bus wakes, tells, alarms and
  agent start messages stay waiting (not lost, not reordered). The 30-second owner liveness bound
  does not release them; it only lets owner input go first, and owner input is never blocked by
  held ridealongs;
- no ask is admitted to the session (queued asks show `paused`, hold `paused(owner)`), auto-wake
  for it is off, and its `delegation_wait` calls end;
- the native external-input gate the interrupt closed stays closed, so input native already
  accepted waits as well. Every release reopens it.

Released by `cutex/userInput/queue/resume` (before its flush; then the usual rules apply, idle
ridealongs first), by `cutex/asks/resume` (the owner queue stays paused), or by the owner's next
message that starts or steers a turn: `cutex/userInput/submit` with disposition `started` /
`steered`, `cutex/userInput/queue/steer`, or an explicit `cutex/userInput/queue/flush` that sent
an item. The ridealongs then ride along with / follow that turn under the normal `nextStep` rules.
A message the owner only queues (disposition `queued`) releases nothing. A queue resume on an
explicit `ownerPause` ends only `stopHold`; the delegation pause stays. The hold survives a daemon
restart (ask store `pauses`). `cutex/userInput/queue/pause` (manual) does not create it.

Snapshot: `admission.ridealongsHeld: "ownerStop"` (omitted when not held) and on each held
`nextStep` item `held: "ownerStop"`. An explicit delegation pause marks ask results (answer,
question, delegation status) `held: "ownerPause"`; bus ridealongs are not held by it. Every change
publishes `cutex/asks/pauseChanged` (`reason`: `ownerStop` | `resumed` | `ownerMessage` | `paused`)
and `cutex/userInput/queueReordered`.

Future alarm timers, passive notes, waited/cancelled-job passive completions, removed
Task Service messages and protocol-only Control envelopes are not ridealong turn starts.
`includedNotes: [{label}]` on the next eligible ask describes passive tells already in the
native inbox that exclusive admission can drain; labels contain no body. `currentTurn`
is null while idle, otherwise owner/ask/delivery (with askId for an ask), based on runtime
turn observations and the admission lease. Native acceptance precedes the turn-start
observation, so that brief interval can still read null. New bus records remember whether
submission started from idle; reconnect restores delivery ownership only with that marker
and a matching native turn receipt. Older records cannot prove this distinction and fall
back to owner after daemon restart.

Each session's user-input store has an additive admission registry. Variable-length
base-65536 fraction keys compare lexicographically, allowing repeated owner insertion
between neighbours without renumbering asks. Initial ordering uses enqueue/create times
(ties: owners first, asks by sequence). Retired rows' keys are collected; only live numbered
rows remain. Existing owner moves survive reads and restarts. Source changes synchronize
only affected sessions. Ask writes never scan the bus store; bus candidates use a cached
per-session index, rebuilt on store version changes. Scheduler and bridge admission reads
project cached state without writes; offline scheduling reads only numbered order.
Bus polling checks file metadata before taking the store lock or refreshing the index.
Background polling runs only with live event subscribers, queued owner input or online
runtime presence. Direct list/bootstrap and source-change checks remain fresh without
subscribers. External replacements are compared through admission fields without materializing message
bodies; retry-only changes do not hydrate the full store. Actual use still validates it.
Explicit list initializes missing keys lazily.

Ask-source locks precede owner-store locks; bus reads take their lock last. Native submission
retains the per-session process mutex and durable receipts. The unified admission-change
hook coalesces changed sessions, wakes an out-of-lock worker, publishes
`cutex/userInput/queueReordered` with `admission.revision`, and checks idle owner flush.
It covers enqueue/remove/move, review/cancel, hold/pause changes, nextStep changes and
current-turn observations. Cached source reloads also signal changes made by other processes.

Move parameters are `{queueId, expectedRevision, before: {kind, id} | null}`;
`before.kind` accepts only owner/ask, and null means end. Old move parameters are rejected.
The CAS compares `expectedRevision` with `admission.orderRevision`, which changes only
when numbered membership or order changes. Hold, notes, timing and current-turn updates
change the full snapshot `revision` without invalidating a move. The result includes both
`revision` and `orderRevision`, also present in the returned `admission`.
Errors: stale revision = `revision_conflict`; missing owner = `queue_item_not_found`;
already sent = `queue_input_already_submitted`; missing before = `admission_item_not_found`;
prohibited passing = `passing_not_allowed`. `/v2/settings` exposes `queue.ownerMayPassAsks`
and `queueDefault.ownerMayPassAsks` (true). False prohibits moving an owner ahead of an ask
that preceded it; backward movement and adjacent owner movement remain allowed.

An idle Auto submission skips paused owner items and starts immediately when no eligible
predecessor remains. Waiting ridealongs defer an ask without making an idle target busy
for `ifBusy: fail`. Explicit queue steer/flush retain their owner-override semantics. Moving during a turn
only affects future admission. Idle result coalescing is independent of numbered order;
`fit_returns` retains prefix deferral: once one result is deferred for size, all subsequent
results are deferred as well, avoiding overtaking within the result batch.

**Rollback to r228:** no conversion is required. r228's serde readers ignore additive owner
`admission`, bus `queuedAt`/`admissionStartedIdle` and config `queue` fields; an old-version save drops them. The
owner-only queue vector survives, but old timestamp comparison and independent bus admission
resume, losing the shared ordering and passing policy. Re-upgrading lazily reconstructs
missing keys. Keep a pre-rollback state backup to restore the exact interleaving. A stop hold reads in older
binaries as a plain delegation pause (`stopHold` is ignored and dropped on save): asks and their
results stay held until `cutex/asks/resume` or the owner's next message, bus ridealongs are no
longer held, and the old binary's own stops never create one. Before `admission-owner-yield`, `cutex/userInput/queue/yield` is
an unknown method and `ownerYielded` is ignored by readers and dropped from a stored snapshot on save
(it is recomputed from memory anyway, so nothing is lost).

**Rollback to before `nextStepLeases`** (r230 and earlier): the field is additive and absent when
empty; an older binary ignores it and drops it on its first ask-store save. A results event still
in flight beside a running turn's lease at that moment stays in `events` unacknowledged with its
deliveries `queuedAsEvent`: the older binary neither re-sends nor acknowledges it, and
`delegation_wait` cannot claim it (the agent may well have read it already). Before stopping for a
rollback, check that `nextStepLeases` in `~/.cutex/runtime/asks/asks-v1.json` is absent (it normally
clears at the agent's next tool step); otherwise set those deliveries back to `pending` (drop
`eventId`) and remove the event record in the backup copy you restore. Results then wait for the
running lease's turn to end again (the old behaviour).

