"""Continuation at a proven native tool-result boundary, never tool replay.

The SDK alone writes native history. This module records observations and reads
that history; missing evidence is ineligible, never synthesized into a result.
"""
from __future__ import annotations

import asyncio
import hashlib
import json
from pathlib import Path
import random
import time
import uuid


CONTINUATION = ('The connection failed after the completed tool results above. '
                'Continue the unfinished work from those results. Do not repeat completed actions. '
                'If an action has an uncertain outcome, inspect its receipt before doing anything else.')
MAX_TRANSCRIPT_BYTES = 16 * 1024 * 1024
MAX_CALLS = 1024


def field(obj, key, default=None):
    return obj.get(key, default) if isinstance(obj, dict) else getattr(obj, key, default)


def kind(obj):
    return field(obj, 'type', type(obj).__name__)


def digest(value):
    return hashlib.sha256(json.dumps(value, ensure_ascii=False, sort_keys=True,
                                    separators=(',', ':')).encode()).hexdigest()


def valid_uuid(value):
    try:
        return isinstance(value, str) and str(uuid.UUID(value)) == value
    except ValueError:
        return False


def install(store):
    store.db.execute('''CREATE TABLE IF NOT EXISTS sdk_recovery_attempts(
        turn_id TEXT NOT NULL REFERENCES turns(id) ON DELETE CASCADE,
        ordinal INTEGER NOT NULL, reason TEXT NOT NULL, state TEXT NOT NULL,
        checkpoint TEXT, created REAL NOT NULL, PRIMARY KEY(turn_id,ordinal))''')
    store.db.execute('''CREATE TABLE IF NOT EXISTS sdk_recovery_tools(
        turn_id TEXT NOT NULL REFERENCES turns(id) ON DELETE CASCADE,
        call_id TEXT NOT NULL, state TEXT NOT NULL, result_uuid TEXT, result_digest TEXT,
        PRIMARY KEY(turn_id,call_id))''')


def attempts(store, turn_id):
    return store.db.execute('SELECT count(*) FROM sdk_recovery_attempts WHERE turn_id=?', (turn_id,)).fetchone()[0]


def reserve(store, turn_id, reason, checkpoint=None):
    with store.transaction():
        ordinal = attempts(store, turn_id) + 1
        if ordinal > 2:
            return None
        store.db.execute('INSERT INTO sdk_recovery_attempts VALUES(?,?,?,?,?,?)',
                         (turn_id, ordinal, reason, 'reserved', json.dumps(checkpoint), time.time()))
        return ordinal


def delay(ordinal):
    base = (2.0, 5.0)[ordinal - 1]
    return base + random.uniform(0, base * .25)


class Checkpoint:
    def __init__(self, engine, tid, turn_id, sid):
        self.engine, self.tid, self.turn_id, self.sid = engine, tid, turn_id, sid
        self.calls = {}
        self.results = {}
        self.request_sequence = 0
        self.call_requests = {}
        self.canonical_calls = set()
        self.unsafe = False
        self.fresh_content = False
        self.hooks = 0
        self.error_uuid = None
        self.snapshot = None
        self.terminal_inflight = False
        self.pending_note = None
        self.prior_diff = None
        # A restored/interrupted original turn is never resumed by this policy.
        self.unsafe = bool(attempts(engine.store, turn_id)) or engine.store.db.execute(
            'SELECT 1 FROM sdk_recovery_tools WHERE turn_id=? LIMIT 1', (turn_id,)).fetchone() is not None

    @property
    def inflight_effects(self):
        return bool(self.hooks or self.terminal_inflight or set(self.calls) - self.results.keys())

    def admit(self, call_id, name):
        if not isinstance(call_id, str) or not call_id or len(call_id) > 256 or not isinstance(name, str):
            self.unsafe = True
            return
        if name in {'Agent', 'Task', 'AskUserQuestion'}:
            self.unsafe = True
        if call_id not in self.calls:
            if len(self.calls) >= MAX_CALLS:
                self.unsafe = True
                return
            self.calls[call_id] = name
            self.call_requests[call_id] = self.request_sequence
            self.engine.store.db.execute('INSERT OR IGNORE INTO sdk_recovery_tools VALUES(?,?,?,NULL,NULL)',
                                         (self.turn_id, call_id, 'admitted'))
        elif self.calls[call_id] != name:
            self.unsafe = True

    def observe(self, message):
        typ = kind(message)
        if field(message, 'parent_tool_use_id'):
            self.unsafe = True
            return
        subtype = field(message, 'subtype')
        if subtype in {'hook_started', 'hook_response'}:
            self.unsafe = True
            self.hooks = self.hooks + 1 if subtype == 'hook_started' else max(0, self.hooks - 1)
        if subtype == 'compact_boundary' or typ == 'ConversationResetMessage':
            self.unsafe = True
        if typ in {'TaskStartedMessage', 'TaskProgressMessage', 'TaskNotificationMessage', 'TaskUpdatedMessage'}:
            from .runtime_tasks import field as task_field
            if task_field(message, 'task_type') not in {'local_bash', 'local_shell', 'bash'}:
                self.unsafe = True
        if typ in {'StreamEvent', 'stream_event'}:
            if field(message, 'event', {}).get('type') == 'message_start':
                self.request_sequence += 1
            if field(message, 'event', {}).get('type') in {'content_block_start', 'content_block_delta'}:
                self.fresh_content = True
        if typ in {'AssistantMessage', 'assistant'}:
            if field(message, 'error'):
                self.error_uuid = field(message, 'uuid')
                return
            if field(message, 'content', []):
                self.fresh_content = True
            for block in field(message, 'content', []):
                if kind(block) in {'ToolUseBlock', 'tool_use'}:
                    self.admit(field(block, 'id'), field(block, 'name'))
                    cid = field(block, 'id')
                    if cid in self.calls and cid not in self.canonical_calls:
                        self.canonical_calls.add(cid)
                        self.call_requests[cid] = self.request_sequence
        if typ in {'UserMessage', 'user'}:
            blocks = field(message, 'content', [])
            if not isinstance(blocks, list):
                return
            new_results = False
            for block in blocks:
                if kind(block) not in {'ToolResultBlock', 'tool_result'}:
                    continue
                call_id, native_uuid = field(block, 'tool_use_id'), field(message, 'uuid')
                if call_id not in self.calls or not valid_uuid(native_uuid):
                    self.unsafe = True
                    continue
                # Failed tools can return uncertainty rather than a known effect.
                # Without a typed certainty receipt, do not automatically continue.
                if field(block, 'is_error', False):
                    self.unsafe = True
                value = digest([field(block, 'content'), bool(field(block, 'is_error', False))])
                observation = (native_uuid, value)
                new_results = new_results or call_id not in self.results
                if call_id in self.results and self.results[call_id] != observation:
                    self.unsafe = True
                    continue  # Keep the first effect receipt, not a later denial/replay.
                self.results[call_id] = observation
                self.engine.store.db.execute('''UPDATE sdk_recovery_tools SET state='result_observed',
                    result_uuid=?,result_digest=? WHERE turn_id=? AND call_id=?''',
                    (native_uuid, value, self.turn_id, call_id))
            if (new_results and self.calls and not (set(self.calls) - self.results.keys())
                    and self.request_sequence == max(self.call_requests.values())):
                self.fresh_content = False

    def live_effects(self, owner):
        bridge = owner.bridge
        pending = getattr(getattr(self.engine, 'permissions', None), 'pending', {})
        return bool(self.engine.open_tools.get(self.turn_id) or bridge.host_tasks or bridge.grants
                    or bridge.native.calls or bridge.native_questions or bridge.children
                    or bridge.background or bridge.background_spawns
                    or any(p.thread_id == self.tid and not p.future.done() for p in pending.values())
                    or any(r.get('status') not in {'completed', 'failed', 'stopped', 'killed', 'cancelled', 'lost'}
                           for r in owner.tasks.rows.values()))

    def terminal(self, owner, message):
        self.snapshot = None
        self.terminal_inflight = self.live_effects(owner) or self.hooks > 0
        if (field(message, 'session_id') != self.sid or self.terminal_inflight
                or self.unsafe or self.fresh_content or not self.calls
                or set(self.calls) != set(self.results) or not valid_uuid(self.error_uuid)):
            return
        pid, exit_code = owner.process_observation()
        if not pid or exit_code is not None:
            return
        self.prior_diff = owner.bridge.native.turn_diff
        self.snapshot = (owner.connection_id, owner.native_connection_generation,
                         owner.recovery_revision, id(owner.client), pid, owner.model_selection_id)

    def stable(self, owner):
        if (not self.snapshot or not owner or owner.active or not owner.accepting or owner.session_id != self.sid
                or owner.autonomous_until is not None or owner.notified_in_turn or owner.deferred
                or self.engine.closing or self.tid in self.engine.offline_threads
                or getattr(self.engine, 'tool_gate', None) and self.engine.tool_gate.closed(self.tid)):
            return False
        pid, exit_code = owner.process_observation()
        return (exit_code is None and self.snapshot == (owner.connection_id, owner.native_connection_generation,
                owner.recovery_arrivals, id(owner.client), pid, owner.model_selection_id) and not self.live_effects(owner)
                and not self.unsafe and not self.hooks and not self.fresh_content)

    def persisted(self, config_dir):
        """Match observed canonical result UUIDs/content on the error tip's chain."""
        if not valid_uuid(self.sid):
            return False
        root = Path(config_dir).resolve() / 'projects'
        paths = list(root.glob(f'*/{self.sid}.jsonl'))
        if len(paths) != 1 or paths[0].is_symlink() or not paths[0].resolve().is_relative_to(root.resolve()):
            return False
        path = paths[0]
        # Read at most a bounded suffix. If it does not contain every required
        # observation, fail closed; never scan a huge history on the event loop.
        with path.open('rb') as stream:
            size = stream.seek(0, 2)
            offset = max(0, size - MAX_TRANSCRIPT_BYTES)
            stream.seek(offset)
            raw = stream.read(MAX_TRANSCRIPT_BYTES + 1)
        if offset:
            raw = raw.partition(b'\n')[2]
        if not raw.endswith(b'\n'):
            return False
        entries = {}
        last = None
        for line in raw.splitlines():
            if not line.strip():
                continue
            entry = json.loads(line)
            if not isinstance(entry, dict):
                return False
            if entry.get('isSidechain') or not entry.get('uuid'):
                continue
            if entry.get('sessionId') != self.sid:
                return False
            key = entry['uuid']
            if key in entries and entries[key] != entry:
                return False
            entries[key] = entry
            last = key
        if last != self.error_uuid or self.error_uuid not in entries:
            return False
        tip = entries[self.error_uuid]
        if not tip.get('isApiErrorMessage') or tip.get('error') not in {'server_error', 'unknown'}:
            return False
        calls, results, walked = set(), {}, set()
        cursor = self.error_uuid
        while cursor in entries:
            if cursor in walked:
                return False
            walked.add(cursor)
            entry = entries[cursor]
            for block in (entry.get('message') or {}).get('content', []):
                if not isinstance(block, dict):
                    continue
                if block.get('type') == 'tool_use' and block.get('id') in self.calls:
                    if block.get('name') != self.calls[block['id']]:
                        return False
                    calls.add(block['id'])
                if block.get('type') == 'tool_result' and block.get('tool_use_id') in self.calls:
                    cid = block['tool_use_id']
                    observed = (entry['uuid'], digest([block.get('content'), bool(block.get('is_error', False))]))
                    if cid in results and results[cid] != observed:
                        return False
                    results[cid] = observed
            cursor = entry.get('parentUuid')
        # CLI parallel tools have one assistant UUID per tool, and each user
        # result points to its own assistant UUID. Earlier sibling results are
        # not on the last result's parent chain. Accept only the exact result
        # UUID we observed, attached directly to its active-chain tool message.
        for cid, (native_uuid, value) in self.results.items():
            if cid in results:
                continue
            entry = entries.get(native_uuid, {})
            parent = entries.get(entry.get('parentUuid'), {})
            if (entry.get('type') != 'user' or entry.get('parentUuid') not in walked
                    or not any(isinstance(b, dict) and b.get('type') == 'tool_use' and b.get('id') == cid
                               for b in (parent.get('message') or {}).get('content', []))):
                return False
            matches = [b for b in (entry.get('message') or {}).get('content', [])
                       if isinstance(b, dict) and b.get('type') == 'tool_result' and b.get('tool_use_id') == cid]
            if len(matches) != 1:
                return False
            block = matches[0]
            results[cid] = (native_uuid, digest([block.get('content'), bool(block.get('is_error', False))]))
        return calls == set(self.calls) and results == self.results

    async def verified(self, owner):
        if not self.stable(owner):
            return False
        try:
            # A terminal CLI frame may precede the filesystem flush slightly.
            for _ in range(3):
                if await asyncio.to_thread(self.persisted, owner.driver.config_dir):
                    return self.stable(owner)
                await asyncio.sleep(.05)
                if not self.stable(owner):
                    return False
        except (OSError, ValueError, TypeError, KeyError, UnicodeError):
            return False
        return False

    async def submitted(self):
        if self.pending_note is None:
            return
        ordinal = self.pending_note
        self.pending_note = None
        self.engine.store.db.execute("UPDATE sdk_recovery_attempts SET state='submitted' WHERE turn_id=? AND ordinal=?",
                                     (self.turn_id, ordinal))
        from .presentation import input_presentation
        row = self.engine.store.db.execute('SELECT payload FROM inputs WHERE turn_id=?', (self.turn_id,)).fetchone()
        metadata = json.loads(row[0]).get('metadata', {}) if row else {}
        metadata = {**metadata, 'sourceName': 'Claude runtime'}
        payload = {'source': {'kind': 'service', 'id': 'claude-runtime'}, 'eventType': 'connection_recovery',
                   'text': f'连接中断，已自动继续 ({ordinal}/2)', 'metadata': metadata}
        item = input_presentation(self.tid, f'connection-recovery:{self.turn_id}:{ordinal}', payload, time.time())
        await self.engine.emit(self.tid, 'thread/presentation/appended',
                               {'threadId': self.tid, 'turnId': self.turn_id, 'item': item})


def candidate(error):
    diagnostic = getattr(error, 'diagnostic', None)
    return (getattr(error, 'native_code', None) in {'server_error', 'unknown'}
            and diagnostic is not None and diagnostic.category == 'ECONNRESET'
            and diagnostic.http_status is None and not getattr(error, 'transient_refresh', False))
