# Ephemeral Codex Side conversations

Capability: `side-chat` (`features.sideChat`). Side is an owner-only adapter of
cute-codex's native `/side`, not another agent or a saved Cutex session. Offline
parents are rejected; opening Side never starts a parent runtime. Claude agents
have Side when their adapter declares `features.sideChat` (claude-r21; see
"Claude agents" below); older Claude runtimes are rejected.

## HTTP API

All routes are below `/v2/sessions/{sessionId}/side-chats`. They require the
Management root credential, including an authenticated client/device token
mapped to root by the existing gateway. Agent/bridge credentials are rejected.

| Method and suffix | Input | Result |
| --- | --- | --- |
| `GET` | — | `{ chats: SideChat[] }` |
| `POST` | `{ anchorTurnId?, lastTurnId?, model? }` | `{ chat }` |
| `GET /{chatId}` | `?afterRevision=N&waitMs=N` | `{ chat }` |
| `POST /{chatId}/messages` | `{ text, clientMessageId }` | `{ chat, turnId }` |
| `POST /{chatId}/interrupt` | `{}` | `{ chat }` |
| `POST /{chatId}/responses` | `{ requestId, result }` **or** `{ requestId, error: { code, message } }` | `{ chat }` |
| `DELETE /{chatId}` | — | `{ closed: true }` |

`model` must be a nonblank string of at most 200 UTF-8 bytes without control
characters. It is passed unchanged to native `thread/fork`; the profile/provider
cannot change. Omitting it inherits the parent's current model. Native model
refusals preserve their message and include `{code,message,data}` in
`error.details.nativeError`; Cutex does not pretend to validate a profile's model
catalog locally.

`text` contains 1–65536 UTF-8 bytes and must not be blank. `clientMessageId` is
1–128 bytes. Retries reuse this id: equal text returns the original turn id,
changed text conflicts. A timed-out delivery is never automatically resubmitted.
This deduplication is memory-only, not the durable request-idempotency database.

`waitMs` is capped at 25000. If `afterRevision` equals the current revision, GET
waits for a change or timeout; it always returns a full bounded snapshot, not a
delta page. A snapshot includes:

```text
id, sessionId, threadId, runtimeGeneration,
anchorTurnId, lastTurnId, forkedThroughTurnId,
status: opening | ready | running | stale | closed,
createdAt, updatedAt, revision, activeTurnId,
model, effort, truncated,
events: [{ seq, method, params }],
pendingRequests: [{ id, method, params }]
```

The three boundary fields have different meanings:

- `anchorTurnId`: the UI's requested annotation origin; by itself it does not
  truncate context.
- `lastTurnId`: the caller's optional inclusive history cutoff, or null.
- `forkedThroughTurnId`: the resolved last terminal parent turn actually selected
  for the fork, or null if there is no completed history. This is the default
  summary-note anchor when no explicit origin was selected.

Side asks the parent connection for paged `thread/turns/list` summaries and uses
the same boundary calculation as ordinary session forks. A requested cutoff must
exist and be terminal. Without one, the latest completed/failed/interrupted turn
is selected; the live active turn is excluded. The resolved boundary is passed
to native `thread/fork` as `lastTurnId` (or `beforeTurnId` when the only history is
running), so a subsequent parent turn cannot silently extend the chosen context.
An empty history uses the native empty/latest fork path and rechecks the parent
before boundary injection (native ephemeral children cannot list history). Any
new parent turn conservatively closes the child and returns
`409 side_boundary_changed` for a retry, including a turn that arrived just after
the fork. No transcript is assembled or copied into a Cutex event store.

There is one ongoing Side per parent runtime connection, at most 32 in memory.
Opening the same source reuses it. Opening another source/model returns
`409 side_chat_already_open` with
`error.details = { chatId, anchorTurnId }`; a desktop can attach directly to that
chat. It must explicitly close it before starting at another origin.

## Native behavior and isolation

The adapter sends native `thread/fork` with `ephemeral: true`,
`excludeTurns: true`, the parent's model/provider/effort/workspace configuration,
and its effective developer instructions followed by the verbatim
`SIDE_DEVELOPER_INSTRUCTIONS` from cute-codex `tui/src/app/side.rs`. It then calls
`thread/inject_items` with the verbatim `SIDE_BOUNDARY_PROMPT` before accepting
any user turn. Permissions inherit via omitted `approvalPolicy` / `sandbox` /
`permissions`; tools are not artificially disabled. The original boundary says
inherited instructions are reference-only and subagents are off-limits.

Side turns and approvals are held in the Side registry. They never become main
session events, `/v2/approvals`, queue input, a sidebar agent, bus identity, usage
projection, or persisted request-idempotency data. Inherited `thread/started`
history and the injected boundary are not displayed as Side messages.

Native startup notifications can precede the fork response. Their diagnostic
classification is deferred in bounded memory until the exact RPC response gives
the owned thread id. A broadcast `thread/started`, even with the same parent and
`ephemeral: true`, is not proof of ownership and never authorizes cleanup.
Unrelated diagnostics are preserved after classification. On transport loss,
still-unclassifiable records produce count/byte-only omission evidence, not raw
text. Filters and tombstones use the event worker's exact connection identity as
well as generation, so an old worker cannot affect a same-generation reconnect.

Limits: 4096 events / 8 MiB per chat, 128 pending requests / 4 MiB, early-event
buffer 128 / 2 MiB, diagnostic deferral 512 / 8 MiB. Event loss is exposed as
`truncated: true`. Pending-request overflow is explicitly declined, not silently
dropped. Clients must not reconstruct a purported complete transcript after
truncation.

## Lifecycle and errors

Unmounting, hiding or moving a desktop component does not close Side. Closing
its tab must call DELETE. Explicit close interrupts the child and unsubscribes
it; it never interrupts the parent. Failed cleanup remains retryable. A timed-out
fork keeps an unresolved registration until its exact late RPC response arrives;
that response schedules cleanup off the native actor. Parent connection loss
marks Side stale, and a daemon restart loses all Side state. Notes are separate
and survive; only an explicit owner action creates a note from Side output.

Important error codes:

- `side_runtime_unsupported` (409): a Claude parent whose adapter does not
  declare `sideChat` (claude-r20 and older), or a Side thread as parent.
- `side_runtime_offline` (409): parent is not connected.
- `side_chat_already_open` (409): different requested origin/model; includes id.
- `side_fork_outcome_unknown` (409): fork result has not yet resolved.
- `side_chat_opening`, `side_chat_not_ready`, `side_runtime_changed` (409).
- `turn_not_found`, `turn_in_progress` (409): invalid explicit cutoff.
- `side_chat_not_found` (404): absent chat or wrong parent session.
- `side_native_error` (502): native rejection/transport failure.
- `side_chat_capacity` (429): close unused chats first.

## Claude agents

For a Claude parent the same routes, inputs and snapshots apply, but cutex holds
no Side state: it relays each call to the agent's running adapter
(`claude/side/open|list|read|send|interrupt|respond|close`, `threadId` set by
cutex to the agent's thread) and adds `sessionId` and `runtimeGeneration` to the
snapshots. The adapter answers refusals with `error.data.code`, mapped to the
codes above (`native_transcript_behind` becomes `409 side_boundary_changed`).
`GET` of the list answers `{chats: []}` while the adapter is stopped or lacks
the feature.

- Boundary: the same rule (an explicit `lastTurnId` must have ended; else the
  newest ended turn, a running one excluded). The adapter waits up to 3 s for
  the boundary turn's last observed message to reach Claude Code's transcript
  and refuses (retryable) when a just-started running turn cannot be separated.
- Fork: the official SDK `fork_session(up_to_message_id)` copies the parent's
  native transcript through the boundary into a new native session. It only
  reads the parent: no session lease, the parent's runtime is neither closed
  nor interrupted. With no ended turn the Side starts a fresh native session.
- Side thread: a hidden adapter thread (thread property `side`, never listed by
  `thread/list`) bound to the copy, with the parent's model (or `model`),
  effort, permission mode and instructions; plan mode, inputs, notes and grants
  are not copied. It runs turns like any thread, with its own Claude Code
  process. Claude Code keeps a resumed session's recorded system prompt, so the
  Side does not change it (this also reuses the parent's prompt cache):
  `SIDE_DEVELOPER_INSTRUCTIONS` and `SIDE_BOUNDARY_PROMPT` are two text blocks
  before the first Side message, not shown in the Side timeline.
- Events and approvals go to a private in-process peer of the adapter: never to
  the parent's subscribers, the management observer, `/v2/approvals` or the
  agent's inputs. Limits as above (4096 events / 8 MiB, 128 pending requests,
  32 chats per adapter, one ongoing Side per parent).
- Close interrupts the Side turn, stops its process and deletes the hidden
  thread and its native copy. Stopping the agent's runtime (and the adapter's
  shutdown) closes its Side chats first. An adapter restart loses all Side
  state (`404 side_chat_not_found`); a leftover hidden thread is deleted on the
  next Side request.

An older Cutex has no Side routes/capability. No persistent Side migration or
rollback is required. Native/model acceptance is separate from the automated
loopback fake-native tests; those tests do not use model credentials.
