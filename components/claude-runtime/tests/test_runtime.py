from __future__ import annotations

import asyncio
import copy
import json
import os
import subprocess
import sys
import uuid
from dataclasses import replace
from pathlib import Path

import pytest

from cute_claude.config import Settings
from cute_claude.engine import Engine
from cute_claude.protocol import Server
from cute_claude.provider import Completion, MockProvider
from cute_claude.store import Store, dump
from cute_claude.tools import ToolRegistry
from conftest import Harness


def external(tid, mid='message-1', value='hello from another agent'):
    return {'threadId': tid, 'messageId': mid, 'source': {'kind': 'agent', 'id': 'director'}, 'eventType': 'task.assigned', 'text': value}


def assert_tool_pairs(messages):
    for index, message in enumerate(messages):
        calls = [b['id'] for b in message['content'] if b['type'] == 'tool_use']
        if calls:
            assert messages[index+1]['role'] == 'user'
            results = [b['tool_use_id'] for b in messages[index+1]['content'] if b['type'] == 'tool_result']
            assert results == calls


async def test_create_resume_are_read_only(h):
    tid = await h.thread()
    uuid.UUID(tid)
    assert (await h.call('thread/resume', {'threadId': tid}))['thread']['id'] == tid
    assert not h.provider.requests
    assert not h.engine.workers
    assert (await h.call('claude/capabilities'))['cliDependency'] is False


async def test_controller_creation_retry_reuses_identity_without_model_turn(h):
    tid = str(uuid.uuid4())
    params = {'threadId': tid, 'instructions': 'Work on this project.'}
    assert await h.call('claude/thread/ensure', params) == {'threadId': tid, 'created': True}
    assert await h.call('claude/thread/ensure', params) == {'threadId': tid, 'created': False}
    assert len((await h.call('thread/list'))['data']) == 1
    assert not h.provider.requests and not h.engine.workers
    error = await h.call('claude/thread/ensure', {**params, 'instructions': 'Different task'}, error=True)
    assert error['code'] == -32009
    assert h.store.thread(tid)['instructions'] == params['instructions']
    assert (await h.call('claude/thread/ensure', {'threadId': '../bad'}, error=True))['code'] == -32602


async def test_controller_creation_survives_reopening_store(h):
    tid = str(uuid.uuid4())
    await h.call('claude/thread/ensure', {'threadId': tid})
    h.store.close()
    h.store = Store(h.settings.state_dir)
    h.engine.store = h.server.store = h.store
    assert await h.call('claude/thread/ensure', {'threadId': tid}) == {'threadId': tid, 'created': False}


async def test_text_stream_response_precedes_notifications(h):
    tid = await h.thread()
    turn_id = await h.turn(tid, 'hello')
    await h.drain()
    response_idx = next(i for i, m in enumerate(h.out) if m.get('result', {}).get('turn', {}).get('id') == turn_id)
    started_idx = next(i for i, m in enumerate(h.out) if m.get('method') == 'turn/started')
    assert response_idx < started_idx
    deltas = ''.join(m['params']['delta'] for m in h.out if m.get('method') == 'item/agentMessage/delta')
    turn = h.store.turn(turn_id)
    assert turn['status'] == 'completed'
    assert deltas == turn['items'][-1]['text'] == '[MOCK] hello'
    assert len(h.provider.requests) == 1


async def test_tool_loop_and_usage(h):
    tid = await h.thread()
    turn_id = await h.turn(tid, 'tool:read_file {"path":"hello.txt"}')
    await h.drain()
    assert len(h.provider.requests) == 2
    history = h.store.model_history(tid, 1_000_000)
    assert_tool_pairs(history)
    assert 'hello from fixture' in history[-1]['content'][0]['text']
    turn = h.store.turn(turn_id)
    call = next(i for i in turn['items'] if i['type'] == 'dynamicToolCall')
    assert call['success'] is True
    usage = (await h.call('claude/thread/inspect', {'threadId': tid}))['usage'][0]
    assert len(usage['modelRequests']) == 2 and usage['mock'] is True
    assert 'x-api-key' not in dump(usage)


async def test_multiple_tool_calls_have_complete_result_pairing(h):
    tid = await h.thread()
    prompt = 'tools:' + json.dumps([{'name': 'read_file', 'input': {'path': 'hello.txt'}}, {'name': 'list_directory', 'input': {}}])
    turn = await h.turn(tid, prompt)
    await h.drain()
    assert h.store.turn(turn)['status'] == 'completed'
    history = h.store.model_history(tid, 1_000_000)
    assert_tool_pairs(history)
    assert len(history[2]['content']) == 2


async def test_idempotent_external_input_and_conflict(h):
    tid = await h.thread()
    params = external(tid)
    first = await h.call('claude/input/submit', params)
    duplicate = await h.call('claude/input/submit', params)
    assert first['turnId'] == duplicate['turnId']
    assert first['receiptScope'] == 'local-inbox'
    assert duplicate['duplicate'] is True
    assert first['sourceVerified'] is False
    error = await h.call('claude/input/submit', {**params, 'text': 'changed'}, error=True)
    assert error['code'] == -32010
    await h.drain()
    assert len(h.provider.requests) == 1
    assert (await h.call('claude/input/status', {'threadId': tid, 'messageId': 'message-1'}))['state'] == 'completed'
    history = h.provider.requests[0]['messages']
    assert history[0]['role'] == 'user'
    assert 'EXTERNAL_INPUT_JSON' in history[0]['content'][0]['text']
    assert '"kind":"agent"' in history[0]['content'][0]['text']


async def test_fifo_new_turns_not_same_turn_steering(h):
    tid = await h.thread()
    active = await h.turn(tid, 'sleep:0.05')
    error = await h.call('turn/start', {'threadId': tid, 'input': [{'type': 'text', 'text': 'not steering'}]}, error=True)
    assert error['code'] == -32009 and error['data']['reason'] == 'threadBusy'
    second = await h.call('claude/input/submit', external(tid, 'second', 'second'))
    third = await h.call('claude/input/submit', external(tid, 'third', 'third'))
    assert second['state'] == third['state'] == 'queued'
    await h.drain()
    ids = [m['params']['turn']['id'] for m in h.out if m.get('method') == 'turn/started']
    assert ids == [active, second['turnId'], third['turnId']]
    assert len(h.provider.requests) == 3


@pytest.mark.parametrize('mode', ['after_turn', 'soon', 'passive'])
async def test_old_delivery_modes_rejected_not_silently_mapped(h, mode):
    tid = await h.thread()
    error = await h.call('claude/input/submit', {**external(tid), 'delivery': mode}, error=True)
    assert error['code'] == -32602
    assert not h.store.pending(tid)


async def test_stale_runtime_fence(h):
    tid = await h.thread()
    error = await h.call('claude/input/submit', {**external(tid), 'expectedRuntimeId': 'old'}, error=True)
    assert error['code'] == -32011


async def test_disconnect_does_not_cancel_model(h):
    tid = await h.thread()
    turn = await h.turn(tid, 'sleep:0.03')
    h.server.disconnect(h.peer)
    await h.drain()
    assert h.store.turn(turn)['status'] == 'completed'
    assert len(h.provider.requests) == 1
    h.peer = h.server.connect(h.send)
    await h.init()
    snapshot = await h.call('thread/resume', {'threadId': tid})
    assert snapshot['thread']['turns'] == []
    page = await h.call('thread/turns/list', {'threadId': tid})
    assert page['data'][0]['id'] == turn
    assert len(h.provider.requests) == 1


async def test_interrupt_pauses_queue_and_does_not_replay(h):
    tid = await h.thread()
    turn = await h.turn(tid, 'sleep:10')
    await asyncio.sleep(0.01)
    queued = await h.call('claude/input/submit', external(tid, 'queued'))
    await h.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
    assert h.store.turn(turn)['status'] == 'interrupted'
    assert h.store.thread(tid)['halted']
    assert len(h.provider.requests) == 1
    assert (await h.call('claude/input/status', {'threadId': tid, 'messageId': 'queued'}))['state'] == 'queued'
    await h.call('claude/queue/resume', {'threadId': tid, 'acknowledgeUncertainEffects': True})
    await h.drain()
    assert h.store.turn(queued['turnId'])['status'] == 'completed'
    assert len(h.provider.requests) == 2


async def test_interrupt_before_worker_runs(h):
    tid = await h.thread()
    turn = await h.turn(tid, 'sleep:10')
    await h.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
    assert h.store.turn(turn)['status'] == 'interrupted'
    assert h.store.thread(tid)['halted']
    assert not h.engine.workers


async def test_cancellation_during_tool_keeps_unknown_result(h):
    entered = asyncio.Event()
    count = 0
    async def blocking(args, ctx):
        nonlocal count
        count += 1
        entered.set()
        await asyncio.sleep(30)
    h.tools.register('blocking', 'test', {'type': 'object'}, blocking)
    tid = await h.thread()
    turn = await h.turn(tid, 'tool:blocking {}')
    await asyncio.wait_for(entered.wait(), 2)
    await h.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
    history = h.store.model_history(tid, 1_000_000)
    assert_tool_pairs(history)
    assert 'uncertain' in history[-1]['content'][0]['content']
    assert count == 1
    inspect = await h.call('claude/thread/inspect', {'threadId': tid})
    assert inspect['toolCalls'][0]['state'] == 'uncertain'
    await h.call('claude/queue/resume', {'threadId': tid, 'acknowledgeUncertainEffects': True})
    await h.drain()
    assert count == 1


async def test_provider_failure_no_retry(h):
    tid = await h.thread()
    turn = await h.turn(tid, 'fail:please')
    await h.drain()
    assert len(h.provider.requests) == 1
    assert h.store.turn(turn)['status'] == 'failed'
    assert h.store.thread(tid)['halted']


async def test_unknown_history_mode_error_is_explicit(h):
    error = await h.call('thread/start', {'historyMode': 'invalid'}, error=True)
    assert error['code'] == -32602 and 'historyMode' in error['message']
    error = await h.call('thread/items/list', {}, error=True)
    assert error['code'] == -32602
    assert not h.store.db.execute('SELECT * FROM threads').fetchall()


@pytest.mark.parametrize('params', [{'approvalPolicy': 'on-request'}, {'sandbox': 'read-only'}, {'dynamicTools': [{'name': 'tool'}]}, {'model': 'other-model'}, {'ephemeral': True}])
async def test_unsupported_runtime_settings_are_not_fake_success(h, params):
    error = await h.call('thread/start', params, error=True)
    assert error['code'] == -32602


async def test_image_input_rejected(h):
    tid = await h.thread()
    err = await h.call('turn/start', {'threadId': tid, 'input': [{'type': 'image', 'url': 'https://example.invalid/image'}]}, error=True)
    assert err['code'] == -32602
    assert not h.provider.requests


async def test_presentation_and_metadata_not_in_model_history(h):
    tid = await h.thread()
    h.store.event(tid, 'claude/displayOnlyExample', {'text': 'SECRET_UI_MARKER'})
    await h.call('claude/input/submit', {**external(tid), 'metadata': {'uiHint': 'SECRET_METADATA_MARKER'}})
    await h.drain()
    sent = dump(h.provider.requests)
    assert 'SECRET_UI_MARKER' not in sent and 'SECRET_METADATA_MARKER' not in sent
    assert len(h.store.events(tid, 0, 200)) > 0
    assert not any(e['method'] == 'item/agentMessage/delta' for e in h.store.events(tid, 0, 200))


async def test_extensions_not_sent_to_stock_client(h):
    h.peer.extensions = False
    tid = await h.thread()
    await h.call('claude/input/submit', external(tid))
    await h.drain()
    assert not any(m.get('method', '').startswith('claude/') for m in h.out)


async def test_no_notifications_for_unsubscribed_peer(h):
    received = []
    async def send(m):
        received.append(copy.deepcopy(m))
    peer = h.server.connect(send)
    peer.initialized = peer.ready = True
    tid = await h.thread()
    await h.turn(tid, 'hello')
    await h.drain()
    assert received == []


async def test_history_budget_refuses_without_calling_provider(h):
    h.engine.settings = replace(h.settings, max_history_bytes=10)
    tid = await h.thread()
    turn = await h.turn(tid, 'too long for tiny budget')
    await h.drain()
    assert h.store.turn(turn)['status'] == 'failed'
    assert not h.provider.requests


async def test_inbox_capacity(h):
    h.server.settings = replace(h.settings, max_pending=1)
    tid = await h.thread()
    h.store.db.execute('UPDATE threads SET halted=1 WHERE id=?', (tid,))
    await h.call('claude/input/submit', external(tid))
    err = await h.call('claude/input/submit', external(tid, 'other'), error=True)
    assert err['code'] == -32001


async def test_response_loss_does_not_strand_accepted_input(h):
    tid = await h.thread()
    delivered = []
    async def lose_once(message):
        if 'result' in message and 'durable' in message['result']:
            raise ConnectionError('response lost')
        delivered.append(message)
    peer = h.server.connect(lose_once)
    peer.initialized = peer.ready = True
    await h.server.handle(peer, {'id': 88, 'method': 'claude/input/submit', 'params': external(tid)})
    await h.drain()
    status = h.store.input_status(tid, 'message-1')
    assert status['state'] == 'completed'
    duplicate = await h.call('claude/input/submit', external(tid))
    assert duplicate['duplicate'] is True and len(h.provider.requests) == 1


async def test_model_budget_stops_instead_of_background_continuation(h):
    h.engine.settings = replace(h.settings, max_steps=1)
    tid = await h.thread()
    turn = await h.turn(tid, 'tool:read_file {"path":"hello.txt"}')
    await h.drain()
    assert len(h.provider.requests) == 1
    assert h.store.turn(turn)['status'] == 'failed'
    assert_tool_pairs(h.store.model_history(tid, 1_000_000))


def test_second_writer_refused(tmp_path):
    first = Store(tmp_path / 'state')
    try:
        with pytest.raises(RuntimeError, match='another app-server'):
            Store(tmp_path / 'state')
    finally:
        first.close()


def test_real_process_crash_after_side_effect_is_not_replayed(tmp_path):
    workspace = tmp_path / 'workspace'
    workspace.mkdir()
    state = tmp_path / 'state'
    code = r'''
import asyncio,sys
from pathlib import Path
from cute_claude.config import Settings
from cute_claude.store import Store
from cute_claude.provider import MockProvider
from cute_claude.tools import ToolRegistry
from cute_claude.engine import Engine
async def main():
 w=Path(sys.argv[1]); s=Store(Path(sys.argv[2])); cfg=Settings(w,Path(sys.argv[2]))
 tid=s.new_thread('mock-claude',str(w)); (w/'thread-id').write_text(tid)
 payload={'source':{'kind':'user','id':'test'},'eventType':'message','text':'tool:crash {}','metadata':{}}
 s.enqueue(tid,'first',payload,128)
 s.enqueue(tid,'queued',{**payload,'text':'queued input'},128)
 tools=ToolRegistry(cfg)
 async def crash(args,ctx):
  import os
  (w/'effect').write_text('one side effect')
  os._exit(77)
 tools.register('crash','crash test',{'type':'object'},crash)
 e=Engine(cfg,s,MockProvider(),tools);e.wake(tid)
 await asyncio.gather(*e.workers.values())
asyncio.run(main())
'''
    env = {**os.environ, 'PYTHONPATH': str(Path(__file__).parents[1] / 'src')}
    proc = subprocess.run([sys.executable, '-c', code, str(workspace), str(state)], env=env, timeout=10, capture_output=True, text=True)
    assert proc.returncode == 77, proc.stderr
    assert (workspace / 'effect').read_text() == 'one side effect'
    store = Store(state)
    try:
        tid = (workspace / 'thread-id').read_text()
        assert store.thread(tid)['halted']
        assert store.input_status(tid, 'first')['state'] == 'uncertain'
        assert store.input_status(tid, 'queued')['state'] == 'queued'
        assert store.claim(tid) is None
        recovered_turn = store.turn(store.input_status(tid, 'first')['turnId'])
        tool_items = [i for i in recovered_turn['items'] if i['type'] == 'dynamicToolCall']
        assert tool_items and tool_items[0]['status'] == 'failed'
        assert any(e['method'] == 'turn/completed' for e in store.events(tid, 0, 200))
        messages = store.model_history(tid, 1_000_000)
        assert_tool_pairs(messages)
        assert 'uncertain' in messages[-1]['content'][0]['content']
    finally:
        store.close()


async def test_handshake_id_zero_and_bad_params(h):
    messages = []
    async def send(message): messages.append(message)
    peer = h.server.connect(send)
    await h.server.handle(peer, {'id': 0, 'method': 'thread/start'})
    assert messages[-1]['id'] == 0 and messages[-1]['error']['code'] == -32002
    await h.server.handle(peer, {'id': 1, 'method': 'initialize', 'params': []})
    assert messages[-1]['error']['code'] == -32602
    await h.server.handle(peer, [])
    assert messages[-1]['error']['code'] == -32600


@pytest.mark.parametrize('override', [
    {'sandboxPolicy': {'type': 'readOnly'}},
    {'approvalsReviewer': 'auto_review'},
    {'collaborationMode': {'mode': 'plan'}},
    {'permissionProfile': 'restricted'},
])
async def test_permission_or_mode_overrides_are_not_silently_ignored(h, override):
    tid = await h.thread()
    error = await h.call('turn/start', {'threadId': tid, 'input': [{'type': 'text', 'text': 'hello'}], **override}, error=True)
    assert error['code'] == -32602
    assert not h.provider.requests and not h.store.pending(tid)


async def test_resume_cannot_mutate_system_instructions(h):
    tid = (await h.call('thread/start', {'developerInstructions': 'Original'}))['thread']['id']
    error = await h.call('thread/resume', {'threadId': tid, 'developerInstructions': 'Changed'}, error=True)
    assert error['code'] == -32602
    assert h.store.thread(tid)['instructions'] == 'Original'
    await h.call('thread/resume', {'threadId': tid, 'developerInstructions': 'Original'})
    assert not h.provider.requests


async def test_standard_last_usage_is_latest_inference_not_entire_turn(h):
    tid = await h.thread()
    await h.turn(tid, 'tool:read_file {"path":"hello.txt"}')
    await h.drain()
    usage = [m['params']['tokenUsage'] for m in h.out if m.get('method') == 'thread/tokenUsage/updated'][-1]
    assert usage['total']['totalTokens'] == 36
    assert usage['last']['totalTokens'] == 18

async def test_tui_default_tier_and_operator_workspace_trust(h):
    config = await h.call('config/read')
    assert config['config']['projects'] == {str(h.settings.workspace): {'trust_level': 'trusted'}}
    tid = await h.thread()
    reply = await h.call('turn/start', {'threadId': tid, 'serviceTier': 'default',
        'input': [{'type': 'text', 'text': 'tui-default-tier'}]})
    await h.drain()
    assert h.store.turn(reply['turn']['id'])['status'] == 'completed'
    failure = await h.call('turn/start', {'threadId': tid, 'serviceTier': 'fast',
        'input': [{'type': 'text', 'text': 'unsupported tier'}]}, error=True)
    assert failure['code'] == -32602
