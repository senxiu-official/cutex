# Owner notes

Notes are owner-private annotations, not native messages or agent memory. They
live in the independent `<cutex home>/notes/notes.sqlite3` database. Cutex does
not inject them into prompts, queue them, copy them on fork, or expose an agent
MCP tool. The desktop may explicitly quote a note into an unsent composer draft.

## HTTP API

All routes require Management root authority, including paired client/device
credentials. Agent credentials are rejected before dispatch. Capability:
`notes` in `featureList`, `notes: true` in the feature map.

| Request | Body / query | Success (HTTP 200) |
|---|---|---|
| `GET /v2/notes` | `scope=session|project|host`, `id`, `pinnedOnly`, `query`, `cursor`, `limit` | `{ notes: Note[], nextCursor: string|null }` |
| `GET /v2/notes/{id}` | No query parameters | `Note` |
| `POST /v2/notes` | `{ clientNonce, title, body, scope, pins?, anchor? }` | `Note` |
| `PATCH /v2/notes/{id}` | `{ expectedRevision, title?, body?, pins? }` | `Note` |
| `DELETE /v2/notes/{id}` | `{ expectedRevision }` | `{ deleted: true }` |

`Note`:

```json
{
  "id": "note-<uuid>",
  "title": "A short title",
  "body": "The saved annotation",
  "scope": { "kind": "session", "id": "cutex.session-id" },
  "pins": [{ "kind": "project", "id": "project-id" }],
  "anchor": { "sessionId": "cutex.session-id", "threadId": "native-thread", "turnId": "native-turn" },
  "anchorState": "ok",
  "anchorStateAt": null,
  "revision": 1,
  "createdAt": "2026-10-11T00:00:00.000000000Z",
  "updatedAt": "2026-10-11T00:00:00.000000000Z"
}
```

Scope is `{kind:"session",id}`, `{kind:"project",id}`, or `{kind:"host"}`.
Anchor may be null; `threadId` and `turnId` are optional. Scope and anchor are
immutable. No ordinal is stored: clients derive display numbers from history.

Pins add navigation destinations, not ownership or access permissions. Session
lists include session-source notes and notes explicitly pinned there. Project
lists include project-source notes, notes whose source sessions currently belong
to the project, and notes explicitly pinned to the project. Host lists include
all notes. Explicit project pins survive membership changes. Unpinning removes
only that marker; all views read the same note record. `pinnedOnly=true` excludes
natural membership matches. Absent list scope defaults to host; session/project
scope requires `id`, host scope forbids it.

Titles are trimmed, control-free and at most 200 Unicode characters (empty is
allowed). Bodies are nonblank, contain no NUL, and are at most 64,000 characters
/ 256,000 UTF-8 bytes. There are at most 64 distinct pins. Nonces are trimmed,
control-free, 1–128 characters. IDs are trimmed, control-free, at most 256 UTF-8
bytes. Search is literal substring matching (ASCII case-insensitive), at most
500 characters. Page size defaults to 50, range 1–200. Cursors are bounded and
bound to scope, search and pin filters; they may be reused with a different page
size. Listing orders by descending updated time and ID. There is no note-count
limit. Unknown and duplicate fields/query keys are rejected. Null is not PATCH
omission. Only anchor may explicitly be null in a create request.

The create nonce is atomically tied to normalized request content, including
sorted pins. An identical replay returns the current record, without a second
create event. A changed payload conflicts. Nonces survive note deletion so a
late retry cannot recreate deleted content. PATCH and DELETE use a positive,
JSON-safe `expectedRevision`; PATCH must include title, body or pins.

## Source lifecycle

- `ok`: new records and migrated records; `anchorStateAt` is null. This is not a
  guarantee that every native turn has been read and verified. Clients may still
  infer missing sources from loaded history.
- `reverted`: a confirmed history revert reported this anchor's session and turn
  in `removedTurnIds` (the selected turn and every newer turn). The stored anchor
  and content remain unchanged.
- `sessionDeleted`: the anchor's session was deleted. A session-scope note with
  no anchor is also marked when that source session is deleted. When an explicit
  anchor points elsewhere it takes precedence over source scope. Content remains
  readable by ID and in applicable lists until the owner deletes it.

An actual state transition increments revision, sets updatedAt and anchorStateAt
to the same UTC timestamp, and emits a change event. Repeated hooks do nothing.
Session deletion supersedes reverted; subsequent reverts cannot downgrade it.
Editing title/body/pins does not reset source status. Fork never copies notes;
retire and restore do not change them because offline history remains readable.

The hooks run only after confirmed main actions and are best effort: storage
errors are logged and do not undo a successful revert/deletion. There is no
automatic retrospective scan or cross-database transaction with native history;
notes created concurrently with or after a hook may still have `ok` status.
Before the first note exists the hooks do nothing: a revert or deletion never
creates `notes/` by itself.

## Events

After each committed create, update or deletion, `publish_host_event` publishes
`cutex/note/changed` on `cutex.host`. Source-state transitions use the same event.

```json
{
  "noteId": "note-<uuid>",
  "change": "created",
  "revision": 1,
  "scope": { "kind": "host" },
  "anchor": null
}
```

`change` is `created | updated | deleted | anchor`. No title or body is included;
clients invalidate their host-specific cache and read the record again. The
deletion event carries the last persisted revision of the deleted record.
Lifecycle hooks emit one `anchor` event **per changed note**, because the wire
format has a single noteId; no event is sent for an unaffected note or a repeated
hook. All SQL changes commit before publication. Host-event delivery is best
effort, not an atomic cross-database outbox: reconnecting clients must refetch.
No event is appended to the source agent's conversation stream.

## Errors

| HTTP | Code | Meaning |
|---|---|---|
| 400 | `invalid_request` | Invalid fields, bounds, query, cursor or note ID |
| 404 | `note_not_found` | Note does not exist |
| 405 | `method_not_allowed` | Unsupported HTTP verb |
| 409 | `note_revision_conflict` | Reload; `details.currentRevision` is supplied |
| 409 | `note_nonce_conflict` | Nonce already belongs to different input |
| 409 | `note_deleted` | Original create nonce refers to a deleted note |
| 503 | `notes_unavailable` | Storage unavailable; retryable |

## Storage and rollback

The notes directory, database and SQLite WAL/SHM sidecars are owner-private.
Transactions use SQLite WAL with FULL synchronous writes; compare-and-swap and
nonce uniqueness are safe across connections. Schema version 2 adds anchor state
columns to version 1 atomically and preserves existing records, pins, revisions
and nonces. Unknown future versions are refused, never overwritten.

Back up with SQLite's backup API (or stop writers), not by copying a live database
without its WAL. Notes are separate from the session-store backup and must be
included explicitly in backup plans. Production Cutex versions predating Notes
ignore `notes/` and reject the new HTTP routes; reverting the binary does not
delete notes. The experimental Notes-v1 branch refuses this v2 database: restore
a v1 backup to use that experimental branch. There is no destructive downgrade
migration.

Tests use marked private homes only; they do not connect native runtimes, invoke
a model, access live services or modify actual agent history.
