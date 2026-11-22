"""Actual bundled CLI, private native transcript, loopback API, counted effects."""
import asyncio
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib
import json
import os
from pathlib import Path
import shlex
import socket
import struct
import sys
import threading

import pytest

from cute_claude import checkpoint_recovery as recovery
from cute_claude.config import Settings
from cute_claude.subscription import OfficialSdkDriver, SIDE_BOUNDARY_BLOCKS
from cute_claude.subscription_auth import CONFLICTING_ENV
from cute_claude.tools import ToolResult
from test_retry_history_native import sse, PNG
from test_subscription import SubHarness

pytestmark = pytest.mark.skipif(not os.environ.get('CUTEX_TEST_CLAUDE_CLI'), reason='explicit private CLI required')


def tool_response(calls, number=1):
    events = [('message_start', {'type': 'message_start', 'message': {'id': f'msg_counter_{number}', 'type': 'message',
        'role': 'assistant', 'model': 'claude-sonnet-4-6', 'content': [], 'stop_reason': None,
        'usage': {'input_tokens': 10, 'output_tokens': 0}}})]
    for n, (name, arguments) in enumerate(calls):
        events.extend([
            ('content_block_start', {'type': 'content_block_start', 'index': n,
                'content_block': {'type': 'tool_use', 'id': f'toolu_counter_{number}_{n}', 'name': name, 'input': {}}}),
            ('content_block_delta', {'type': 'content_block_delta', 'index': n,
                'delta': {'type': 'input_json_delta', 'partial_json': json.dumps(arguments)}}),
            ('content_block_stop', {'type': 'content_block_stop', 'index': n})])
    events.extend([
        ('message_delta', {'type': 'message_delta', 'delta': {'stop_reason': 'tool_use', 'stop_sequence': None}, 'usage': {'output_tokens': 7}}),
        ('message_stop', {'type': 'message_stop'})])
    return ''.join(f'event: {name}\ndata: {json.dumps(data)}\n\n' for name, data in events).encode()


@pytest.mark.parametrize('mode', ['bash', 'write', 'mcp', 'parallel', 'exhausted', 'effect-before-journal',
                                  'journal-before-sdk', 'sdk-before-native', 'partial-text', 'partial-tool', 'cancel',
                                  'project-hooks', 'project-no-hooks', 'subagent', 'duplicate-native'])
async def test_real_cli_counted_effect_and_checkpoint_windows(tmp_path, monkeypatch, mode):
    for name in CONFLICTING_ENV:
        monkeypatch.delenv(name, raising=False)
    for name in ('HOME', 'USERPROFILE', 'APPDATA', 'LOCALAPPDATA', 'TEMP', 'TMP', 'TMPDIR'):
        folder = tmp_path / name.lower()
        folder.mkdir(exist_ok=True)
        monkeypatch.setenv(name, str(folder))
    monkeypatch.setattr(recovery, 'delay', lambda n: .05)
    work = tmp_path / 'work'
    work.mkdir()
    counter = work / 'counter.txt'
    edited = work / 'edited.txt'
    hook_count = work / 'hook-count.txt'
    project = mode in {'project-hooks', 'project-no-hooks'}
    if project:
        (work / '.claude').mkdir()
        settings = {'hooks': {'UserPromptSubmit': [{'hooks': [{'type': 'command',
            'command': f'echo HOOK_ONCE >> "{hook_count.as_posix()}"'}]}]}} if mode == 'project-hooks' else {'permissions': {}}
        (work / '.claude/settings.json').write_text(json.dumps(settings))
    calls, queries, hook_calls, effects, frames, checkpoints = [], [], [], [], [], []
    terminal = recovery.Checkpoint.terminal
    def observed_terminal(self, owner, message):
        terminal(self, owner, message)
        checkpoints.append({'calls': self.calls, 'results': self.results.copy(), 'unsafe': self.unsafe,
                            'fresh': self.fresh_content, 'inflight': self.terminal_inflight, 'snapshot': bool(self.snapshot)})
    monkeypatch.setattr(recovery.Checkpoint, 'terminal', observed_terminal)
    program = ('from pathlib import Path; p=Path(' + repr(counter.as_posix()) + '); '
               'p.write_text(str(int(p.read_text())+1) if p.exists() else "1"); print("COUNTER_OK")')
    bash = ('Bash', {'command': shlex.quote(Path(sys.executable).as_posix()) + ' -c ' + shlex.quote(program), 'description': 'private counter fixture'})
    write = ('Write', {'file_path': edited.as_posix(), 'content': 'ONE WRITE'})
    mcp = ('mcp__cutex__counter', {'arguments': {}})
    agent = ('Agent', {'description': 'private fixture child', 'prompt': 'Execute the private child fixture then finish.',
                       'subagent_type': 'general-purpose', 'run_in_background': False})
    planned = [bash] if mode in {'bash', 'duplicate-native'} else [write] if mode == 'write' else [bash, write] if mode == 'parallel' else [agent] if mode == 'subagent' else [mcp]

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def reset(self):
            self.connection.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack('ii', 1, 0))
            self.close_connection = True
            self.connection.close()

        def do_POST(self):
            self.connection.setblocking(True)
            raw = self.rfile.read(int(self.headers.get('Content-Length', 0)))
            if not self.path.split('?')[0].endswith('/messages'):
                data = b'{"input_tokens":1}'
            else:
                calls.append(json.loads(raw))
                n = len(calls)
                if n == 1:
                    data = tool_response(planned)
                elif mode == 'subagent' and n == 2:
                    data = tool_response([bash], n)
                elif mode == 'subagent' and n == 3:
                    data = sse(n, 'CHILD COMPLETE')
                elif mode == 'duplicate-native' and n == 3:
                    data = tool_response([bash], n).replace(b'toolu_counter_3_0', b'toolu_counter_1_0')
                elif mode == 'subagent':
                    self.reset()
                    return
                elif mode == 'exhausted' or n == 2:
                    if mode in {'partial-text', 'partial-tool'}:
                        data = sse(n, 'INCOMPLETE-TEXT') if mode == 'partial-text' else tool_response([bash], n)
                        cut = data.split(b'event: content_block_stop')[0]
                        self.send_response(200)
                        self.send_header('Content-Type', 'text/event-stream')
                        self.send_header('Content-Length', str(len(data) + 4096))
                        self.end_headers()
                        self.wfile.write(cut)
                        self.wfile.flush()
                    self.reset()
                    return
                else:
                    data = sse(n, 'COUNTER TURN COMPLETE')
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream' if self.path.split('?')[0].endswith('/messages') else 'application/json')
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    sdk = importlib.import_module('claude_agent_sdk')
    real_client = sdk.ClaudeSDKClient

    def instrumented(*, options):
        options.env.update({'ANTHROPIC_API_KEY': 'synthetic-checkpoint-key',
            'ANTHROPIC_BASE_URL': f'http://127.0.0.1:{server.server_port}',
            'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1', 'DISABLE_TELEMETRY': '1', 'DISABLE_ERROR_REPORTING': '1'})
        original_hook = options.hooks['PreToolUse'][0].hooks[0]

        async def counted(data, call_id, context):
            hook_calls.append(data.get('tool_name'))
            return await original_hook(data, call_id, context)
        options.hooks['PreToolUse'][0].hooks[0] = counted
        client = real_client(options=options)
        original_query = client.query

        async def query(*args, **kwargs):
            queries.append(True)
            return await original_query(*args, **kwargs)
        client.query = query
        receive = client.receive_messages
        async def observed():
            async for message in receive():
                if recovery.kind(message) in {'StreamEvent', 'stream_event'}:
                    event = recovery.field(message, 'event', {})
                    frames.append([event.get('type'), (event.get('content_block') or {}).get('type')])
                yield message
        client.receive_messages = observed
        return client
    monkeypatch.setattr(sdk, 'ClaudeSDKClient', instrumented)
    driver = OfficialSdkDriver(Path(os.environ['CUTEX_TEST_CLAUDE_CLI']), tmp_path / 'native')
    h = SubHarness(Settings(work, tmp_path / 'state', model='claude-sonnet-4-6', mock=False,
                           backend='subscription', thinking='disabled', allow_exec=True, allow_write=True,
                           setting_sources=('project',) if project else ()), driver)
    async def effect(args, context):
        effects.append(context.call_id)
        return ToolResult('COUNTER-RESULT')
    h.tools.register('counter', 'counted private fixture', {'type': 'object', 'properties': {}}, effect)
    original_execute = h.tools.execute
    if mode == 'effect-before-journal':
        async def gap(*args, **kwargs):
            await original_execute(*args, **kwargs)
            raise asyncio.CancelledError()
        monkeypatch.setattr(h.tools, 'execute', gap)
    if mode == 'journal-before-sdk':
        original_observe = recovery.Checkpoint.observe
        def omit(self, message):
            if recovery.kind(message) in {'UserMessage', 'user'}:
                return
            return original_observe(self, message)
        monkeypatch.setattr(recovery.Checkpoint, 'observe', omit)
    if mode == 'sdk-before-native':
        monkeypatch.setattr(recovery.Checkpoint, 'persisted', lambda *a: False)
    if mode == 'cancel':
        monkeypatch.setattr(recovery, 'delay', lambda n: 60)
    try:
        await h.init()
        tid = await h.thread()
        monkeypatch.setattr(h.store, 'turn_note_frames', lambda turn: ['CHECKPOINT-NOTE-ONCE'])
        monkeypatch.setattr(h.engine.side_chats, 'boundary_pending', lambda tid: True)
        before = await h.engine.start_runtime(tid)
        turn = (await h.call('turn/start', {'threadId': tid, 'input': [
            {'type': 'text', 'text': 'CHECKPOINT-ORIGINAL-ONCE'},
            {'type': 'image', 'url': 'data:image/png;base64,' + PNG}]}))['turn']['id']
        if mode == 'cancel':
            async with asyncio.timeout(60):
                while turn not in h.engine.retry_waiting:
                    await asyncio.sleep(.01)
            await h.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
        async with asyncio.timeout(90):
            while h.engine.workers:
                await asyncio.gather(*list(h.engine.workers.values()))
        result = h.store.turn(turn)
        positive = mode in {'bash', 'write', 'mcp', 'parallel', 'project-no-hooks', 'duplicate-native'}
        expected_queries = 2 if positive else 3 if mode == 'exhausted' else 1
        evidence = {'mode': mode, 'queries': len(queries), 'http': len(calls), 'hooks': hook_calls,
                    'effectCount': len(effects), 'status': result['status'], 'error': result.get('error'), 'frames': frames,
                    'checkpoints': checkpoints}
        print(json.dumps(evidence))
        assert len(queries) == expected_queries, evidence
        assert result['status'] == ('completed' if positive else 'interrupted' if mode == 'cancel' else 'failed'), evidence
        assert hook_calls == (['Agent', 'Bash'] if mode == 'subagent' else ['Bash', 'Bash'] if mode == 'duplicate-native' else [name for name, _ in planned]), evidence
        if mode in {'bash', 'parallel', 'subagent', 'duplicate-native'}:
            assert counter.read_text() == '1'
        if mode == 'duplicate-native':
            commands = [x for x in h.store.items(turn) if x['type'] == 'commandExecution']
            assert len(commands) == 1 and commands[0]['status'] == 'completed'
            assert 'COUNTER_OK' in commands[0]['aggregatedOutput']
            native = next((driver.config_dir / 'projects').glob(f'*/{h.engine.session(tid)["session_id"]}.jsonl'))
            original_result = next(json.loads(line)['uuid'] for line in native.read_text().splitlines()
                if any(b.get('type') == 'tool_result' and b.get('tool_use_id') == 'toolu_counter_1_0'
                       and not b.get('is_error') for b in (json.loads(line).get('message') or {}).get('content', [])
                       if isinstance(b, dict)))
            assert h.store.db.execute('SELECT result_uuid FROM sdk_recovery_tools WHERE turn_id=? AND call_id=?',
                                      (turn, 'toolu_counter_1_0')).fetchone()[0] == original_result
        if mode in {'write', 'parallel'}:
            assert edited.read_text() == 'ONE WRITE'
        if mode not in {'bash', 'write', 'parallel', 'subagent', 'duplicate-native'}:
            assert len(effects) == 1, evidence
        if mode == 'project-hooks':
            assert hook_count.read_text().splitlines() == ['HOOK_ONCE']
        for body in calls:
            blocks = [b for m in body.get('messages', []) for b in m.get('content', []) if isinstance(b, dict)]
            text = '\n'.join(b.get('text', '') for b in blocks if b.get('type') == 'text')
            if mode != 'subagent' or text.count('CHECKPOINT-ORIGINAL-ONCE'):
                assert text.count('CHECKPOINT-ORIGINAL-ONCE') == 1
                assert text.count('CHECKPOINT-NOTE-ONCE') == 1
                assert sum(b.get('type') == 'image' for b in blocks) == 1
                assert all(text.count(b) == 1 for b in SIDE_BOUNDARY_BLOCKS)
        if positive:
            after = h.engine.runtime_status(tid)
            assert (after['processId'], after['sessionId'], after['connectionId']) == (before['processId'], before['sessionId'], before['connectionId'])
            assert sum('已自动继续' in json.dumps(x, ensure_ascii=False) for x in h.out) == 1
        assert len([x for x in h.out if x.get('method') == 'turn/completed' and x['params']['threadId'] == tid]) == 1
    finally:
        await h.close()
        server.shutdown()
        server.server_close()
        thread.join()
