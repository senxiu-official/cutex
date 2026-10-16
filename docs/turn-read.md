# Single-turn history and ordinals (C-T1 / C-T2)

Implementation for `gpt/turn-read`, based on Cutex `47cab2d8af3001fca1675e7f3541d72728d53172`.
No adapter, native runtime, storage migration, dependency, event or desktop changes.

## HTTP contract

`GET /v2/sessions/{id}/history/turns/{turnId}[?compact=1]` uses the same root-only
authorization, hidden/archived session lookup and live-to-offline transport fallback as `/history`.
The two path IDs are decoded separately; neither may contain a slash. Only `compact` is
accepted as a query parameter. The native result is `thread/read` with exactly one turn.
The envelope adds `turnId`, non-null `ordinal` (one-based), `turnCount` and `complete`.
It does not include `cutexState`.

Missing session: 404 `session_not_found`; missing/reverted turn: 404 `turn_not_found`,
with `cutexSessionId`, current `threadId`, and `turnId` in details. Unbound thread: 409;
native RPC failure: 502 `native_history_unavailable`; unavailable offline reader: 503.
Unknown/invalid queries use 400 `invalid_request`.

Items are read in ascending order through **all** `thread/items/list` pages. Preview
detail has a 512 KiB budget. If a complete turn cannot fit, the returned prefix carries
`itemsView: "summary"`, `truncated: true`, `truncationReason` and envelope `complete: false`.
An indivisible native item over its transport limit also produces an explicitly incomplete
preview. `compact=1` retains the existing per-tool 2048-byte string limit and sets envelope
`complete: false` whenever it actually truncates an item.

`/history.page` and paged bootstrap `historyPage` now include `firstOrdinal` and `turnCount`.
Both may be null. Empty pages have null `firstOrdinal`; known empty threads have count zero.
When ordinals are known, every returned ID is checked against the index, so `firstOrdinal + i`
is the ordinal of page turn `i`. A concurrent native edit that breaks this correspondence
returns unknown ordinals, never a guessed position.

`/history` pages add **no native calls** for ordinals (PERF-10):
the newest page (no cursor) is reconciled with the cached index exactly like the bootstrap page
(known when the page reaches the first turn or overlaps a cached index); an older page reports
`firstOrdinal` from the cached index and `turnCount: null` (only the newest page can confirm
the count). Only a single-turn read builds a cold index.

Capabilities, immediately after `history-pages`:

- `history-turn-read` / `historyTurnRead`
- `history-turn-ordinals` / `historyTurnOrdinals`

The existing `bootstrap.json` export includes both new page properties and `historyTurn`.

## Index and bootstrap work budget

The process-local cache keys session ID, native thread ID, source (`live`/`offline`) and
runtime generation. It contains at most 64 entries and 16 MiB of serialized metadata;
item bodies are never cached. Each index is capped at 100,000 turns and 16 MiB as well.
Rust allocation/hash-table overhead is additional to the serialized metadata budget.

Cold scans request 100 `notLoaded` turns per native call (Codex caps at 100; Claude at
200). Warm reads request the newest 16 turns, then append, truncate after revert, or
rebuild when there is no consistent overlap. Duplicate IDs, missing IDs, empty pages
with a next cursor, repeated/cyclic cursors, and capacity overruns fail closed.
No native I/O runs under the global cache mutex. An invalidated cache token cannot be
repopulated by an older in-flight read; unrelated sessions keep their own tokens.

Revert/fork invalidate both the index and warm offline reader after the operation,
including an error with an uncertain mutation outcome. Existing receipts and warnings
pass through unchanged. New thread bindings and runtime generations discard old bindings.
Forked threads receive their own prefix and numbering. Compaction does not change the
turn IDs in either inspected native implementation; no adapter logic was changed.

Bootstrap adds **zero native calls** for indexing. If a cold thread does not fit in the
requested newest page, it returns null ordinals. The threshold is therefore `turnLimit`
(1..200), and the effective native page size can be smaller (Codex caps at 100). A full
newest page with no next cursor can seed the index immediately. A single-turn read builds a
cold long index synchronously; the next bootstrap and `/history` page can use it. There is no
background scan, worker lifetime, or unbounded queue of bootstrap indexing jobs.

Older natives that report `turns/list` as an unknown method use full `thread/read` and
local offsets/counts. Other native errors are not treated as an unsupported method.
The complete-turn helper refuses a claimed paginated full-read fallback because that
native response may omit older turns.

## Internal complete-read contract for task 08

Public modules: `cutex::management::v2::{history, turn_index}`.

```rust
let key = turn_index::Key::new(session_id, thread_id, source, runtime_generation);
let result: Result<Option<history::CompleteHistoryTurn>, history::HistoryCallError> =
    history::read_complete_turn(&key, thread_id, turn_id, &mut call);
```

`call: &mut dyn FnMut(&str, serde_json::Value) -> Result<serde_json::Value, HistoryCallError>`
returns the native RPC **result**, not its envelope. `history::rpc_call` adapts an envelope
forwarder if needed. The key's thread must match the requested thread.

- `Ok(Some(value))`: `value.turn()` and `value.thread()` are complete native JSON;
  `value.ordinal` and `value.turn_count` are available. The JSON fields are private and
  only exposed as immutable references. This type is not constructed from HTTP previews.
- `Ok(None)`: the indexed current thread has no such turn.
- `Err(HistoryCallError::Rpc(value))`: native or protocol failure. Incomplete/oversized
  detail uses RPC `code: -32001`, `data.code: "history_turn_incomplete"`; malformed
  pagination/identity uses `-32603`. Native errors otherwise retain their native code.
- `Err(HistoryCallError::Unavailable(message))`: transport/reader unavailable.

Complete reads allow 16 MiB, and return an error on native truncation, a summary/notLoaded
selected turn, partial items, capacity errors, or bad pagination. They never silently
downgrade to a preview. A caller planning a mutation must hold its own operation lock
and revalidate the binding/history version before applying changes; these read RPCs do
not constitute a transaction. This task adds no file-revert operation or new helper HTTP API.

