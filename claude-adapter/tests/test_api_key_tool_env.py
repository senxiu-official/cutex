"""Opt-in native check: an API account's key never reaches the agent's tool subprocesses.

Set CUTEX_TEST_CLAUDE_CLI to an explicitly selected test-only Claude executable
(e.g. the SDK-bundled copy in a private venv). A fake loopback Anthropic endpoint
answers the first model request with one Bash tool call that dumps the tool's
environment to a file, then ends the turn. Only a synthetic key is used; no real
credential, login or network is involved.
"""
import asyncio
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import threading

import pytest

from cute_claude.config import Settings
from cute_claude.subscription import OfficialSdkDriver
from cute_claude.subscription_auth import ApiAccount, CONFLICTING_ENV
from test_subscription import SubHarness

FAKE_KEY = 'sk-ant-fixture-0000-do-not-use-tool-env-probe'


def sse(events):
    return b''.join(f'event: {e["type"]}\ndata: {json.dumps(e)}\n\n'.encode() for e in events)


def message(blocks, stop):
    events = [{'type': 'message_start', 'message': {
        'id': 'msg_fixture', 'type': 'message', 'role': 'assistant', 'model': 'fixture-model', 'content': [],
        'stop_reason': None, 'stop_sequence': None, 'usage': {'input_tokens': 1, 'output_tokens': 1}}}]
    for i, block in enumerate(blocks):
        if block['type'] == 'tool_use':
            events += [{'type': 'content_block_start', 'index': i, 'content_block': {**block, 'input': {}}},
                       {'type': 'content_block_delta', 'index': i,
                        'delta': {'type': 'input_json_delta', 'partial_json': json.dumps(block['input'])}}]
        else:
            events += [{'type': 'content_block_start', 'index': i, 'content_block': {'type': 'text', 'text': ''}},
                       {'type': 'content_block_delta', 'index': i, 'delta': {'type': 'text_delta', 'text': block['text']}}]
        events.append({'type': 'content_block_stop', 'index': i})
    events += [{'type': 'message_delta', 'delta': {'stop_reason': stop, 'stop_sequence': None}, 'usage': {'output_tokens': 1}},
               {'type': 'message_stop'}]
    return sse(events)


@pytest.mark.skipif(not os.environ.get('CUTEX_TEST_CLAUDE_CLI'), reason='private native CLI not explicitly selected')
@pytest.mark.parametrize('key_env', ['ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN'])
async def test_api_key_stays_out_of_bash_tool_env_and_shell_snapshots(tmp_path, monkeypatch, key_env):
    dump = tmp_path / 'tool-env.txt'
    auth_seen, tool_results = [], []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args): pass

        def do_POST(self):
            raw = self.rfile.read(int(self.headers.get('Content-Length', 0)))
            path = self.path.split('?')[0]
            auth_seen.append(self.headers.get('x-api-key') or self.headers.get('authorization') or '')
            if path.endswith('/messages/count_tokens'):
                return self.reply(json.dumps({'input_tokens': 1}).encode(), 'application/json')
            body = json.loads(raw or b'{}')
            names = {t.get('name') for t in body.get('tools') or []}
            last = (body.get('messages') or [{}])[-1]
            content = last.get('content') if isinstance(last.get('content'), list) else []
            results = [c for c in content if isinstance(c, dict) and c.get('type') == 'tool_result']
            if 'Bash' in names and not results and not tool_results:
                tool_results.append(None)
                out = message([{'type': 'tool_use', 'id': 'toolu_fixture_env', 'name': 'Bash',
                                'input': {'command': f'env > {dump.as_posix()}', 'description': 'dump env'}}], 'tool_use')
            else:
                tool_results.extend(results)
                out = message([{'type': 'text', 'text': 'done'}], 'end_turn')
            self.reply(out, 'text/event-stream')

        def reply(self, raw, ctype):
            self.send_response(200)
            self.send_header('Content-Type', ctype)
            self.send_header('Content-Length', str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    serving = threading.Thread(target=server.serve_forever, daemon=True)
    serving.start()
    private_home = tmp_path / 'home'
    private_home.mkdir()
    monkeypatch.setenv('HOME', str(private_home))
    monkeypatch.setenv('USERPROFILE', str(private_home))
    for name in CONFLICTING_ENV:
        monkeypatch.delenv(name, raising=False)
    key_file = tmp_path / 'keys' / 'ca-loopback.json'  # outside the Claude config dir, as cutex keeps it
    key_file.parent.mkdir()
    key_file.write_text(json.dumps({'apiKey': FAKE_KEY}), encoding='utf-8')
    account = ApiAccount('ca-loopback', key_env, f'http://127.0.0.1:{server.server_port}', {
        'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1', 'DISABLE_TELEMETRY': '1',
        'DISABLE_ERROR_REPORTING': '1', 'ANTHROPIC_DEFAULT_SONNET_MODEL': 'fixture-model',
    }, FAKE_KEY, key_file)
    workspace = tmp_path / 'workspace'
    workspace.mkdir()
    config_dir = private_home / 'claude'
    driver = OfficialSdkDriver(Path(os.environ['CUTEX_TEST_CLAUDE_CLI']), config_dir, api_account=account)
    h = SubHarness(Settings(workspace, tmp_path / 'state', model='sonnet', mock=False, backend='subscription',
                                allow_exec=True, allow_write=True), driver)
    try:
        await h.init()
        tid = await h.thread()
        turn = await h.turn(tid, 'Run the fixture tool.')
        async with asyncio.timeout(120):
            while h.engine.workers:
                await asyncio.gather(*list(h.engine.workers.values()))
        result = h.store.turn(turn)
        assert result['status'] == 'completed', result.get('error')
        # The key still authenticates the model requests ...
        assert auth_seen and all(FAKE_KEY in a for a in auth_seen if a), 'model requests carry the key'
        # ... but the Bash tool ran without it.
        assert dump.is_file(), 'the Bash tool call ran'
        env_text = dump.read_text(encoding='utf-8', errors='replace')
        assert 'PATH=' in env_text
        assert FAKE_KEY not in env_text, 'API key reached the Bash tool environment'
        for name in ('ANTHROPIC_API_KEY=', 'ANTHROPIC_AUTH_TOKEN='):
            assert name not in env_text
        assert FAKE_KEY not in json.dumps(tool_results)
    finally:
        await h.close()
        server.shutdown()
        server.server_close()
        serving.join()
    # Shell snapshots, transcripts and any other file under the CLI's config dir. Scanned after
    # close(): while the harness runs, Windows holds the session lock files exclusively.
    leaked = [path for path in _config_files(config_dir) if FAKE_KEY.encode() in Path(path).read_bytes()]
    assert not leaked, f'key persisted under the config dir: {leaked}'


def _config_files(root: Path):
    """Every file under `root` except lock files; extended-length paths on Windows (temp dirs pass MAX_PATH)."""
    base = os.path.abspath(root)
    if os.name == 'nt' and not base.startswith('\\\\?\\'):
        base = '\\\\?\\' + base
    for directory, _, names in os.walk(base):
        for name in names:
            if not name.endswith('.lock'):
                yield os.path.join(directory, name)
