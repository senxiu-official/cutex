"""Offline native session mutations and transactional display projections.

The official SDK alone writes native transcripts. Revert creates a native fork
at the retained prefix and switches the adapter binding; the source remains an
audit backup. No saved inbox input, permission, or tool invocation is replayed.
"""
from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
import re
import sys
import time
import uuid
from contextlib import ExitStack
from functools import wraps

from .errors import RpcError, require, text
from .history import History, decode, encode, thread_wire
from .sdk_runtime import NativeSessionLease
from .store import Store, dump, identifier


def structural(method):
    @wraps(method)
    async def run(self, tid, *args, **kwargs):
        with self.store.history_mutation(tid):
            return await method(self, tid, *args, **kwargs)
    return run


class NativeSessionApi:
    def __init__(self, engine):
        self.engine = engine
        self.config_dir = Path(engine.driver.config_dir).resolve()

    def transcript(self, sid: str) -> Path | None:
        uuid.UUID(sid)
        root = self.config_dir / 'projects'
        matches = []
        if root.is_dir():
            for path in root.glob(f'*/{sid}.jsonl'):
                require(not path.is_symlink() and path.resolve().is_relative_to(root.resolve()),
                        'native transcript resolves outside the selected account directory', -32009)
                matches.append(path)
        require(len(matches) <= 1, 'native session UUID is ambiguous in this account', -32009)
        return matches[0] if matches else None

    def entries(self, sid: str, *, live: bool = False) -> list[dict]:
        """Main-chain transcript entries. `live`: a running Claude Code may be
        appending, so an unterminated last line is a record still being written
        and is left out; any other unreadable record still fails."""
        path = self.transcript(sid)
        require(path is not None, 'native transcript is unavailable; no history was changed', -32004)
        entries = []
        raw = path.read_bytes()
        lines = raw.split(b'\n')
        for index, line in enumerate(lines):
            if not line.strip():
                continue
            try:
                entry = json.loads(line.decode('utf-8'))
            except (ValueError, UnicodeError):
                if live and index == len(lines) - 1:
                    break
                raise RpcError(-32009, 'native transcript has an incomplete record; no history was changed') from None
            if isinstance(entry, dict) and entry.get('uuid') and not entry.get('isSidechain'):
                entries.append(entry)
        return entries

    async def call(self, kind: str, sid: str, **arguments) -> dict:
        # SDK filesystem helpers use process-global CLAUDE_CONFIG_DIR. Isolate
        # that setting in a short-lived Python helper; never mutate server env.
        env = dict(os.environ)
        env['CLAUDE_CONFIG_DIR'] = str(self.config_dir)
        env['PYTHONPATH'] = str(Path(__file__).resolve().parents[1]) + os.pathsep + env.get('PYTHONPATH', '')
        process = await asyncio.create_subprocess_exec(sys.executable, '-m', 'cute_claude.session_ops', '--native',
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE, env=env)
        try:
            out, _ = await asyncio.wait_for(process.communicate(dump({'kind': kind, 'sessionId': sid, **arguments}).encode()), 60)
        except BaseException:
            process.kill()
            await process.wait()
            raise
        require(process.returncode == 0, 'official SDK session mutation failed; inspect session operation audit', -32009)
        try:
            return json.loads(out)
        except (ValueError, UnicodeError):
            raise RpcError(-32009, 'official SDK returned an invalid session mutation receipt') from None


class SessionOperations:
    def __init__(self, engine, native=None):
        self.engine, self.store = engine, engine.store
        self.native = native

    def api(self):
        require(hasattr(self.engine, 'session'), 'native session operation requires the subscription backend', -32601)
        if self.native is None:
            self.native = NativeSessionApi(self.engine)
        return self.native

    def idle(self, tid):
        self.store.thread(tid)
        require(not self.store.pending(tid) and not self.store.pending_notes(tid),
                'finish or explicitly clear queued inputs and notes before changing session history', -32009)
        require(not self.store.db.execute("SELECT 1 FROM turns WHERE thread_id=? AND state='inProgress'", (tid,)).fetchone(),
                'interrupt the active turn before changing session history', -32009)

    async def close(self, tid):
        if getattr(self.engine.driver, 'persistent', False):
            await self.engine.close_runtime(tid)
            owner = self.engine.driver.owners.get(tid)
            require(owner is None or (owner.task.done() and owner.lease is None),
                    'native process exit is unverified; history was not changed', -32009)
            self.engine.driver.owners.pop(tid, None)

    def was_online(self, tid):
        owner = getattr(self.engine.driver, 'owners', {}).get(tid)
        return owner is not None and not owner.task.done()

    async def restore_runtime(self, tid, online, offline):
        if online:
            try:
                return await self.engine.start_runtime(tid)
            except Exception as exc:
                # A committed native binding must still reach the controller
                # if connection-only restart fails. Never pretend the history
                # mutation rolled back, and never issue a business prompt.
                return {'threadId': tid, 'state': 'failed', 'sessionId': self.engine.session(tid)['session_id'],
                        'restartError': type(exc).__name__}
        if not offline and hasattr(self.engine, 'offline_threads'):
            self.engine.offline_threads.discard(tid)
        return self.engine.runtime_status(tid) if hasattr(self.engine, 'runtime_status') else None

    def visible_turns(self, tid):
        return [dict(r) for r in self.store.db.execute(
            'SELECT * FROM turns WHERE thread_id=? AND reverted=0 ORDER BY started,rowid', (tid,))]

    def checkpoint(self, tid, turn_id, entries):
        """Resolve an observed user prompt, with a conservative legacy fallback.

        r7 did not capture SDK UUIDs. Its plain prompts can be recovered only
        when exactly one native user message matches the stored projection.
        """
        ids = {r[0] for r in self.store.db.execute(
            "SELECT native_uuid FROM sdk_turn_messages WHERE thread_id=? AND turn_id=? AND role='user'", (tid, turn_id))}
        candidates = [e for e in entries if e['uuid'] in ids and e.get('type') == 'user']
        if candidates:
            return candidates[0]
        row = self.store.db.execute('SELECT payload FROM inputs WHERE thread_id=? AND turn_id=?', (tid, turn_id)).fetchone()
        if row:
            expected = Store.project_input(json.loads(row[0]))[0]['text']
            def prompt(entry):
                content = entry.get('message', {}).get('content', '')
                return content if isinstance(content, str) else '\n'.join(b.get('text', '') for b in content if b.get('type') == 'text')
            candidates = [e for e in entries if e.get('type') == 'user' and not e.get('isMeta') and prompt(e) == expected]
        require(len(candidates) == 1, 'no unambiguous native prompt checkpoint for this turn; no history was changed', -32009)
        return candidates[0]

    def prefix(self, tid, turns, keep_count, entries):
        if not keep_count:
            return None
        if keep_count == len(turns):
            require(bool(entries), 'native transcript has no messages to fork', -32009)
            return entries[-1]['uuid']
        excluded = self.checkpoint(tid, turns[keep_count]['id'], entries)
        parent = excluded.get('parentUuid')
        require(parent and any(e['uuid'] == parent for e in entries),
                'native checkpoint has no retained parent entry; no history was changed', -32009)
        return parent

    def journal(self, tid, kind, data):
        action_id = identifier()
        self.store.db.execute('INSERT INTO session_operations VALUES(?,?,?,?,?,?)',
            (action_id, tid, kind, 'prepared', dump(data), time.time()))
        return action_id

    def audit(self, action_id, data, state='completed'):
        directory = self.store.directory / 'session-operations'
        directory.mkdir(exist_ok=True, mode=0o700)
        path = directory / f'{action_id}.json'
        record = {**data, 'actionId': action_id, 'state': state, 'receiptPath': str(path)}
        # Replace, never append duplicate JSON documents after a failed operation.
        temporary = path.with_suffix('.tmp')
        temporary.write_text(dump(record), encoding='utf-8')
        temporary.replace(path)
        self.store.db.execute('UPDATE session_operations SET state=?,data=? WHERE id=?', (state, dump(record), action_id))
        return record

    def busy(self, tid) -> bool:
        """A running turn, or queued inputs / notes, in the source."""
        return bool(self.store.pending(tid) or self.store.pending_notes(tid) or self.store.db.execute(
            "SELECT 1 FROM turns WHERE thread_id=? AND reverted=0 AND state='inProgress'", (tid,)).fetchone())

    @staticmethod
    def boundary(turns: list[dict], last: str | None = None, before: str | None = None) -> int:
        """How many visible turns a fork of a possibly busy source keeps: through
        `last` (inclusive, must have ended), before `before`, or by default every
        turn before the first running one. Same rule as Codex forks and Side."""
        ids = [t['id'] for t in turns]
        require(last is None or before is None, 'lastTurnId and beforeTurnId are mutually exclusive')
        for name, value in (('lastTurnId', last), ('beforeTurnId', before)):
            if value is not None and value not in ids:
                raise RpcError(-32004, f'{name} is not a visible turn', {'code': 'turn_not_found', 'turnId': value})
        if last is not None:
            keep = ids.index(last) + 1
            if turns[keep - 1]['state'] == 'inProgress':
                raise RpcError(-32009, 'the turn is still running; fork after it completed or pick an earlier turn',
                               {'code': 'turn_in_progress', 'turnId': last})
            return keep
        if before is not None:
            keep = ids.index(before)
        else:
            keep = next((i for i, t in enumerate(turns) if t['state'] == 'inProgress'), len(turns))
        running = next((t['id'] for t in turns[:keep] if t['state'] == 'inProgress'), None)
        if running:
            raise RpcError(-32009, 'the turn is still running; fork after it completed or pick an earlier turn',
                           {'code': 'turn_in_progress', 'turnId': running})
        return keep

    @staticmethod
    def prompt_entry(entry: dict) -> bool:
        """A typed user prompt (not a tool result or meta record)."""
        if entry.get('type') != 'user' or entry.get('isMeta'):
            return False
        content = entry.get('message', {}).get('content')
        return isinstance(content, str) or (isinstance(content, list) and any(
            isinstance(b, dict) and b.get('type') != 'tool_result' for b in content))

    async def live_cutoff(self, tid: str, turns: list[dict], keep: int, sid: str) -> str | None:
        """Native UUID ending `turns[:keep]` in a transcript the source's live
        runtime may still append to (it is never closed or leased). None: keep
        nothing. Refuses (retryable) rather than guess where a running turn starts."""
        if not keep:
            return None
        api = self.api()
        observed = [r[0] for r in self.store.db.execute('SELECT native_uuid FROM sdk_turn_messages WHERE thread_id=? '
                                                         'AND turn_id=? ORDER BY seq', (tid, turns[keep - 1]['id']))]
        # Claude Code writes its transcript asynchronously: wait briefly until the
        # kept turn's last observed message is on disk (or, after that, at least
        # one of them: a streamed message need not be persisted).
        for _ in range(30):
            entries = api.entries(sid, live=True)
            if not observed or observed[-1] in {e['uuid'] for e in entries}:
                break
            await asyncio.sleep(0.1)
        else:
            if not set(observed) & {e['uuid'] for e in entries}:
                raise RpcError(-32009, 'the native transcript has not caught up with the selected turn yet; retry in a moment',
                               {'code': 'native_transcript_behind', 'retryable': True})
        running = any(t['state'] == 'inProgress' for t in turns)
        if keep < len(turns):
            try:
                return self.prefix(tid, turns, keep, entries)
            except RpcError:
                if turns[keep]['state'] != 'inProgress':
                    raise
            # The running turn's prompt is not identifiable (not yet written, or not
            # yet observed): only safe when none of its messages are on disk.
            later = {r[0] for r in self.store.db.execute(
                'SELECT native_uuid FROM sdk_turn_messages WHERE thread_id=? AND turn_id=?', (tid, turns[keep]['id']))}
            if any(e['uuid'] in later for e in entries):
                raise RpcError(-32009, 'the running turn cannot be separated from the kept history yet; retry in a moment',
                               {'code': 'native_transcript_behind', 'retryable': True})
        require(bool(entries), 'native transcript has no messages to fork', -32009)
        if running and self.prompt_entry(entries[-1]):
            # A prompt after the last finished turn belongs to the running one.
            raise RpcError(-32009, 'the running turn has just started; retry in a moment',
                           {'code': 'native_transcript_behind', 'retryable': True})
        return entries[-1]['uuid']

    def fork_target(self, params: dict) -> tuple[str, Path | None]:
        target_id = params.get('targetThreadId') or identifier()
        try:
            valid_id = str(uuid.UUID(target_id)) == target_id
        except (ValueError, TypeError, AttributeError):
            valid_id = False
        require(valid_id, 'targetThreadId must be a canonical UUID')
        target_path = Path(params['targetStateDir']) if params.get('targetStateDir') else None
        if target_path:
            require(target_path.is_absolute() and not target_path.is_symlink(), 'targetStateDir must be an absolute private directory')
            require(not target_path.exists() or not any(target_path.iterdir()), 'targetStateDir must be empty', -32009)
        return target_id, target_path

    def target_store(self, target_path: Path | None) -> Store:
        if not target_path:
            return self.store
        target_store = Store(target_path)
        target_store.bind_backend('subscription')
        target_store.db.execute('''CREATE TABLE IF NOT EXISTS sdk_sessions(
            thread_id TEXT PRIMARY KEY REFERENCES threads(id), session_id TEXT NOT NULL UNIQUE,
            attempted INTEGER NOT NULL DEFAULT 0, last_status TEXT NOT NULL DEFAULT 'allocated')''')
        return target_store

    async def fork(self, tid: str, params: dict | None = None) -> dict:
        """An idle source is forked offline (its runtime closed and restarted
        around the native copy). A busy one (running turn, queued inputs or
        notes) is forked live from its transcript: the running turn and the
        queue are left out, and the source runtime is not touched (claude-r21)."""
        params = params or {}
        self.store.thread(tid)
        if self.busy(tid):
            return await self.live_fork(tid, params)
        return await self.offline_fork(tid, params)

    async def live_fork(self, tid: str, params: dict) -> dict:
        api = self.api()
        target_id, target_path = self.fork_target(params)
        turns = self.visible_turns(tid)
        keep = self.boundary(turns, params.get('lastTurnId'), params.get('beforeTurnId'))
        ids = [t['id'] for t in turns]
        name = text(params['name'], 'name', limit=512) if params.get('name') is not None else None
        session = self.engine.session(tid)
        action_id = self.journal(tid, 'fork', {'sourceSessionId': session['session_id'], 'targetThreadId': target_id, 'live': True})
        result, target_store = None, self.store
        try:
            if keep:
                cutoff = await self.live_cutoff(tid, turns, keep, session['session_id'])
                # Read-only on the source transcript: no lease, the owner keeps writing.
                result = await api.call('fork', session['session_id'], upToMessageId=cutoff, title=name)
            else:
                result = {'sessionId': identifier()}
            target_store = self.target_store(target_path)
            mapping = {e.get('forkedFrom', {}).get('messageUuid'): e['uuid'] for e in api.entries(result['sessionId'])} if keep else {}
            self._copy_projection(target_store, tid, target_id, turns[:keep], result['sessionId'], bool(keep), params, mapping)
            return self.audit(action_id, {'threadId': target_id, 'nativeSessionId': result['sessionId'],
                'stateDir': str(target_store.directory), 'forkedThroughTurnId': ids[keep - 1] if keep else None,
                'sourceThreadId': tid, 'sourceRuntime': None, 'sourceRuntimeUntouched': True,
                'fileCheckpointsCopied': False})
        except BaseException:
            self.audit(action_id, {'sourceThreadId': tid, 'targetThreadId': target_id,
                'stateDir': str(target_path or self.store.directory),
                'nativeSessionId': result.get('sessionId') if result else None}, 'failed')
            raise
        finally:
            if target_store is not self.store:
                target_store.close()

    @structural
    async def offline_fork(self, tid: str, params: dict) -> dict:
        self.idle(tid)
        api = self.api()
        target_id, target_path = self.fork_target(params)
        online, offline = self.was_online(tid), tid in getattr(self.engine, 'offline_threads', set())
        turns = self.visible_turns(tid)
        last = params.get('lastTurnId')
        before = params.get('beforeTurnId')
        ids = [t['id'] for t in turns]
        require(last is None or before is None, 'lastTurnId and beforeTurnId are mutually exclusive')
        require(last is None or last in ids, 'lastTurnId is not a visible turn', -32004)
        require(before is None or before in ids, 'beforeTurnId is not a visible turn', -32004)
        keep = ids.index(before) if before else (ids.index(last) + 1 if last else len(turns))
        name = text(params['name'], 'name', limit=512) if params.get('name') is not None else None
        session = self.engine.session(tid)
        await self.close(tid)
        # Closing does not re-enqueue inputs; it merely flushes the native log.
        action_id = self.journal(tid, 'fork', {'sourceSessionId': session['session_id'], 'targetThreadId': target_id})
        result, target_store = None, self.store
        try:
            with_lease = NativeSessionLease(api.config_dir, session['session_id'])
            try:
                if keep:
                    entries = api.entries(session['session_id'])
                    cutoff = self.prefix(tid, turns, keep, entries)
                    result = await api.call('fork', session['session_id'], upToMessageId=cutoff, title=name)
                else:
                    result = {'sessionId': identifier()}
            finally:
                with_lease.close()
            target_store = self.target_store(target_path)
            mapping = {e.get('forkedFrom', {}).get('messageUuid'): e['uuid'] for e in api.entries(result['sessionId'])} if keep else {}
            self._copy_projection(target_store, tid, target_id, turns[:keep], result['sessionId'], bool(keep), params, mapping)
            source_runtime = await self.restore_runtime(tid, online, offline)
            receipt = self.audit(action_id, {'threadId': target_id, 'nativeSessionId': result['sessionId'],
                'stateDir': str(target_store.directory), 'forkedThroughTurnId': ids[keep - 1] if keep else None,
                'sourceThreadId': tid, 'sourceRuntime': source_runtime, 'fileCheckpointsCopied': False})
            return receipt
        except BaseException:
            self.audit(action_id, {'sourceThreadId': tid, 'targetThreadId': target_id,
                'stateDir': str(target_path or self.store.directory),
                'nativeSessionId': result.get('sessionId') if result else None}, 'failed')
            raise
        finally:
            if target_store is not self.store:
                target_store.close()

    def _copy_projection(self, target, tid, new_id, turns, sid, attempted, params, mapping):
        source = self.store.thread(tid)
        now = time.time()
        with target.transaction():
            require(target.db.execute('SELECT 1 FROM threads WHERE id=?', (new_id,)).fetchone() is None, 'target thread already exists', -32009)
            target.db.execute('INSERT INTO threads(id,model,cwd,created,updated,instructions,name) VALUES(?,?,?,?,?,?,?)',
                (new_id, params.get('model') or source['model'], source['cwd'], now, now,
                 params.get('instructions', source['instructions']), params.get('name') or source['name']))
            props = self.store.thread_properties(tid)
            props.update(forkedFromId=tid)
            target.set_thread_properties(new_id, props)
            target.db.execute('INSERT INTO sdk_sessions(thread_id,session_id,attempted,last_status) VALUES(?,?,?,?)',
                (new_id, sid, int(attempted), 'forked' if attempted else 'allocated'))
            for turn in turns:
                copied_id = identifier()
                target.db.execute('INSERT INTO turns(id,thread_id,state,started,completed,error,usage) VALUES(?,?,?,?,?,?,?)',
                    (copied_id, new_id, turn['state'], turn['started'], turn['completed'], turn['error'],
                     dump({'inheritedFrom': {'threadId': tid, 'turnId': turn['id']}, 'calls': []})))
                for item in self.store.items(turn['id']):
                    item['id'] = identifier()
                    target.put_item(copied_id, item)
                for message in self.store.db.execute('SELECT native_uuid,role FROM sdk_turn_messages WHERE thread_id=? AND turn_id=?', (tid, turn['id'])):
                    if message[0] in mapping:
                        target.record_sdk_message(new_id, copied_id, mapping[message[0]], message[1])
                # Terminal source receipts retain display attribution and legacy
                # checkpoint matching; state=inherited can never be claimed.
                original = self.store.db.execute('SELECT * FROM inputs WHERE thread_id=? AND turn_id=?', (tid, turn['id'])).fetchone()
                if original:
                    target.db.execute('INSERT INTO inputs(thread_id,message_id,semantic_hash,payload,turn_id,state,created) VALUES(?,?,?,?,?,\'inherited\',?)',
                        (new_id, original['message_id'], original['semantic_hash'], original['payload'], copied_id, original['created']))
            # Copy durable preferences only, never per-session approval grants.
            for table in ('thread_efforts', 'thread_tool_approvals', 'thread_launch_models', 'thread_plan_mode', 'thread_fast_mode'):
                schema = self.store.db.execute('SELECT sql FROM sqlite_master WHERE type=\'table\' AND name=?', (table,)).fetchone()
                if schema:
                    target.db.execute(schema[0].replace('CREATE TABLE ', 'CREATE TABLE IF NOT EXISTS ', 1))
                    row = self.store.db.execute(f'SELECT * FROM {table} WHERE thread_id=?', (tid,)).fetchone()
                    if row:
                        target.db.execute(f'INSERT INTO {table} VALUES({",".join("?" for _ in row)})', (new_id, *tuple(row)[1:]))

    @structural
    async def revert(self, tid: str, before_turn_id: str, *, rewind_files=False) -> dict:
        self.idle(tid)
        api = self.api()
        turns = self.visible_turns(tid)
        ids = [t['id'] for t in turns]
        require(before_turn_id in ids, 'beforeTurnId is not a visible turn', -32004)
        keep = ids.index(before_turn_id)
        session = self.engine.session(tid)
        online, offline = self.was_online(tid), tid in getattr(self.engine, 'offline_threads', set())
        source_path = api.transcript(session['session_id'])
        entries = api.entries(session['session_id']) if source_path else []
        checkpoint = self.checkpoint(tid, before_turn_id, entries) if rewind_files else None
        cutoff = self.prefix(tid, turns, keep, entries)
        data = {'threadId': tid, 'beforeTurnId': before_turn_id, 'removedTurnIds': ids[keep:],
            'sourceSessionId': session['session_id'], 'backupPath': str(source_path) if source_path else None,
            'rewindFilesRequested': bool(rewind_files), 'filesRewound': False, 'historyCommitted': False}
        action_id = self.journal(tid, 'revert', data)
        try:
            if rewind_files:
                # The SDK owns file-history validation. A missing checkpoint is
                # an error, never reported as a successful file rewind.
                await self.engine.runtime_control(tid, 'rewind_files', user_message_id=checkpoint['uuid'])
                data['filesRewound'] = True
                # File rollback is an independent native operation. Retain its
                # acknowledgement before any close/fork/projection can fail.
                self.store.db.execute('UPDATE session_operations SET data=? WHERE id=?', (dump(data), action_id))
            await self.close(tid)
            lease = NativeSessionLease(api.config_dir, session['session_id'])
            try:
                new_session = await api.call('fork', session['session_id'], upToMessageId=cutoff) if keep else {'sessionId': identifier()}
                data['nativeSessionId'] = new_session['sessionId']
                with self.store.transaction():
                    for removed in ids[keep:]:
                        self.store.db.execute('UPDATE turns SET reverted=1 WHERE id=?', (removed,))
                        self.store.db.execute("UPDATE inputs SET state='reverted' WHERE turn_id=?", (removed,))
                        self.store.db.execute("UPDATE notes SET state='reverted' WHERE turn_id=?", (removed,))
                    self.store.db.execute("UPDATE sdk_sessions SET session_id=?,attempted=?,last_status='reverted' WHERE thread_id=?", (new_session['sessionId'], int(bool(keep)), tid))
                    self.store.db.execute('UPDATE threads SET halted=0,updated=? WHERE id=?', (time.time(), tid))
                    # Checklist state is a projection of tool calls, not a
                    # permission or native transcript. Do not show reverted tasks.
                    if self.store.db.execute("SELECT 1 FROM sqlite_master WHERE name='thread_plan_state'").fetchone():
                        self.store.db.execute('DELETE FROM thread_plan_state WHERE thread_id=?', (tid,))
                    # Remap retained checkpoints from the SDK's public fork provenance.
                    if keep:
                        mapping = {e.get('forkedFrom', {}).get('messageUuid'): e['uuid'] for e in api.entries(new_session['sessionId'])}
                        for old, new in mapping.items():
                            self.store.db.execute('UPDATE sdk_turn_messages SET native_uuid=? WHERE thread_id=? AND native_uuid=?', (new, tid, old))
                    committed_data = {**data, 'historyCommitted': True}
                    self.store.db.execute('UPDATE session_operations SET state=\'committed\',data=? WHERE id=?', (dump(committed_data), action_id))
                data = committed_data
            finally:
                lease.close()
            data['runtime'] = await self.restore_runtime(tid, online, offline)
            return self.audit(action_id, data)
        except BaseException as exc:
            if data['filesRewound']:
                warning = {'code': 'fileRewindPartialFailure', 'filesRewound': True,
                           'historyCommitted': data['historyCommitted'],
                           'message': 'Files were rewound, but the session history operation did not finish. Inspect this receipt before another history operation.'}
                data['warnings'] = [warning]
                receipt = self.audit(action_id, data, 'partial')
                if isinstance(exc, Exception):
                    raise RpcError(-32009, warning['message'], {
                        'reason': warning['code'], 'outcomeUnknown': True, 'actionId': action_id,
                        'filesRewound': True, 'historyCommitted': data['historyCommitted'],
                        'receiptPath': receipt['receiptPath'], 'receipt': receipt,
                        'causeType': type(exc).__name__}) from None
            else:
                self.audit(action_id, data, 'failed')
            raise

    @structural
    async def delete(self, tid: str) -> dict:
        self.idle(tid)
        api = self.api()
        session = self.engine.session(tid)
        await self.close(tid)
        sessions = {session['session_id']}
        # Revert backups belong to this logical thread; a delete removes those
        # native contents too. Fork descendants own independent SDK copies.
        for row in self.store.db.execute("SELECT data FROM session_operations WHERE thread_id=? AND kind='revert'", (tid,)):
            old = json.loads(row[0]).get('sourceSessionId')
            if old:
                sessions.add(old)
        data = {'threadId': tid, 'nativeSessionIds': sorted(sessions), 'deletedNativeSessionIds': []}
        action_id = self.journal(tid, 'delete', data)
        try:
            with ExitStack() as leases:
                for sid in sorted(sessions):
                    lease = NativeSessionLease(api.config_dir, sid)
                    leases.callback(lease.close)
                for sid in sorted(sessions):
                    if api.transcript(sid) is not None:
                        await api.call('delete', sid)
                    data['deletedNativeSessionIds'].append(sid)
                    self.store.db.execute('UPDATE session_operations SET data=? WHERE id=?', (dump(data), action_id))
                with self.store.transaction():
                    self.store.db.execute('DELETE FROM items WHERE turn_id IN (SELECT id FROM turns WHERE thread_id=?)', (tid,))
                    # Extension tables (external-input receipts, metrics, settings)
                    # are covered without importing each service into this module.
                    tables = [r[0] for r in self.store.db.execute("SELECT name FROM sqlite_master WHERE type='table'")]
                    for table in tables:
                        if table in {'threads', 'session_operations'} or not re.fullmatch(r'[A-Za-z_][A-Za-z_0-9]*', table):
                            continue
                        columns = {r[1] for r in self.store.db.execute(f'PRAGMA table_info("{table}")')}
                        if 'thread_id' in columns:
                            self.store.db.execute(f'DELETE FROM "{table}" WHERE thread_id=?', (tid,))
                    self.store.db.execute('DELETE FROM threads WHERE id=?', (tid,))
            self.audit(action_id, data)
        except BaseException:
            self.audit(action_id, data, 'failed')
            raise
        return {}

    def metadata(self, tid: str, params: dict):
        self.store.thread(tid)
        require(set(params) <= {'threadId', 'gitInfo'}, 'unsupported metadata fields')
        if 'gitInfo' not in params:
            return
        patch = params['gitInfo']
        require(patch is None or isinstance(patch, dict), 'gitInfo must be an object or null')
        props = self.store.thread_properties(tid)
        if patch is None:
            props['gitInfo'] = None
        else:
            require(set(patch) <= {'sha', 'branch', 'originUrl'}, 'unsupported gitInfo field')
            require(all(v is None or isinstance(v, str) for v in patch.values()), 'gitInfo values must be strings or null')
            props['gitInfo'] = {**(props.get('gitInfo') or {'sha': None, 'branch': None, 'originUrl': None}), **patch}
        with self.store.transaction():
            self.store.set_thread_properties(tid, props)
            self.store.db.execute('UPDATE threads SET updated=? WHERE id=?', (time.time(), tid))

    @staticmethod
    def search_text(item):
        if item.get('type') == 'userMessage':
            return '\n'.join(b.get('text', '') for b in item.get('content', []) if b.get('type') == 'text')
        if item.get('type') == 'agentMessage' and item.get('phase') in {None, 'final_answer'}:
            return item.get('text', '')
        return ''

    @staticmethod
    def search_limit(params):
        value = params.get('limit') if params.get('limit') is not None else 50
        require(type(value) is int and value > 0, 'limit must be a positive integer')
        return min(value, 200)

    @staticmethod
    def snippet(content, match):
        start, end = max(0, match.start() - 80), min(len(content), match.end() + 160)
        prefix = '…' if start else ''
        snippet = prefix + content[start:end] + ('…' if end < len(content) else '')
        before = prefix + content[start:match.start()]
        offset = len(before.encode('utf-16-le')) // 2
        return snippet, {'start': offset, 'end': offset + len(match.group().encode('utf-16-le')) // 2}

    def search_occurrences(self, tid: str, params: dict) -> dict:
        self.store.thread(tid)
        term = text(params.get('searchTerm'), 'searchTerm', limit=2048)
        limit = self.search_limit(params)
        matcher = re.compile(re.escape(term), re.IGNORECASE)
        scope = ['occurrences', tid, term]
        anchor, _ = decode(params.get('cursor'), scope)
        require(anchor is None or (isinstance(anchor, list) and len(anchor) == 2 and all(type(v) is int and v >= 0 for v in anchor)), 'invalid occurrence cursor')
        data = []
        rows = self.store.db.execute('''SELECT i.seq,i.id,i.turn_id,i.payload,t.started FROM items i
            JOIN turns t ON t.id=i.turn_id WHERE t.thread_id=? AND t.reverted=0 AND i.seq>=? ORDER BY i.seq''',
            (tid, anchor[0] if anchor else 0))
        for row in rows:
            content = self.search_text(json.loads(row['payload']))
            for index, match in enumerate(matcher.finditer(content)):
                if anchor and row['seq'] == anchor[0] and index < anchor[1]:
                    continue
                if len(data) == limit:
                    return {'data': data, 'nextCursor': encode(scope, [row['seq'], index])}
                snippet, matched = self.snippet(content, match)
                data.append({'turnId': row['turn_id'], 'itemId': row['id'], 'snippet': snippet,
                    'snippetMatchRange': matched,
                    'turnCursor': encode(['turns', tid, None], [row['started'], row['turn_id']], True)})
        return {'data': data, 'nextCursor': None}

    def search(self, params: dict, wire=None) -> dict:
        term = text(params.get('searchTerm'), 'searchTerm', limit=2048)
        limit = self.search_limit(params)
        sort = params.get('sortKey') or 'created_at'
        require(sort in {'created_at', 'updated_at', 'recency_at'}, 'invalid sortKey')
        direction = params.get('sortDirection') or 'desc'
        require(direction in {'asc', 'desc'}, 'invalid sortDirection')
        sources = params.get('sourceKinds') or []
        require(isinstance(sources, list) and all(isinstance(s, str) for s in sources), 'invalid sourceKinds')
        archived = params.get('archived') or False
        require(type(archived) is bool, 'archived must be boolean')
        scope = ['threadSearch', term, sort, archived, sources]
        anchor, inclusive = decode(params.get('cursor'), scope)
        require(anchor is None or (isinstance(anchor, list) and len(anchor) == 2 and isinstance(anchor[0], (float, int)) and isinstance(anchor[1], str)), 'invalid thread search cursor')
        if sources and 'appServer' not in sources:
            return {'data': [], 'nextCursor': None, 'backwardsCursor': None}
        column = 'created' if sort == 'created_at' else 'updated'
        sql, args = 'SELECT id,' + column + ' AS ordering,name FROM threads WHERE archived=?', [archived]
        if anchor:
            op = ('>' if direction == 'asc' else '<') + ('=' if inclusive else '')
            sql += f' AND ({column},id) {op} (?,?)'
            args.extend(anchor)
        sql += f' ORDER BY {column} {direction},id {direction}'
        matcher, history = re.compile(re.escape(term), re.IGNORECASE), History(self.store)
        data, keys = [], []
        for row in self.store.db.execute(sql, args):
            content, match = row['name'] or '', matcher.search(row['name'] or '')
            if not match:
                for item in self.store.db.execute('''SELECT i.payload FROM items i JOIN turns t ON t.id=i.turn_id
                    WHERE t.thread_id=? AND t.reverted=0 ORDER BY i.seq''', (row['id'],)):
                    content = self.search_text(json.loads(item[0]))
                    match = matcher.search(content)
                    if match:
                        break
            if not match:
                continue
            if len(data) == limit:
                return {'data': data, 'nextCursor': encode(scope, keys[-1]), 'backwardsCursor': encode(scope, keys[0], True)}
            data.append({'thread': wire(row['id']) if wire else thread_wire(self.store, history, row['id']),
                         'snippet': self.snippet(content, match)[0]})
            keys.append([row['ordering'], row['id']])
        return {'data': data, 'nextCursor': None, 'backwardsCursor': encode(scope, keys[0], True) if keys else None}


def _native_main():
    # Private adapter helper; stdin contains IDs only, no prompt or credential.
    import claude_agent_sdk as sdk
    request = json.load(sys.stdin)
    sid = request['sessionId']
    if request['kind'] == 'fork':
        result = sdk.fork_session(sid, up_to_message_id=request.get('upToMessageId'), title=request.get('title'))
        print(dump({'sessionId': result.session_id}))
    elif request['kind'] == 'delete':
        sdk.delete_session(sid)
        print('{}')
    else:
        raise ValueError('unknown native session mutation')


if __name__ == '__main__' and sys.argv[1:] == ['--native']:
    _native_main()
