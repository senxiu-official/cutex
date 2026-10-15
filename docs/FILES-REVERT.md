# File-only turn revert

Management root callers may send `cutex/session/files/revert` to
`POST /v2/sessions/{id}/cutex/requests`. Discover it through
`session-files-revert` (`features.sessionFilesRevert`). Registry version stays 3.
History, model state, commands and subagent changes are not reverted.

Preview with a new `requestId` and `{turnId, dryRun: true, paths?}`. Execution
requires another request ID and `{turnId, dryRun: false, paths?,
expectedPlanDigest}` using the returned digest. Optional `expectedRevision`
uses the normal session revision check. Path selection uses original history
paths; either side of a move selects its whole connected rename chain.

The result includes `planDigest`, `applicable`, `files`, `warnings`, `agentNote`,
`actionId`, `applied`, `journalPath`, and `recovered`. File statuses are `ok`,
`alreadyReverted`, `conflict`, `missing`, `outsideWorkspace`, and `unsupported`.
Only `ok` and `alreadyReverted` permit execution, and at least one file must be
`ok`. The digest binds the canonical root, selected file identities, actions,
statuses and SHA-256 before/after images. A preview performs no journal or
workspace writes; the normal Management request idempotency receipt still applies.

Only completed `fileChange` items are used, in reverse item/change order.
The planner uses an in-memory filesystem, so repeated changes and renames in a
turn compose. Unified patches support headers, hunks, offset contexts, CRLF/LF
matching and explicit no-final-newline markers. Context-free patches require a
unique match. Ambiguous/incomplete patches fail closed. Writes preserve the
current dominant line ending and final newline unless the patch explicitly
records a final-newline change. Recreated files use their recorded content.

Paths are confined to the session cwd or its project's workspace root.
On Windows paths compare case-insensitively, but writes keep each existing
name's on-disk spelling and a recreated name's history spelling.
Relative history paths are always resolved against cwd; absolute paths may
refer to either permitted root, including a disjoint project workspace.
Absolute history paths are accepted inside that boundary. Parent traversal,
`.git`, alternate streams, links/junctions/reparse points at any component,
nonregular files, non-UTF-8 data, files/diffs over 8 MiB and notebooks are rejected.
No shell commands, Git checkout, stash or SDK file rewind are used.

The turn is read with task 03's complete reader
(`history::read_complete_session_turn` -> `read_complete_turn`): the turn
index locates it at any depth (no 500-turn limit), its items are paged with
`thread/items/list`, and the read fails closed (`turn_history_too_large`) on
the 16 MiB limit, a native oversize answer or any summary/truncation marker.
The bounded HTTP preview (`GET .../history/turns/{id}`) is never used.
Source choice matches the HTTP turn route: the live runtime while connected
(the offline reader only if it is unavailable), else the offline reader.
Newer turns touching the target paths are reported as `laterTurnsTouchedFiles`
warnings; only the newest 500 newer turns are scanned and their listed items
may be native-trimmed, so `laterTurnsNotFullyScanned {unscannedTurns,
partialTurns}` says when that advisory scan was incomplete.

## Execution and recovery

Execution serializes all file reverts in a daemon and all cooperating daemons
sharing the same Cutex home with a file lock. Under that lock it reloads the
session (a changed native thread binding -> `history_changed`, an active turn
-> `session_busy`), re-reads the complete turn, rebuilds the plan (the digest
covers the turn's changes and the files' current content), checks session
activity again, and rechecks every file before writing.
Session activity and hashing cannot exclude unrelated editors, shell commands,
other sessions or a non-cooperating process. This is a recoverable sequence of
file replacements, **not an operating-system atomic multi-file transaction**.
There remains a pathname/hash check-to-write race with external writers.

Each action has an owner-private directory
`<cutex home>/runtime/file-reverts/frv-<UUID>/`:

- `plan.json`: version, workspace, digest, paths, before/after hashes, backup
  indices, original Unix modes and planned parent directories; no file contents.
- `before/<n>`: original bytes, checked against the plan hash during recovery.
- `state.json`: `prepared`, `applying`, `applied`, `rolledBack`, or `rollbackFailed`.

Before-images and the plan are durable before `applying`. Each write uses a
same-directory temporary file and rename, preserving Unix modes. Deletes remove
the file directly. Failure restores touched files in reverse order and removes
empty created directories. Rollback checks every touched destination and backup
before any restoration; unknown hashes are preserved for manual recovery.

Before each execution, `applying` journals are recovered only when every target
hash equals its recorded before or after hash. Unknown content produces
`rollbackFailed`, touches no target, and blocks further execution until the
owner resolves the journal. Successful automatic recovery returns action IDs in
`recovered`. Applied/rolled-back journals are pruned after seven days; other
states are retained. Dry-runs do not perform recovery or cleanup.
Recovery runs before validating the new execution plan. A changed/conflicting
new plan can therefore be rejected after an older interrupted action was
restored; the new preview in error details includes those recovered action IDs.

Successful execution emits `cutex/session/filesReverted` with thread, turn,
action, affected paths/actions and timestamp. The optional English `agentNote`
is returned; Cutex does not automatically send it to the agent.

## Errors

| Code | HTTP | Meaning |
|---|---:|---|
| `cutex_method_denied` | 403 | Root credential required |
| `invalid_request` | 400 | Invalid params or path selection |
| `thread_unbound` | 409 | Session has no native thread |
| `turn_not_found` | 404 | The turn is not in the bound thread |
| `history_changed` | 409 | The native thread binding changed before execution |
| `session_not_found` | 404 | The session disappeared before execution |
| `turn_in_progress` / `session_busy` | 409 | Target turn running / execution while session active |
| `plan_changed` | 409 | Digest or write-time hash changed; `details` holds a fresh preview when available |
| `revert_conflict` | 409 | A selected file blocks execution; none are written |
| `nothing_to_revert` | 409 | No applicable completed changes |
| `turn_history_too_large` | 422 | Complete history cannot be established |
| `codex_app_server_unavailable` / `native_history_unavailable` | 503 / 502 | History transport/native failure |
| `revert_rolled_back` | 500 | Execution failed, all touched files restored; outcome known |
| `revert_partial` | 500 | Rollback/recovery cannot safely finish; outcome unknown, journal path provided |

For manual recovery, stop writers, inspect `plan.json` and the current hashes,
and restore verified before-images only after preserving unexpected current
content elsewhere. Keep the journal until resolution is recorded. Old Cutex
versions ignore the journal directory and reject the unknown method. Downgrades
must not be used to bypass an unresolved recovery journal.

## Private fixture acceptance

Debug builds support `CUTEX_TEST_FILES_REVERT_HISTORY=<json>`, containing
`{"turns":[...]}` in oldest-first order. It requires `CUTEX_TEST_PRIVATE_HOME`
with `.cutex-test-private-home`; both the fixture and session cwd must resolve
inside that private home. Release builds ignore the hook. Test credentials are
unnecessary. Task 08 uses ports 24770/24760, never the deployed daemon ports.
