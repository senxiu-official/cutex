"""Offline model evidence tests: fixture processes and private SDK transcripts only."""
from pathlib import Path

import pytest

from cute_claude.model_observations import MAX_MODELS, ModelObservations, SOURCE, model_identifier
from cute_claude.store import Store, dump, identifier
from test_persistent import ph, until
from test_r8_sessions import native


async def hang(ph, tid):
    turn = await ph.turn(tid, 'HANG')
    await until(lambda: tid in ph.engine.driver.owners and
                ph.engine.driver.owners[tid].active is not None and
                ph.engine.driver.owners[tid].active.turn_id == turn)
    return turn, ph.engine.driver.owners[tid], ph.sdk.clients[-1]


async def deliver(ph, client, *messages):
    marker = 'probe_' + identifier()
    for message in messages:
        await client.events.put(message)
    await client.events.put({'type': marker})
    await until(lambda: any(e.get('method') == 'claude/runtime/event' and
                e['params']['kind'] == marker for e in ph.out))


async def complete(ph, owner, client, **extra):
    await client.events.put({'type': 'result', 'session_id': owner.session_id,
        'subtype': 'success', 'is_error': False, 'terminal_reason': 'completed',
        'num_turns': 1, 'usage': {'input_tokens': 2, 'output_tokens': 3}, **extra})
    await ph.drain()


def assistant(model, **extra):
    # The public SDK AssistantMessage does not require a session_id.
    return {'type': 'assistant', 'model': model, 'content': [], **extra}


def observed_events(ph):
    return [e['params'] for e in ph.out if e.get('method') == 'claude/model/observed']


async def test_alias_records_only_parent_assistant_evidence_and_fallback_changes(ph):
    tid = await ph.thread()
    initial = ph.engine.model_status(tid)
    assert initial['requestedModel'] == 'sonnet' and initial['lastObservedModel'] is None
    assert not ph.sdk.clients  # A status read does not connect or infer.
    turn, owner, client = await hang(ph, tid)
    await deliver(ph, client,
        {'type': 'system', 'subtype': 'init', 'data': {'session_id': owner.session_id,
            'model': 'claude-opus-5-5'}},
        {'type': 'stream_event', 'event': {'type': 'message_start',
            'message': {'model': 'claude-haiku-5-5'}}})
    assert ph.engine.model_status(tid)['lastObservedModel'] is None
    await deliver(ph, client, assistant('claude-sonnet-5-5'), assistant('claude-sonnet-5-5'))
    first = ph.engine.model_status(tid)
    assert first['requestedModel'] == first['observedForRequestedModel'] == 'sonnet'
    assert first['observedModels'] == ['claude-sonnet-5-5']
    assert first['lastObservedModel'] == 'claude-sonnet-5-5'
    assert first['source'] == SOURCE and first['turnId'] == turn
    assert first['current'] and first['freshness'] == 'activeTurn'
    assert first['observedAtMs'] > 0 and not first['appliesToNextTurn']
    assert len(observed_events(ph)) == 1
    await deliver(ph, client, assistant('claude-haiku-5-5'), assistant('claude-sonnet-5-5'))
    assert ph.engine.model_status(tid)['observedModels'] == ['claude-sonnet-5-5', 'claude-haiku-5-5']
    assert ph.engine.model_status(tid)['lastObservedModel'] == 'claude-sonnet-5-5'
    assert len(observed_events(ph)) == 3
    await complete(ph, owner, client, model_usage={'claude-opus-5-5': {'inputTokens': 8},
                                                'claude-haiku-5-5': {'inputTokens': 1}})
    latest = ph.engine.model_status(tid)
    assert latest['freshness'] == 'latestTurn' and latest['current']
    assert latest['turnStatus'] == 'completed' and not latest['appliesToNextTurn']
    assert latest['lastObservedModel'] == 'claude-sonnet-5-5'
    runtime = ph.engine.runtime_status(tid)
    assert runtime['model'] == 'sonnet' and runtime['modelObservation'] == latest


async def test_child_background_idle_and_invalid_identifiers_are_never_evidence(ph):
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    client = ph.sdk.clients[-1]
    await deliver(ph, client, assistant('claude-opus-5-5'))
    assert ph.engine.model_status(tid)['lastObservedModel'] is None
    turn, owner, client = await hang(ph, tid)
    await deliver(ph, client,
        assistant('claude-opus-5-5', parent_tool_use_id='child-task'),
        assistant('claude-opus-5-5', agent_id='unbound-child'),
        assistant('claude-opus-5-5', origin={'kind': 'task-notification'}),
        assistant(None), assistant({'secret': 'do-not-retain'}),
        assistant('sk-ant-do-not-retain'), assistant('claude-sonnet\nsecret'),
        assistant('claude-' + 'x' * 500))
    await ph.engine.observe_model(tid, turn, assistant('claude-opus-5-5', session_id='old-session'))
    await ph.engine.observe_model(tid, 'old-turn', assistant('claude-opus-5-5'))
    assert ph.engine.model_status(tid)['lastObservedModel'] is None
    assert observed_events(ph) == []
    await complete(ph, owner, client, model_usage={'claude-sonnet-5-5': {'inputTokens': 2}})
    assert ph.engine.model_status(tid)['lastObservedModel'] is None
    assert ph.store.db.execute('SELECT count(*) FROM model_observations').fetchone()[0] == 0
    assert 'do-not-retain' not in dump(ph.store.events(tid, 0, 200))


async def test_new_turn_and_model_roundtrip_do_not_reconfirm_old_evidence(ph):
    tid = await ph.thread()
    first, owner, client = await hang(ph, tid)
    await deliver(ph, client, assistant('claude-sonnet-5-5'))
    await complete(ph, owner, client)
    await ph.engine.set_model(tid, 'opus')
    status = ph.engine.model_status(tid)
    assert status['requestedModel'] == 'opus' and status['observedForRequestedModel'] == 'sonnet'
    assert not status['current'] and status['staleReason'] == 'requestedModelChanged'
    await ph.engine.set_model(tid, 'sonnet')
    assert ph.engine.model_status(tid)['staleReason'] == 'modelSelectionChanged'
    second, owner, client = await hang(ph, tid)
    assert not ph.engine.model_status(tid)['current']
    await deliver(ph, client, assistant('claude-sonnet-5-5'))
    assert ph.engine.model_status(tid)['turnId'] == second
    assert ph.engine.model_status(tid)['current']
    assert [e['turnId'] for e in observed_events(ph)] == [first, second]
    await complete(ph, owner, client)
    third, owner, client = await hang(ph, tid)
    assert ph.engine.model_status(tid)['turnId'] == second
    assert ph.engine.model_status(tid)['staleReason'] == 'newerTurnUnobserved'
    await complete(ph, owner, client)
    assert not ph.engine.model_status(tid)['current']
    assert ph.store.turn(third)['status'] == 'completed'


async def test_restart_recovery_and_effort_reconnect_preserve_only_historical_evidence(ph):
    tid = await ph.thread()
    _, owner, client = await hang(ph, tid)
    await deliver(ph, client, assistant('claude-sonnet-5-5'))
    await complete(ph, owner, client)
    await ph.engine.set_model_effort(tid, 'sonnet', 'high')
    assert ph.engine.model_status(tid)['staleReason'] == 'connectionChanged'
    assert ph.sdk.clients[-1].query_count == 0
    _, owner, client = await hang(ph, tid)
    await deliver(ph, client, assistant('claude-sonnet-5-5'))
    await complete(ph, owner, client)
    generation = owner.native_connection_generation
    client._transport._process.kill()
    await until(lambda: owner.native_connection_generation == generation + 1)
    assert ph.engine.model_status(tid)['staleReason'] == 'connectionChanged'
    assert ph.sdk.clients[-1].query_count == 0
    await ph.engine.close_runtime(tid)
    assert ph.engine.model_status(tid)['staleReason'] == 'runtimeOffline'
    before_queries = sum(c.query_count for c in ph.sdk.clients)
    await ph.engine.start_runtime(tid)
    status = ph.engine.model_status(tid)
    assert status['lastObservedModel'] == 'claude-sonnet-5-5'
    assert status['staleReason'] == 'connectionChanged' and not status['current']
    assert sum(c.query_count for c in ph.sdk.clients) == before_queries


async def test_bounded_history_never_discards_the_latest_model(ph):
    tid = await ph.thread()
    turn, owner, client = await hang(ph, tid)
    models = [f'claude-fixture-{n}' for n in range(MAX_MODELS + 2)]
    await deliver(ph, client, *(assistant(model) for model in models))
    status = ph.engine.model_status(tid)
    assert status['lastObservedModel'] == models[-1] and status['current']
    assert status['observedModels'] == models[-MAX_MODELS:]
    assert status['observedModelsTruncated'] is True
    assert len(observed_events(ph)) == len(models)
    await complete(ph, owner, client)


def seed_observation(store, tid, turn, model):
    # The offline transcript fixture is already completed. Reconstruct the
    # evidence write that would have preceded its terminal Result.
    original = store.db.execute('SELECT state FROM turns WHERE id=?', (turn,)).fetchone()[0]
    store.db.execute("UPDATE turns SET state='inProgress' WHERE id=?", (turn,))
    observations = ModelObservations(store)
    assert observations.record(tid, turn, model, requested_model=store.thread(tid)['model'],
        connection_id='fixture-connection', connection_generation=1, selection_id='fixture-selection')
    store.db.execute('UPDATE turns SET state=? WHERE id=?', (original, turn))
    return observations


def test_persisted_evidence_reopens_without_runtime_confirmation(tmp_path):
    from test_r8_sessions import add_turn
    store = Store(tmp_path / 'state')
    tid = store.new_thread('sonnet', str(tmp_path))
    turn, _ = add_turn(store, tid, 'fixture')
    observations = seed_observation(store, tid, turn, 'claude-sonnet-5-5')
    old = observations.status(tid)
    schema_before = dict(store.db.execute('SELECT * FROM meta').fetchall())
    store.close()
    reopened = Store(tmp_path / 'state')
    try:
        status = ModelObservations(reopened).status(tid)
        assert status == old
        assert status['freshness'] == 'historical' and status['staleReason'] == 'runtimeOffline'
        assert status['lastObservedModel'] == 'claude-sonnet-5-5' and not status['current']
        assert dict(reopened.db.execute('SELECT * FROM meta').fetchall()) == schema_before
    finally:
        reopened.close()


async def test_revert_filters_future_observations_and_delete_cleans_extensions(native):
    n = native
    observations = seed_observation(n.store, n.tid, n.turns[0], 'claude-sonnet-5-5')
    seed_observation(n.store, n.tid, n.turns[1], 'claude-opus-5-5')
    assert observations.status(n.tid)['lastObservedModel'] == 'claude-opus-5-5'
    await n.ops.revert(n.tid, n.turns[1])
    status = observations.status(n.tid)
    assert status['turnId'] == n.turns[0] and status['lastObservedModel'] == 'claude-sonnet-5-5'
    assert n.store.db.execute('SELECT count(*) FROM model_observations').fetchone()[0] == 2
    assert n.store.db.execute('PRAGMA foreign_keys').fetchone()[0] == 1
    await n.ops.delete(n.tid)
    assert n.store.db.execute('SELECT count(*) FROM model_observations').fetchone()[0] == 0


async def test_fork_does_not_copy_source_observations(native, tmp_path):
    n = native
    seed_observation(n.store, n.tid, n.turns[0], 'claude-sonnet-5-5')
    seed_observation(n.store, n.tid, n.turns[1], 'claude-opus-5-5')
    child = identifier()
    result = await n.ops.fork(n.tid, {'targetThreadId': child,
        'targetStateDir': str(tmp_path / 'forked'), 'lastTurnId': n.turns[0]})
    forked = Store(Path(result['stateDir']))
    try:
        status = ModelObservations(forked).status(child)
        assert status['freshness'] == 'unobserved' and status['lastObservedModel'] is None
        assert status['observedModels'] == [] and not status['current']
    finally:
        forked.close()


@pytest.mark.parametrize('value', [None, 12, {}, [], 'future-model', 'sk-ant-key',
    'https://secret.example/model', 'C:\\secret', 'claude-opus 5', 'claude-opus\nkey',
    'claude-opus-5-5?token=secret', 'claude-' + 'x' * 200])
def test_unknown_model_identifiers_are_not_retained(value):
    assert model_identifier(value) is None


@pytest.mark.parametrize('value', ['claude-sonnet-5-5', 'claude-fable-5-1',
    'claude-3-7-sonnet-20250219', 'us.anthropic.claude-sonnet-4-5-20250929-v1:0',
    'claude-sonnet-4-5@20250929', 'sonnet'])
def test_bounded_model_identifier_forms(value):
    assert model_identifier(value) == value


async def test_effort_unknown_stays_distinct_from_explicit_empty(ph, monkeypatch):
    async def info(self):
        return {'models': [{'value': 'sonnet', 'displayName': 'Sonnet'},
                           {'value': 'haiku', 'displayName': 'Haiku', 'supportedEffortLevels': []}]}
    monkeypatch.setattr(ph.sdk.ClaudeSDKClient, 'get_server_info', info)
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    owner = ph.engine.driver.owners[tid]
    assert 'sonnet' not in owner.model_efforts and owner.model_efforts['haiku'] == []
