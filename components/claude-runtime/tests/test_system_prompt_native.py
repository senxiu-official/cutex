"""Opt-in real SDK/CLI probe of a resumed session's system prompt against a loopback model; never real credentials.

CUTEX_SYSTEM_PROMPT_NATIVE_SMOKE=1 python -m pytest -q -s tests/test_system_prompt_native.py

Claude Code records a session's system prompt on its first request and sends
that record on every resume even when `--append-system-prompt` changed
(`systemPromptSnapshot`, default on), until a compaction. The adapter turns the
snapshot off for a resume whose prompt changed (system_prompt_option).
"""
import asyncio
import importlib.util
import json
import os
from pathlib import Path
import threading
import subprocess
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from cute_claude.config import Settings
from cute_claude.sdk_runtime import PROMPT_RECORD, prompt_digest
from cute_claude.subscription import OfficialSdkDriver
from cute_claude.subscription_auth import ApiAccount, CONFLICTING_ENV
from test_subscription import SubHarness
from prompt_fixture import large_append, memory_index, LAST_LINE, AGENTS_MARKER

pytestmark = pytest.mark.skipif(os.environ.get('CUTEX_SYSTEM_PROMPT_NATIVE_SMOKE') != '1', reason='opt-in local SDK/CLI probe')

MARKERS = ('ROLE_ALPHA_17', 'ROLE_BETA_29')


@pytest.mark.parametrize('large,subagent,transport,idle_note', [(False, False, 'file', False), (True, False, 'file', False), (True, True, 'file', False), (True, True, 'inline-control', False), (True, False, 'file', True)])
async def test_real_cli_resume_sends_the_changed_system_prompt(tmp_path, monkeypatch, large, subagent, transport, idle_note):
    if transport == 'inline-control':
        # Original adapter transport, kept small enough for a Windows argv.
        monkeypatch.setattr('cute_claude.subscription.file_backed_system_prompt', lambda option, *_: (option, {}))
    for name in CONFLICTING_ENV:
        monkeypatch.delenv(name, raising=False)
    for name in ('HOME', 'USERPROFILE', 'APPDATA', 'LOCALAPPDATA'):
        isolated = tmp_path / name.lower(); isolated.mkdir()
        monkeypatch.setenv(name, str(isolated))
    work = tmp_path / 'workspace'; work.mkdir()
    requests = []
    command_lengths = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            data = json.loads(self.rfile.read(int(self.headers.get('Content-Length', '0'))))
            if '/messages' not in self.path or '/count_tokens' in self.path:
                body = json.dumps({'input_tokens': 1}).encode()
                self.send_response(200); self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(body))); self.end_headers(); self.wfile.write(body)
                return
            requests.append(data)
            use_tool = subagent and len(requests) == 1
            block = ({'type':'tool_use','id':'call_agent','name':'Agent','input':{}}
                     if use_tool else {'type':'text','text':''})
            delta = ({'type':'input_json_delta','partial_json':json.dumps({'subagent_type':'general-purpose',
                        'description':'inspect inherited system prompt','prompt':'CHILD_PROMPT_PROBE_98; answer directly without tools.'})}
                     if use_tool else {'type':'text_delta','text':f'answer {len(requests)}'})
            events = [
                ('message_start', {'type': 'message_start', 'message': {'id': f'msg_{len(requests)}', 'type': 'message',
                    'role': 'assistant', 'model': 'claude-sonnet-4-6', 'content': [], 'stop_reason': None,
                    'stop_sequence': None, 'usage': {'input_tokens': 10, 'output_tokens': 0}}}),
                ('content_block_start', {'type': 'content_block_start', 'index': 0, 'content_block': block}),
                ('content_block_delta', {'type': 'content_block_delta', 'index': 0,
                                         'delta': delta}),
                ('content_block_stop', {'type': 'content_block_stop', 'index': 0}),
                ('message_delta', {'type': 'message_delta', 'delta': {'stop_reason': 'tool_use' if use_tool else 'end_turn', 'stop_sequence': None},
                                   'usage': {'output_tokens': 5}}),
                ('message_stop', {'type': 'message_stop'}),
            ]
            body = ''.join(f'event: {name}\ndata: {json.dumps(value)}\n\n' for name, value in events).encode()
            self.send_response(200); self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Content-Length', str(len(body))); self.end_headers(); self.wfile.write(body)

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    base = f'http://127.0.0.1:{server.server_address[1]}'
    thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
    bundled = Path(importlib.util.find_spec('claude_agent_sdk').origin).parent / '_bundled'
    cli = bundled / ('claude.exe' if os.name == 'nt' else 'claude')
    key_file = tmp_path / 'keys' / 'loopback.json'
    key_file.parent.mkdir()
    key_file.write_text(json.dumps({'apiKey': 'local-fixture-not-a-real-key'}), encoding='utf-8')
    account = ApiAccount('loopback-fixture', 'ANTHROPIC_API_KEY', base,
        {'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1', 'DISABLE_TELEMETRY': '1', 'DISABLE_ERROR_REPORTING': '1'},
        'local-fixture-not-a-real-key', key_file)
    driver = OfficialSdkDriver(cli, tmp_path / 'native-config', api_account=account)
    append = (memory_index() + AGENTS_MARKER if transport == 'inline-control' else large_append()) if large else None
    h = SubHarness(Settings(work, tmp_path / 'adapter', model='claude-sonnet-4-6', backend='subscription', mock=False,
                            thinking='disabled', system_append=append,
                            external_owner_id='owner' if idle_note else None,
                            external_runtime_generation=7 if idle_note else None), driver)

    async def drain():
        async with asyncio.timeout(60):
            while h.engine.workers:
                await asyncio.gather(*list(h.engine.workers.values()))

    async def ask(tid, label, instructions=None):
        """One turn in a fresh Claude Code process (a resume after the first); the markers its system prompt had."""
        if h.engine.session(tid)['attempted']:
            await h.call('claude/runtime/close', {'threadId': tid})
            if instructions is not None:
                h.store.db.execute('UPDATE threads SET instructions=? WHERE id=?', (instructions, tid))
            await h.call('claude/runtime/start', {'threadId': tid})
        await h.turn(tid, label); await drain()
        request = next(r for r in reversed(requests) if label in json.dumps(r['messages'][-1]))
        if large:
            system = '\n'.join(b['text'] for b in request['system'] if b.get('type') == 'text')
            assert LAST_LINE in system and memory_index().rstrip() in system
            assert AGENTS_MARKER in system
            client = h.engine.driver.owners[tid].client
            command = client._transport._build_command()
            assert ('--append-system-prompt-file' in command) == (transport == 'file')
            assert ('--append-system-prompt' in command) == (transport == 'inline-control')
            assert len(subprocess.list2cmdline(command).encode('utf-16-le')) // 2 < 32767
            command_lengths.append(len(subprocess.list2cmdline(command).encode('utf-16-le')) // 2)
        return [m for m in MARKERS if m in json.dumps(request['system'])]

    try:
        await h.init()
        tid = await h.thread()
        h.store.db.execute('UPDATE threads SET instructions=? WHERE id=?', (MARKERS[0], tid))
        assert await ask(tid, 'T1') == ['ROLE_ALPHA_17']
        if idle_note:
            from test_r8_services import envelope, binding
            from cute_claude.external_input import semantic_digest
            notice = envelope(tid, 'memory-index-native-probe', passive=True)
            notice['message'].update(source={'kind':'service','id':'cutex-memory'}, type='memory_index_changed',
                text='Cutex memory notice: MEMORY.md changed. Read MEMORY.md.\n- [new](new.md) — INDEX_UPDATED_SENTINEL')
            notice['semanticSha256'] = semantic_digest(notice)
            first_client = h.engine.driver.owners[tid].client
            first_system = requests[-1]['system']
            count = len(requests)
            await h.call('thread/externalInput/submit', notice)
            await asyncio.sleep(0.05)
            assert len(requests) == count and not h.engine.workers, 'passive memory note started a turn'
            note_turn = await h.turn(tid, 'OWNER_ORIGINAL_NOTE_TURN')
            await drain()
            assert h.engine.driver.owners[tid].client is first_client, 'note restarted the runtime'
            assert requests[-1]['system'] == first_system, 'note rewrote the system prompt'
            assert 'INDEX_UPDATED_SENTINEL' in json.dumps(requests[-1]['messages'], ensure_ascii=False)
            items = h.store.turn(note_turn)['items']
            users = [i for i in items if i['type'] == 'userMessage']
            assert len(users) == 1 and 'INDEX_UPDATED_SENTINEL' not in json.dumps(users)
            notes = [i for i in items if i['type'] == 'functionCallOutput' and i.get('name') == 'external_event']
            assert len(notes) == 1 and 'cutex-memory' in json.dumps(notes)
            status = await h.call('thread/externalInput/status', {**binding(tid),
                'messages':[{'messageId':notice['message']['id'],'semanticSha256':notice['semanticSha256']}]})
            assert status['statuses'][0]['deliveryState'] == 'context_persisted'
            print(json.dumps({'probe':'memory-idle-note-real-cli','passiveNoWake':True,'sameSystemPrompt':True,
                              'sameNativeClient':True,'ownerInputUnchanged':True,'serviceRows':len(notes)}))
        if subagent:
            children = [r for r in requests if any('CHILD_PROMPT_PROBE_98' in str(m.get('content'))
                         for m in r['messages'] if m['role'] == 'user')]
            assert children, 'foreground native Agent tool never reached the loopback provider'
            # Claude Code gives a foreground Agent its own prompt, for both the
            # original text flag and the file flag. Transport must preserve it.
            assert all(LAST_LINE not in json.dumps(r['system'], ensure_ascii=False) for r in children)
        assert await ask(tid, 'T2') == ['ROLE_ALPHA_17']          # Unchanged: the record is reused.
        assert await ask(tid, 'T3', MARKERS[1]) == ['ROLE_BETA_29']  # Changed: rendered fresh.
        assert await ask(tid, 'T4') == ['ROLE_BETA_29']          # Still the new text on the next resume.

        # Control: claiming the record is current shows what the CLI does with the snapshot on.
        props = h.store.thread_properties(tid)
        h.store.set_thread_properties(tid, {**props, PROMPT_RECORD: {'digest': prompt_digest(
            h.engine.settings.preset_append(MARKERS[1]))}})
        assert await ask(tid, 'T5') == ['ROLE_ALPHA_17']          # The first request's record, not the launch text.
        h.store.set_thread_properties(tid, props)

        # A compaction re-records the launch prompt; later resumes keep the snapshot again.
        await h.call('thread/compact/start', {'threadId': tid}); await drain()
        assert h.store.thread_properties(tid)[PROMPT_RECORD]['digest'] == prompt_digest(h.engine.settings.preset_append(MARKERS[1]))
        assert await ask(tid, 'T6') == ['ROLE_BETA_29']
        assert 'snapshot' not in h.engine.driver.owners[tid].client.options.system_prompt
        print(json.dumps({'probe': 'real-sdk-cli-loopback', 'kind': 'system-prompt-resume', 'modelRequests': len(requests),
                          'changedPromptReachedModel': True, 'snapshotOnSendsFirstRecord': True,
                          'indexBytes':25*1024 if large else 0, 'indexLines':200 if large else 0,
                          'lastChineseLineInSystem':large, 'fullAgentsInSystem':large,
                          'foregroundSubagent':subagent,
                          'transport':transport, 'nativeChildUsesOwnPrompt':subagent,
                          'maxCommandUtf16Units':max(command_lengths, default=0),
                          'appendUtf16Units':len(append.encode('utf-16-le'))//2 if append else 0}))
    finally:
        await h.close()
        server.shutdown(); server.server_close(); thread.join(timeout=2)
