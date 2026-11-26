"""Profile launch defaults are distinct from interactive model selection."""
from dataclasses import replace

import pytest

from test_persistent import ph


async def test_profile_launch_model_changes_after_restart_keep_native_identity(ph):
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    sid = ph.engine.session(tid)['session_id']
    original_cwd = ph.store.thread(tid)['cwd']
    original_instructions = ph.store.thread(tid)['instructions']
    # A live native process must never have its saved model silently changed.
    from cute_claude.errors import RpcError
    with pytest.raises(RpcError, match='Close the native runtime'):
        ph.engine.ensure_launch_thread(tid, 'opus', original_cwd, original_instructions)
    assert ph.store.thread(tid)['model'] == 'sonnet'
    await ph.engine.close_runtime(tid)
    ph.server.settings = ph.engine.settings = replace(ph.settings, model='opus')
    result = await ph.call('claude/thread/ensure', {'threadId': tid, 'model': 'opus', 'instructions': original_instructions})
    assert result['created'] is False
    await ph.engine.start_runtime(tid)
    assert ph.sdk.clients[-1].options.model == 'opus'
    assert ph.engine.session(tid)['session_id'] == sid
    assert all(c.query_count == 0 for c in ph.sdk.clients)
    # Ordinary reconnect/ensure still preserves a per-thread interactive choice.
    await ph.engine.set_model(tid, 'haiku')
    await ph.call('claude/thread/ensure', {'threadId': tid, 'model': 'opus', 'instructions': original_instructions})
    assert ph.store.thread(tid)['model'] == 'haiku'
    # Identity checks precede mutation even when a new launch model is requested.
    await ph.engine.close_runtime(tid)
    with pytest.raises(RpcError, match='different settings'):
        ph.engine.ensure_launch_thread(tid, 'sonnet', original_cwd, 'different instructions')
    assert ph.store.thread(tid)['model'] == 'haiku'


async def test_switching_profiles_with_same_default_resets_manual_choice_once(ph):
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    await ph.engine.set_model(tid, 'haiku')
    await ph.engine.close_runtime(tid)
    # Host launches the replacement adapter with this hint only on profile change.
    ph.engine.reset_launch_model = True
    await ph.call('claude/thread/ensure', {'threadId': tid})
    assert ph.store.thread(tid)['model'] == 'sonnet'
    await ph.engine.start_runtime(tid)
    assert ph.sdk.clients[-1].options.model == 'sonnet'
    await ph.engine.set_model(tid, 'opus')
    # The launch hint is consumed; repeated Start/ensure cannot overwrite a live choice.
    await ph.call('claude/thread/ensure', {'threadId': tid})
    assert ph.store.thread(tid)['model'] == 'opus'


async def test_explicit_offline_launch_selection_replaces_retired_saved_pair(ph):
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    saved_native_id = ph.engine.session(tid)['session_id']
    instructions = ph.store.thread(tid)['instructions']
    cwd = ph.store.thread(tid)['cwd']
    await ph.engine.close_runtime(tid)
    ph.engine.save_model_effort(tid, 'retired-fixture', 'max')
    assert ph.store.thread(tid)['model'] == 'retired-fixture'
    assert ph.engine.thread_effort(tid) == 'max'

    selection = {'threadId': tid, 'model': 'sonnet', 'instructions': instructions,
                 'launchSelection': {'effort': 'low'}}
    with pytest.raises(Exception, match='explicit effort or null'):
        await ph.call('claude/thread/ensure', {**selection, 'launchSelection': {}})
    assert ph.store.thread(tid)['model'] == 'retired-fixture'
    for _ in range(2):  # Retry before the runtime result is committed.
        result = await ph.call('claude/thread/ensure', selection)
        assert result['created'] is False
    assert ph.store.thread(tid)['cwd'] == cwd
    assert ph.store.thread(tid)['instructions'] == instructions
    assert ph.store.thread(tid)['model'] == 'sonnet'
    assert ph.engine.thread_effort(tid) == 'low'
    assert ph.engine.session(tid)['session_id'] == saved_native_id

    await ph.engine.start_runtime(tid)
    assert ph.sdk.clients[-1].options.model == 'sonnet'
    assert ph.engine.session(tid)['session_id'] == saved_native_id
    assert all(client.query_count == 0 for client in ph.sdk.clients)
    with pytest.raises(Exception, match='Close the native runtime'):
        await ph.call('claude/thread/ensure', {**selection, 'launchSelection': {'effort': None}})
    assert ph.engine.thread_effort(tid) == 'low'
