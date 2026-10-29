"""Claude P0 (2026-10-17): fast mode as the Codex Fast service tier, and the
automatic re-send after a transient OAuth refresh-lock failure."""
import json

from cute_claude import subscription
from cute_claude.model_catalog import FAST_TIER, fast_mode_supported
from test_persistent import ph  # noqa: F401  (fixture)

REFRESH_TEXT = ('Failed to refresh OAuth token: another Claude Code process is refreshing it or exited mid-refresh. '
                'This is usually transient; retry in a minute')


def test_fast_mode_models_follow_the_cli_rule():
    for model in ('claude-opus-4-8', 'claude-opus-5', 'claude-opus-5-5', 'opus', 'opus[1m]', 'best'):
        assert fast_mode_supported(model), model
    for model in ('claude-opus-4-7', 'claude-sonnet-5-5', 'sonnet', 'fable', 'haiku', None):
        assert not fast_mode_supported(model), model


async def test_model_list_offers_the_fast_tier_only_where_claude_code_has_it(ph):
    models = {m['model']: m for m in (await ph.call('model/list'))['data']}
    assert models['opus']['serviceTiers'] == [{'id': FAST_TIER, 'name': 'Fast', 'description': models['opus']['serviceTiers'][0]['description']}]
    assert models['sonnet']['serviceTiers'] == []
    features = (await ph.call('claude/capabilities'))['features']
    assert features['fastMode'] is True


# The harness profile has command execution off: settings hooks stay off too.
EXEC_OFF = {'disableAllHooks': True}


async def test_fast_tier_reconnects_with_the_flag_setting_and_survives_restart(ph):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    first = ph.sdk.clients[-1]
    assert json.loads(first.options.settings) == EXEC_OFF
    await ph.call('thread/settings/update', {'threadId': tid, 'model': 'opus'})
    # Fast on a model without fast mode is refused.
    await ph.call('thread/settings/update', {'threadId': tid, 'model': 'sonnet', 'serviceTier': FAST_TIER}, error=True)
    original = ph.engine.runtime_status(tid)
    await ph.call('thread/settings/update', {'threadId': tid, 'serviceTier': FAST_TIER})
    after = ph.engine.runtime_status(tid)
    assert after['sessionId'] == original['sessionId'] and after['processId'] != original['processId']
    assert json.loads(ph.sdk.clients[-1].options.settings) == {'fastMode': True, **EXEC_OFF}
    resumed = await ph.call('thread/resume', {'threadId': tid})
    assert resumed['serviceTier'] == FAST_TIER
    settings = [m for m in ph.out if m.get('method') == 'thread/settings/updated']
    assert settings[-1]['params']['threadSettings']['serviceTier'] == FAST_TIER
    # Reapplying is a no-op; a turn may carry the current tier.
    await ph.call('thread/settings/update', {'threadId': tid, 'serviceTier': FAST_TIER})
    assert ph.engine.runtime_status(tid)['processId'] == after['processId']
    await ph.call('claude/runtime/close', {'threadId': tid})
    await ph.call('claude/runtime/start', {'threadId': tid})
    assert json.loads(ph.sdk.clients[-1].options.settings) == {'fastMode': True, **EXEC_OFF}
    # Off (the desktop sends the explicit standard tier).
    await ph.call('thread/settings/update', {'threadId': tid, 'serviceTier': 'default'})
    assert json.loads(ph.sdk.clients[-1].options.settings) == EXEC_OFF
    assert (await ph.call('thread/resume', {'threadId': tid}))['serviceTier'] is None


async def test_switching_to_a_model_without_fast_mode_turns_it_off(ph):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    await ph.call('thread/settings/update', {'threadId': tid, 'model': 'opus', 'serviceTier': FAST_TIER})
    assert ph.engine.thread_fast(tid)
    await ph.call('thread/settings/update', {'threadId': tid, 'model': 'sonnet'})
    assert not ph.engine.thread_fast(tid)
    assert json.loads(ph.sdk.clients[-1].options.settings) == EXEC_OFF


async def test_fast_mode_alone_is_saved_while_the_runtime_is_offline(ph):
    tid = await ph.thread()
    ph.engine.save_model_effort(tid, 'opus', ph.engine.thread_effort(tid))
    await ph.call('thread/settings/update', {'threadId': tid, 'serviceTier': FAST_TIER})
    assert ph.engine.thread_fast(tid)
    await ph.call('claude/runtime/start', {'threadId': tid})
    assert json.loads(ph.sdk.clients[-1].options.settings) == {'fastMode': True, **EXEC_OFF}


def _refresh_then(client, ph, tid, failures):
    """Fake SDK query: `failures` refresh-lock failures, then a normal reply."""
    async def query(prompt):
        client.query_count += 1
        sid = ph.engine.session(tid)['session_id']
        if client.query_count <= failures:
            await client.events.put({'type': 'assistant', 'error': 'server_error',
                                     'content': [{'type': 'text', 'text': REFRESH_TEXT}]})
            await client.events.put({'type': 'result', 'session_id': sid, 'subtype': 'error_during_execution',
                                     'is_error': True, 'num_turns': 1, 'usage': {}})
            return
        await client.events.put({'type': 'assistant', 'session_id': sid, 'model': 'claude-sonnet-5-5',
                                 'content': [{'type': 'text', 'text': 'retried answer'}]})
        await client.events.put({'type': 'result', 'session_id': sid, 'subtype': 'success', 'is_error': False,
                                 'num_turns': 1, 'usage': {'input_tokens': 3, 'output_tokens': 2}})
    return query


async def test_refresh_lock_failure_is_retried_in_the_same_turn(ph, monkeypatch):
    monkeypatch.setattr(subscription, 'REFRESH_RETRY_DELAYS', (0.01, 0.01))
    monkeypatch.delenv('CUTE_CLAUDE_REFRESH_RETRY_DELAYS', raising=False)
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    client = ph.sdk.clients[0]
    client.query = _refresh_then(client, ph, tid, failures=1)
    turn = await ph.turn(tid, 'first message after a long idle')
    await ph.drain()
    result = ph.store.turn(turn)
    assert result['status'] == 'completed', result
    assert client.query_count == 2 and len(ph.sdk.clients) == 1
    retries = [m['params'] for m in ph.out if m.get('method') == 'error']
    assert len(retries) == 1 and retries[0]['willRetry'] is True and retries[0]['turnId'] == turn
    assert 'retrying' in retries[0]['error']['message']
    assert 'retried answer' in json.dumps(ph.store.items(turn))


async def test_refresh_lock_retries_are_bounded_and_reported(ph, monkeypatch):
    monkeypatch.setenv('CUTE_CLAUDE_REFRESH_RETRY_DELAYS', '0.01,0.01')
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    client = ph.sdk.clients[0]
    client.query = _refresh_then(client, ph, tid, failures=5)
    turn = await ph.turn(tid, 'still locked')
    await ph.drain()
    result = ph.store.turn(turn)
    assert result['status'] == 'failed' and client.query_count == 3
    assert 'sent again 2 time(s)' in result['error']['message']


async def test_refresh_lock_retry_can_be_disabled(ph, monkeypatch):
    monkeypatch.setenv('CUTE_CLAUDE_REFRESH_RETRY_DELAYS', '')
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    client = ph.sdk.clients[0]
    client.query = _refresh_then(client, ph, tid, failures=1)
    turn = await ph.turn(tid, 'no retry')
    await ph.drain()
    assert ph.store.turn(turn)['status'] == 'failed' and client.query_count == 1
    assert not [m for m in ph.out if m.get('method') == 'error']


async def test_turn_usage_reports_the_sdk_steps_as_model_requests(ph):
    """Token audit 2026-10-22: cutex counts a Claude turn's model requests (SDK steps) to show
    tokens per step; the turn's final usage update carries them as `modelRequests`."""
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    client = ph.sdk.clients[0]

    async def query(prompt):
        sid = ph.engine.session(tid)['session_id']
        await client.events.put({'type': 'assistant', 'session_id': sid, 'model': 'claude-sonnet-5-5',
                                 'content': [{'type': 'text', 'text': 'done in seven steps'}]})
        await client.events.put({'type': 'result', 'session_id': sid, 'subtype': 'success', 'is_error': False,
                                 'num_turns': 7, 'usage': {'input_tokens': 3, 'cache_read_input_tokens': 700, 'output_tokens': 2}})
    client.query = query
    turn = await ph.turn(tid, 'work in several steps')
    await ph.drain()
    assert ph.store.turn(turn)['status'] == 'completed'
    usage = [m['params']['tokenUsage'] for m in ph.out
             if m.get('method') == 'thread/tokenUsage/updated' and m['params'].get('turnId') == turn][-1]
    assert usage['modelRequests'] == 7
    assert usage['total']['cachedInputTokens'] == 700
