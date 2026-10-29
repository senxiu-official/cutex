"""Session mutations use real offline SDK transcripts; never login or infer."""
import json
from pathlib import Path
from types import SimpleNamespace
import sqlite3

import pytest

from cute_claude.errors import RpcError
from cute_claude.history import History, thread_wire
from cute_claude.history_reader import Reader
from cute_claude.session_ops import NativeSessionApi, SessionOperations
from cute_claude.store import Store, dump, identifier


def add_turn(store, tid, prompt, answer='answer'):
    receipt = store.enqueue(tid, identifier(), {'source': {'kind': 'user', 'id': 'owner'},
        'eventType': 'message', 'text': prompt}, 20)
    record = store.claim(tid)
    turn = record['turn_id']
    store.put_item(turn, {'id': identifier(), 'type': 'agentMessage', 'phase': 'final_answer', 'text': answer})
    store.finish(tid, turn, 'completed')
    return turn, receipt['messageId']


@pytest.fixture
def native(tmp_path):
    pytest.importorskip('claude_agent_sdk')
    store = Store(tmp_path / 'state')
    store.bind_backend('subscription')
    tid = store.new_thread('claude-test', str(tmp_path))
    turn1, mid1 = add_turn(store, tid, 'first prompt', 'first answer')
    turn2, mid2 = add_turn(store, tid, 'second prompt', 'second answer')
    sid = identifier()
    store.db.execute('CREATE TABLE sdk_sessions(thread_id TEXT PRIMARY KEY REFERENCES threads(id), session_id TEXT NOT NULL UNIQUE, attempted INTEGER DEFAULT 0,last_status TEXT DEFAULT \'allocated\')')
    store.db.execute('INSERT INTO sdk_sessions VALUES(?,?,1,\'completed\')', (tid, sid))
    config = tmp_path / 'claude-home'
    project = config / 'projects' / 'fixture'
    project.mkdir(parents=True)
    entries = []
    parent = None
    # Last entry retained before turn2 is a user tool result, not an assistant.
    # This catches implementations that silently drop the kept turn's tail.
    for turn, typ, content in [(turn1, 'user', 'first prompt'), (turn1, 'assistant', 'first answer'),
                               (turn1, 'user', [{'type': 'tool_result', 'tool_use_id': 'end-turn', 'content': 'done'}]),
                               (turn2, 'user', 'second prompt'), (turn2, 'assistant', 'second answer')]:
        native_id = identifier()
        entries.append({'uuid': native_id, 'parentUuid': parent, 'sessionId': sid, 'type': typ,
            'cwd': str(tmp_path), 'timestamp': '2026-10-10T00:00:00Z', 'message': {'role': typ, 'content': content}})
        if isinstance(content, str):
            store.record_sdk_message(tid, turn, native_id, typ)
        parent = native_id
    source_path = project / f'{sid}.jsonl'
    source_path.write_text('\n'.join(dump(e) for e in entries) + '\n', encoding='utf-8')
    driver = SimpleNamespace(config_dir=config, persistent=False, owners={})
    engine = SimpleNamespace(store=store, driver=driver, offline_threads=set(),
        session=lambda thread: dict(store.db.execute('SELECT * FROM sdk_sessions WHERE thread_id=?', (thread,)).fetchone()),
        runtime_status=lambda thread: {'threadId': thread, 'state': 'offline'})
    api = NativeSessionApi(engine)
    ops = SessionOperations(engine, api)
    yield SimpleNamespace(store=store, tid=tid, sid=sid, turns=[turn1, turn2], mids=[mid1, mid2],
        path=source_path, entries=entries, engine=engine, ops=ops, api=api)
    store.close()


async def test_revert_preserves_native_tail_and_audit_without_replay(native):
    n = native
    original = n.path.read_bytes()
    result = await n.ops.revert(n.tid, n.turns[1])
    assert result['removedTurnIds'] == [n.turns[1]]
    assert result['nativeSessionId'] != n.sid
    assert n.path.read_bytes() == original
    assert Path(result['backupPath']) == n.path
    assert json.loads(Path(result['receiptPath']).read_text())['state'] == 'completed'
    copied = [e for e in n.api.entries(result['nativeSessionId']) if 'message' in e]
    assert len(copied) == 3
    assert copied[-1]['message']['content'][0]['type'] == 'tool_result'
    assert n.store.db.execute('SELECT count(*) FROM turns').fetchone()[0] == 2
    assert [t['id'] for t in History(n.store).full_read(n.tid)] == [n.turns[0]]
    assert n.store.input_status(n.tid, n.mids[1])['state'] == 'reverted'
    assert n.store.claim(n.tid) is None
    assert n.store.db.execute('SELECT count(*) FROM history').fetchone()[0] == 0
    # Offline readers observe exactly the same selected history.
    reader = Reader(n.store.directory)
    try:
        assert [t['id'] for t in reader.handle('thread/read', {'threadId': n.tid, 'includeTurns': True})['thread']['turns']] == [n.turns[0]]
    finally:
        reader.db.close()


async def test_first_turn_revert_resets_to_unattempted_session(native):
    result = await native.ops.revert(native.tid, native.turns[0])
    assert native.engine.session(native.tid)['attempted'] == 0
    assert result['removedTurnIds'] == native.turns
    assert native.api.transcript(result['nativeSessionId']) is None
    assert native.path.exists()
    assert History(native.store).full_read(native.tid) == []


async def test_sdk_fork_exports_private_projection_with_inert_sources(native, tmp_path):
    from cute_claude.external_input import ExternalInputService
    native.store.db.execute('CREATE TABLE thread_plan_mode(thread_id TEXT PRIMARY KEY REFERENCES threads(id), enabled INTEGER NOT NULL)')
    native.store.db.execute('INSERT INTO thread_plan_mode VALUES(?,1)', (native.tid,))
    ExternalInputService(SimpleNamespace(store=native.store,
        settings=SimpleNamespace(external_owner_id='source-owner', external_runtime_generation=42)))
    new_tid = identifier()
    result = await native.ops.fork(native.tid, {'targetThreadId': new_tid, 'targetStateDir': str(tmp_path / 'forked'), 'lastTurnId': native.turns[0]})
    forked = Store(Path(result['stateDir']))
    try:
        assert result['threadId'] == new_tid
        assert forked.thread_properties(new_tid)['forkedFromId'] == native.tid
        assert len(History(forked).full_read(new_tid)) == 1
        assert forked.claim(new_tid) is None
        assert not forked.pending(new_tid)
        assert forked.db.execute('SELECT state FROM inputs').fetchone()[0] == 'inherited'
        usage = json.loads(forked.db.execute('SELECT usage FROM turns').fetchone()[0])
        assert usage['calls'] == [] and usage['inheritedFrom']['turnId'] == native.turns[0]
        assert forked.db.execute('SELECT count(*) FROM tool_calls').fetchone()[0] == 0
        assert forked.db.execute('SELECT count(*) FROM sdk_turn_messages').fetchone()[0] == 2
        assert forked.db.execute('SELECT enabled FROM thread_plan_mode WHERE thread_id=?', (new_tid,)).fetchone()[0] == 1
        assert forked.db.execute("SELECT value FROM meta WHERE key='external_input_binding'").fetchone() is None
        # A child owner starts its own occurrence. Copying the source binding
        # would reject both this different owner and its lower generation.
        child_inputs = ExternalInputService(SimpleNamespace(store=forked,
            settings=SimpleNamespace(external_owner_id='child-owner', external_runtime_generation=1)))
        assert child_inputs.enabled
        assert json.loads(forked.db.execute("SELECT value FROM meta WHERE key='external_input_binding'").fetchone()[0]) == {
            'ownerId': 'child-owner', 'runtimeGeneration': 1}
        assert json.loads(native.store.db.execute("SELECT value FROM meta WHERE key='external_input_binding'").fetchone()[0]) == {
            'ownerId': 'source-owner', 'runtimeGeneration': 42}
        assert native.store.db.execute('SELECT count(*) FROM threads').fetchone()[0] == 1
        assert len([e for e in native.api.entries(result['nativeSessionId']) if 'message' in e]) == 3
    finally:
        forked.close()


async def test_structural_change_rejects_pending_and_held_native_lease(native):
    from cute_claude.sdk_runtime import NativeSessionLease
    from cute_claude.provider import ProviderError
    lease = NativeSessionLease(native.api.config_dir, native.sid)
    try:
        with pytest.raises(ProviderError):
            await native.ops.fork(native.tid)
    finally:
        lease.close()
    assert len(History(native.store).full_read(native.tid)) == 2
    native.store.enqueue_note(native.tid, identifier(), {'source': {'kind': 'agent', 'id': 'peer'}, 'eventType': 'message', 'text': 'pending'}, 20)
    with pytest.raises(RpcError, match='queued'):
        await native.ops.revert(native.tid, native.turns[0])


async def test_failed_rewind_does_not_change_conversation(native):
    async def failed(*args, **kwargs):
        raise RpcError(-32009, 'checkpoint unavailable')
    native.engine.runtime_control = failed
    with pytest.raises(RpcError, match='checkpoint unavailable'):
        await native.ops.revert(native.tid, native.turns[1], rewind_files=True)
    assert native.engine.session(native.tid)['session_id'] == native.sid
    assert len(History(native.store).full_read(native.tid)) == 2


async def test_delete_uses_sdk_and_cleans_extension_rows(native):
    native.store.db.execute('CREATE TABLE external_inputs(thread_id TEXT, payload TEXT)')
    native.store.db.execute('INSERT INTO external_inputs VALUES(?,?)', (native.tid, '{}'))
    await native.ops.delete(native.tid)
    assert not native.path.exists()
    assert native.store.db.execute('SELECT count(*) FROM external_inputs').fetchone()[0] == 0
    with pytest.raises(RpcError, match='thread not found'):
        native.store.thread(native.tid)


async def test_late_reconnect_failure_returns_committed_binding(native):
    async def fail_restart(tid):
        raise RuntimeError('private native error must not escape')
    native.engine.start_runtime = fail_restart
    native.engine.driver.owners[native.tid] = SimpleNamespace(task=SimpleNamespace(done=lambda: False))
    result = await native.ops.revert(native.tid, native.turns[1])
    assert result['state'] == 'completed'
    assert result['runtime']['state'] == 'failed'
    assert result['runtime']['restartError'] == 'RuntimeError'
    assert result['runtime']['sessionId'] == native.engine.session(native.tid)['session_id']
    assert result['nativeSessionId'] != native.sid
    assert len(History(native.store).full_read(native.tid)) == 1


async def test_online_fork_returns_new_source_connection_and_keeps_plan_mode(native):
    native.store.db.execute('CREATE TABLE thread_plan_mode(thread_id TEXT PRIMARY KEY REFERENCES threads(id), enabled INTEGER NOT NULL)')
    native.store.db.execute('INSERT INTO thread_plan_mode VALUES(?,1)', (native.tid,))
    old_connection, new_connection = identifier(), identifier()
    lifecycle = []
    state = {'closed': False}
    native.engine.driver.persistent = True
    native.engine.driver.owners[native.tid] = SimpleNamespace(
        connection_id=old_connection, task=SimpleNamespace(done=lambda: state['closed']), lease=None)
    async def close_runtime(tid):
        assert tid == native.tid
        lifecycle.append('close')
        state['closed'] = True
        native.engine.offline_threads.add(tid)
    async def start_runtime(tid):
        assert tid == native.tid and state['closed']
        assert native.engine.session(tid)['session_id'] == native.sid
        lifecycle.append('connect')
        native.engine.offline_threads.discard(tid)
        return {'threadId': tid, 'sessionId': native.sid, 'connectionId': new_connection, 'state': 'ready'}
    native.engine.close_runtime = close_runtime
    native.engine.start_runtime = start_runtime
    receipt = await native.ops.fork(native.tid)
    assert lifecycle == ['close', 'connect']
    assert receipt['sourceRuntime']['connectionId'] == new_connection != old_connection
    assert receipt['sourceRuntime']['sessionId'] == native.sid != receipt['nativeSessionId']
    assert native.tid not in native.engine.offline_threads
    assert json.loads(Path(receipt['receiptPath']).read_text())['sourceRuntime']['connectionId'] == new_connection
    assert native.store.db.execute('SELECT enabled FROM thread_plan_mode WHERE thread_id=?', (receipt['threadId'],)).fetchone()[0] == 1


async def test_ambiguous_r7_checkpoint_fails_without_native_mutation(native):
    native.store.db.execute('DELETE FROM sdk_turn_messages')
    duplicate = {**native.entries[3], 'uuid': identifier()}
    with native.path.open('a', encoding='utf-8') as transcript:
        transcript.write(dump(duplicate) + '\n')
    original = native.path.read_bytes()
    with pytest.raises(RpcError, match='unambiguous'):
        await native.ops.revert(native.tid, native.turns[1])
    assert native.path.read_bytes() == original
    assert native.engine.session(native.tid)['session_id'] == native.sid
    assert native.store.claim(native.tid) is None
    assert len(History(native.store).full_read(native.tid)) == 2


async def test_fork_projection_failure_is_journaled_without_copying_inputs(native, tmp_path, monkeypatch):
    def fail(*args, **kwargs):
        raise RuntimeError('projection failed')
    monkeypatch.setattr(native.ops, '_copy_projection', fail)
    destination = tmp_path / 'failed-fork'
    with pytest.raises(RuntimeError, match='projection failed'):
        await native.ops.fork(native.tid, {'targetStateDir': str(destination)})
    row = native.store.db.execute('SELECT state,data FROM session_operations ORDER BY rowid DESC LIMIT 1').fetchone()
    receipt = json.loads(row['data'])
    assert row['state'] == 'failed'
    assert receipt['nativeSessionId'] != native.sid
    assert receipt['stateDir'] == str(destination)
    assert native.store.claim(native.tid) is None
    failed = Store(destination)
    try:
        assert failed.db.execute('SELECT count(*) FROM threads').fetchone()[0] == 0
    finally:
        failed.close()


async def test_delete_removes_revert_native_backups(native):
    reverted = await native.ops.revert(native.tid, native.turns[1])
    current = native.api.transcript(reverted['nativeSessionId'])
    await native.ops.delete(native.tid)
    assert not native.path.exists() and not current.exists()


async def test_native_rpc_session_shapes_and_empty_prefix_fork(native):
    from cute_claude.r8_protocol import R8Protocol
    notifications = []
    async def emit(tid, method, params):
        notifications.append((method, params))
    native.engine.emit = emit
    peer = SimpleNamespace(subscriptions={native.tid})
    wire = lambda tid, include_turns=False: thread_wire(native.store, History(native.store), tid, include_turns=include_turns)
    server = SimpleNamespace(engine=native.engine, store=native.store, settings=SimpleNamespace(backend='subscription'),
        _thread_wire=wire, _thread_response=lambda tid, turns=False: {'thread': wire(tid, turns)}, peers=[peer])
    rpc = R8Protocol(server)
    rpc.sessions = native.ops
    metadata, _ = await rpc.dispatch(peer, 'thread/metadata/update', {'threadId': native.tid, 'gitInfo': {'branch': 'r8'}})
    assert metadata['thread']['gitInfo']['branch'] == 'r8'
    fork, notify = await rpc.dispatch(peer, 'thread/fork', {'threadId': native.tid,
        'beforeTurnId': native.turns[0], 'excludeTurns': True, 'name': 'Empty branch'})
    assert fork['thread']['name'] == 'Empty branch'
    assert fork['thread']['turns'] == [] and fork['forkedThroughTurnId'] is None
    assert native.engine.session(fork['threadId'])['attempted'] == 0
    await notify()
    reverted, notify = await rpc.dispatch(peer, 'thread/revert', {'threadId': native.tid, 'beforeTurnId': native.turns[1]})
    assert reverted['thread']['turns'] == []
    assert 'turnsBackwardsCursor' in reverted and 'itemsBackwardsCursor' in reverted
    await notify()
    assert notifications[-1][0] == 'thread/reverted'
    assert notifications[-1][1]['threadId'] == native.tid


def test_r7_database_read_then_additive_migration(tmp_path):
    directory = tmp_path / 'old-state'
    store = Store(directory)
    tid = store.new_thread('test', str(tmp_path))
    turn, _ = add_turn(store, tid, 'legacy prompt')
    store.close()
    db = sqlite3.connect(directory / 'state.sqlite3')
    db.execute('ALTER TABLE turns DROP COLUMN reverted')
    for table in ('thread_properties', 'sdk_turn_messages', 'session_operations'):
        db.execute('DROP TABLE ' + table)
    db.execute("UPDATE meta SET value='1' WHERE key='schema'")
    db.commit()
    db.close()
    reader = Reader(directory)
    try:
        assert reader.handle('thread/read', {'threadId': tid, 'includeTurns': True})['thread']['turns'][0]['id'] == turn
    finally:
        reader.db.close()
    migrated = Store(directory)
    try:
        assert migrated.db.execute('SELECT reverted FROM turns').fetchone()[0] == 0
        assert migrated.db.execute("SELECT value FROM meta WHERE key='schema'").fetchone()[0] == '2'
        assert migrated.thread_properties(tid) == {}
        assert migrated.claim(tid) is None
    finally:
        migrated.close()


def test_unknown_schema_is_rejected_before_recovery(tmp_path):
    directory = tmp_path / 'future-state'
    store = Store(directory)
    tid = store.new_thread('test', str(tmp_path))
    store.enqueue(tid, identifier(), {'source': {'kind': 'user', 'id': 'owner'}, 'text': 'never dispatch', 'eventType': 'message'}, 2)
    store.db.execute("UPDATE meta SET value='99' WHERE key='schema'")
    store.close()
    with pytest.raises(RuntimeError, match='unsupported database version'):
        Store(directory)
    db = sqlite3.connect(directory / 'state.sqlite3')
    try:
        assert db.execute("SELECT value FROM meta WHERE key='schema'").fetchone()[0] == '99'
        assert db.execute('SELECT halted FROM threads').fetchone()[0] == 0
        assert db.execute('SELECT state FROM inputs').fetchone()[0] == 'queued'
    finally:
        db.close()


def test_failed_migration_rolls_back_version_and_schema_and_releases_lease(tmp_path, monkeypatch):
    directory = tmp_path / 'migration-failure'
    store = Store(directory)
    store.close()
    original_connect = sqlite3.connect
    db = original_connect(directory / 'state.sqlite3')
    db.execute('ALTER TABLE turns DROP COLUMN reverted')
    db.execute('DROP TABLE sdk_turn_messages')
    db.execute("UPDATE meta SET value='1' WHERE key='schema'")
    db.commit()
    db.close()
    def fail_new_table(*args, **kwargs):
        connection = original_connect(*args, **kwargs)
        connection.set_authorizer(lambda action, name, *rest:
            sqlite3.SQLITE_DENY if action == sqlite3.SQLITE_CREATE_TABLE and name == 'sdk_turn_messages' else sqlite3.SQLITE_OK)
        return connection
    with monkeypatch.context() as patch:
        patch.setattr(sqlite3, 'connect', fail_new_table)
        with pytest.raises(sqlite3.DatabaseError):
            Store(directory)
    db = original_connect(directory / 'state.sqlite3')
    try:
        assert db.execute("SELECT value FROM meta WHERE key='schema'").fetchone()[0] == '1'
        assert 'reverted' not in {r[1] for r in db.execute('PRAGMA table_info(turns)')}
    finally:
        db.close()
    recovered = Store(directory)
    recovered.close()


def test_nested_transactions_and_mutation_admission(tmp_path):
    store = Store(tmp_path / 'state')
    try:
        tid = store.new_thread('test', str(tmp_path))
        with pytest.raises(ValueError):
            with store.transaction():
                add_turn(store, tid, 'rolled back')
                raise ValueError('outer rollback')
        assert store.db.execute('SELECT count(*) FROM inputs').fetchone()[0] == 0
        with store.history_mutation(tid):
            with pytest.raises(RpcError, match='mutation'):
                add_turn(store, tid, 'blocked')
            assert store.claim(tid) is None
    finally:
        store.close()


def test_native_search_occurrence_shapes_unicode_pagination_and_reverted_filter(tmp_path):
    store = Store(tmp_path / 'state')
    try:
        tid = store.new_thread('test', str(tmp_path))
        turn, _ = add_turn(store, tid, '😀 Needle needle', 'final NEEDLE')
        store.put_item(turn, {'type': 'agentMessage', 'id': identifier(), 'phase': 'commentary', 'text': 'needle hidden commentary'})
        ops = SessionOperations(SimpleNamespace(store=store))
        first = ops.search_occurrences(tid, {'searchTerm': 'needle', 'limit': 1})
        assert first['data'][0]['snippetMatchRange'] == {'start': 3, 'end': 9}
        second = ops.search_occurrences(tid, {'searchTerm': 'needle', 'limit': 1, 'cursor': first['nextCursor']})
        assert second['data'][0]['snippetMatchRange'] == {'start': 10, 'end': 16}
        third = ops.search_occurrences(tid, {'searchTerm': 'needle', 'cursor': second['nextCursor']})
        assert len(third['data']) == 1 and third['nextCursor'] is None
        page = History(store).page(tid, {'cursor': first['data'][0]['turnCursor']}, items=False)
        assert page['data'][0]['id'] == turn
        assert ops.search({'searchTerm': 'NEEDLE'})['data'][0]['thread']['id'] == tid
        ops.metadata(tid, {'gitInfo': {'branch': 'topic', 'originUrl': 'https://example.invalid/repo'}})
        ops.metadata(tid, {'gitInfo': {'branch': None}})
        assert thread_wire(store, History(store), tid)['gitInfo'] == {'branch': None, 'sha': None, 'originUrl': 'https://example.invalid/repo'}
        store.db.execute('UPDATE turns SET reverted=1 WHERE id=?', (turn,))
        assert ops.search({'searchTerm': 'needle'})['data'] == []
        assert ops.search_occurrences(tid, {'searchTerm': 'needle'})['data'] == []
    finally:
        store.close()


async def test_busy_fork_is_live_and_leaves_the_running_turn_and_runtime_alone(native, tmp_path):
    """claude-r21 `forkWhileBusy`: a running turn is left out, the source's live
    runtime is neither closed nor leased, its transcript is only read."""
    from cute_claude.sdk_runtime import NativeSessionLease
    n = native
    n.store.enqueue(n.tid, identifier(), {'source': {'kind': 'user', 'id': 'owner'}, 'eventType': 'message', 'text': 'third prompt'}, 20)
    running = n.store.claim(n.tid)['turn_id']
    prompt = {'uuid': identifier(), 'parentUuid': n.entries[-1]['uuid'], 'sessionId': n.sid, 'type': 'user',
              'cwd': str(tmp_path), 'timestamp': '2026-10-10T00:00:01Z', 'message': {'role': 'user', 'content': 'third prompt'}}
    n.store.record_sdk_message(n.tid, running, prompt['uuid'], 'user')
    with n.path.open('a', encoding='utf-8') as transcript:
        transcript.write(dump(prompt) + '\n{"uuid": "still-being-writ')  # Claude Code is mid-append.
    original = n.path.read_bytes()
    closed = []
    async def close_runtime(tid):
        closed.append(tid)
    n.engine.close_runtime = close_runtime
    n.engine.driver.persistent = True
    n.engine.driver.owners[n.tid] = SimpleNamespace(task=SimpleNamespace(done=lambda: False), lease=None)
    lease = NativeSessionLease(n.api.config_dir, n.sid)  # Held by the live owner.
    try:
        result = await n.ops.fork(n.tid, {'targetStateDir': str(tmp_path / 'live-fork')})
        with pytest.raises(RpcError) as refused:
            await n.ops.fork(n.tid, {'lastTurnId': running})
        assert refused.value.data == {'code': 'turn_in_progress', 'turnId': running}
        partial = await n.ops.fork(n.tid, {'lastTurnId': n.turns[0], 'targetStateDir': str(tmp_path / 'first')})
    finally:
        lease.close()
    assert closed == [] and n.path.read_bytes() == original
    assert result['forkedThroughTurnId'] == n.turns[1] and partial['forkedThroughTurnId'] == n.turns[0]
    assert result['sourceRuntime'] is None and result['sourceRuntimeUntouched'] is True
    copied = [e for e in n.api.entries(result['nativeSessionId']) if 'message' in e]
    assert [e['message']['content'] for e in copied][-2:] == ['second prompt', 'second answer']
    assert len(copied) == 5
    assert len([e for e in n.api.entries(partial['nativeSessionId']) if 'message' in e]) == 3
    assert n.store.turn(running)['status'] == 'inProgress'
    forked = Store(Path(result['stateDir']))
    try:
        assert len(History(forked).full_read(result['threadId'])) == 2
        assert forked.claim(result['threadId']) is None
    finally:
        forked.close()


async def test_busy_fork_refuses_a_running_turn_it_cannot_separate(native, tmp_path):
    n = native
    n.store.enqueue(n.tid, identifier(), {'source': {'kind': 'user', 'id': 'owner'}, 'eventType': 'message', 'text': 'third prompt'}, 20)
    n.store.claim(n.tid)
    # On disk but neither observed nor matching the stored text (notes rode along).
    prompt = {'uuid': identifier(), 'parentUuid': n.entries[-1]['uuid'], 'sessionId': n.sid, 'type': 'user',
              'message': {'role': 'user', 'content': [{'type': 'text', 'text': 'note'}, {'type': 'text', 'text': 'third prompt'}]}}
    with n.path.open('a', encoding='utf-8') as transcript:
        transcript.write(dump(prompt) + '\n')
    with pytest.raises(RpcError) as refused:
        await n.ops.fork(n.tid)
    assert refused.value.data['code'] == 'native_transcript_behind'
    assert n.store.db.execute('SELECT count(*) FROM threads').fetchone()[0] == 1


async def test_fork_with_only_queued_notes_keeps_every_turn_without_closing(native):
    n = native
    n.store.enqueue_note(n.tid, identifier(), {'source': {'kind': 'agent', 'id': 'peer'}, 'eventType': 'message', 'text': 'pending'}, 20)
    result = await n.ops.fork(n.tid)
    assert result['forkedThroughTurnId'] == n.turns[1] and result['sourceRuntimeUntouched'] is True
    assert len(History(n.store).full_read(result['threadId'])) == 2
    assert n.store.pending_notes(n.tid) == 1 and n.store.pending_notes(result['threadId']) == 0


@pytest.mark.parametrize('effort', ['low', None])
async def test_profile_fork_uses_target_defaults_not_source_interactive_overrides(native, tmp_path, effort):
    native.store.db.execute('CREATE TABLE IF NOT EXISTS thread_efforts(thread_id TEXT PRIMARY KEY, effort TEXT)')
    native.store.db.execute('CREATE TABLE IF NOT EXISTS thread_launch_models(thread_id TEXT PRIMARY KEY, model TEXT NOT NULL)')
    native.store.db.execute('INSERT OR REPLACE INTO thread_efforts VALUES(?,?)', (native.tid, 'max'))
    native.store.db.execute('INSERT OR REPLACE INTO thread_launch_models VALUES(?,?)', (native.tid, 'old-model'))
    child = identifier()
    result = await native.ops.fork(native.tid, {'targetThreadId': child, 'targetStateDir': str(tmp_path / 'profile-fork'),
        'launchDefaults': {'model': 'target-model', 'effort': effort}})
    forked = Store(Path(result['stateDir']))
    try:
        assert forked.thread(child)['model'] == 'target-model'
        assert forked.db.execute('SELECT model FROM thread_launch_models WHERE thread_id=?', (child,)).fetchone()[0] == 'target-model'
        row = forked.db.execute('SELECT effort FROM thread_efforts WHERE thread_id=?', (child,)).fetchone()
        assert (row[0] if row else None) == effort
        assert native.store.db.execute('SELECT effort FROM thread_efforts WHERE thread_id=?', (native.tid,)).fetchone()[0] == 'max'
    finally:
        forked.close()
