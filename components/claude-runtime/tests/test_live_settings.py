"""Live model / effort / fast mode / per-model auto-compaction (claude-r24, live_settings).

Offline only: the fake SDK stands in for Claude Code (its `apply_flag_settings` merges like 2.1.301's).
"""
import json
from unittest.mock import AsyncMock

import pytest

from cute_claude.live_settings import MODELS_ENV, PERCENT_ENV, WINDOW_ENV, Compaction, live_supported
from test_persistent import ph  # noqa: F401
from test_perms_steer import running_turn


@pytest.mark.parametrize('version, live', [
    ('2.1.301', True), ('2.1.295', True), ('2.2.0', True), ('3.0.0', True),
    ('2.1.294', False), ('2.0.999', False), (None, False), ('', False), ('garbage', False)])
def test_live_needs_a_checked_cli(version, live):
    assert live_supported(version, {}) is live


def test_an_effort_pinned_by_the_environment_keeps_the_restart_path():
    assert live_supported('2.1.301', {'CLAUDE_CODE_EFFORT_LEVEL': 'high'}) is False


def test_compaction_table_falls_back_to_the_launch_values():
    env = {WINDOW_ENV: '533000', PERCENT_ENV: '100',
           MODELS_ENV: json.dumps({'claude-sonnet-5-5': {'window': 233000, 'percent': 100},
                                   'bad-window': {'window': 5, 'percent': 100}, 'bad-shape': 7})}
    c = Compaction(env)
    assert c.models == {'claude-sonnet-5-5': (233000, 100)}, 'invalid entries are dropped'
    assert c.for_model('claude-sonnet-5-5') == (233000, 100)
    assert c.for_model('claude-opus-5-5') == (533000, 100)
    assert c.child_env('claude-opus-5-5') == {}, 'the launch environment already says so'
    assert c.child_env('claude-sonnet-5-5') == {WINDOW_ENV: '233000', PERCENT_ENV: '100'}
    assert c.switch_env('claude-opus-5-5', 'claude-opus-4-8') is None
    assert c.switch_env('claude-opus-5-5', 'claude-sonnet-5-5') == {WINDOW_ENV: '233000', PERCENT_ENV: '100'}
    assert c.switch_env('claude-sonnet-5-5', 'claude-opus-5-5') == {WINDOW_ENV: '533000', PERCENT_ENV: '100'}


def test_compaction_without_launch_values_unsets_on_a_switch():
    c = Compaction({MODELS_ENV: json.dumps({'claude-sonnet-5-5': {'window': 233000, 'percent': 90}})})
    assert c.default is None and c.for_model('claude-opus-5-5') is None
    assert c.child_env('claude-opus-5-5') == {}
    # `""` unsets the override in Claude Code (checked in 2.1.301): Claude Code decides again.
    assert c.switch_env('claude-sonnet-5-5', 'claude-opus-5-5') == {WINDOW_ENV: '', PERCENT_ENV: ''}
    assert Compaction({MODELS_ENV: 'not json'}).models == {}
    assert Compaction({WINDOW_ENV: '533000'}).default is None, 'both values or none'


def flag_requests(client):
    return [request['settings'] for kind, *rest in client.controls if kind == 'control'
            for request in rest if request.get('subtype') == 'apply_flag_settings']


async def test_effort_and_model_change_while_a_turn_runs_go_to_the_running_child(ph, monkeypatch):
    monkeypatch.setattr(ph.server.model_catalog.probe, 'read', AsyncMock(return_value='2.1.301'))
    tid, turn, _ = await running_turn(ph)
    owner = ph.engine.driver.owners[tid]
    client, generation = owner.client, owner.native_connection_generation
    await ph.call('thread/settings/update', {'threadId': tid, 'effort': 'low'})
    await ph.call('thread/settings/update', {'threadId': tid, 'model': 'opus', 'effort': 'max'})
    assert owner.client is client and owner.native_connection_generation == generation, 'no new child'
    assert client.disconnect_count == 0
    assert flag_requests(client) == [{'effortLevel': 'low'}, {'effortLevel': 'max'}]
    assert client.options.model == 'opus' and client.options.effort == 'max'
    assert ph.engine.thread_effort(tid) == 'max' and ph.store.thread(tid)['model'] == 'opus'
    assert ph.store.db.execute('SELECT state FROM turns WHERE id=?', (turn,)).fetchone()[0] == 'inProgress', \
        'the turn keeps running'
    await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})


async def test_fast_mode_goes_live_too(ph, monkeypatch):
    monkeypatch.setattr(ph.server.model_catalog.probe, 'read', AsyncMock(return_value='2.1.301'))
    tid, turn, _ = await running_turn(ph)
    await ph.call('thread/settings/update', {'threadId': tid, 'model': 'claude-opus-5-5', 'serviceTier': 'priority'})
    client = ph.engine.driver.owners[tid].client
    assert flag_requests(client)[-1]['fastMode'] is True and ph.engine.thread_fast(tid)
    await ph.call('thread/settings/update', {'threadId': tid, 'serviceTier': None})
    assert flag_requests(client)[-1]['fastMode'] is False and not ph.engine.thread_fast(tid)
    assert client.disconnect_count == 0
    await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})


async def test_an_older_cli_still_refuses_while_busy(ph, monkeypatch):
    monkeypatch.setattr(ph.server.model_catalog.probe, 'read', AsyncMock(return_value='2.1.290'))
    tid, turn, _ = await running_turn(ph)
    error = await ph.call('thread/settings/update', {'threadId': tid, 'effort': 'low'}, error=True)
    assert error['code'] == -32009 and 'Finish or interrupt' in error['message']
    assert flag_requests(ph.engine.driver.owners[tid].client) == []
    await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})


async def test_a_model_switch_carries_the_models_auto_compaction(ph, monkeypatch):
    monkeypatch.setattr(ph.server.model_catalog.probe, 'read', AsyncMock(return_value='2.1.301'))
    ph.engine.compaction = Compaction({WINDOW_ENV: '533000', PERCENT_ENV: '100',
                                       MODELS_ENV: json.dumps({'opus': {'window': 333000, 'percent': 100}})})
    tid, turn, _ = await running_turn(ph)
    client = ph.engine.driver.owners[tid].client
    assert WINDOW_ENV not in client.options.env, 'sonnet runs on the launch values'
    await ph.call('thread/settings/update', {'threadId': tid, 'model': 'opus'})
    assert flag_requests(client)[-1]['env'] == {WINDOW_ENV: '333000', PERCENT_ENV: '100'}
    await ph.call('thread/settings/update', {'threadId': tid, 'model': 'sonnet'})
    assert flag_requests(client)[-1]['env'] == {WINDOW_ENV: '533000', PERCENT_ENV: '100'}
    await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})


async def test_a_new_child_starts_with_its_models_auto_compaction(ph):
    ph.engine.compaction = Compaction({MODELS_ENV: json.dumps({'sonnet': {'window': 233000, 'percent': 90}})})
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    env = ph.engine.driver.owners[tid].client.options.env
    assert (env[WINDOW_ENV], env[PERCENT_ENV]) == ('233000', '90')


async def test_an_older_cli_replaces_the_idle_child_when_auto_compaction_differs(ph, monkeypatch):
    monkeypatch.setattr(ph.server.model_catalog.probe, 'read', AsyncMock(return_value='2.1.290'))
    ph.engine.compaction = Compaction({MODELS_ENV: json.dumps({'opus': {'window': 333000, 'percent': 100}})})
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    owner = ph.engine.driver.owners[tid]
    first = owner.client
    await ph.call('thread/settings/update', {'threadId': tid, 'model': 'opus'})
    assert owner.client is not first, 'set_model alone would keep the old environment'
    assert owner.client.options.model == 'opus' and owner.client.options.env[WINDOW_ENV] == '333000'
    assert flag_requests(first) == []
