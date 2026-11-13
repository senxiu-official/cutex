#!/usr/bin/env python3
"""Real SDK180 + CLI321 + adapter stdio, loopback provider and dummy key only.

Run with the SDK180 virtualenv Python; --runtime-root names components/claude-runtime.
No inherited account/provider environment, real credentials, or daemon are used.
Each invocation owns a new isolated directory beside this script (or --output-root).
"""
from __future__ import annotations

import argparse
import asyncio
import importlib.metadata
import importlib.util
import json
import os
from pathlib import Path
import secrets
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class CheckFailure(Exception):
    pass


def require(ok, message):
    if not ok:
        raise CheckFailure(message)


class Provider:
    def __init__(self, key, readable):
        self.key = key
        self.readable = readable
        self.mode = 'simple'
        self.calls = []
        self.phase_calls = 0
        self.started = threading.Event()
        self.release = threading.Event()
        self.marker = ''
        self.server = None

    def phase(self, mode, marker=''):
        self.mode = mode
        self.marker = marker
        self.phase_calls = 0
        self.started.clear()
        self.release.clear()

    def start(self):
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                self.reply(200, {'data': [], 'has_more': False})

            def reply(self, status, value, headers=None):
                body = json.dumps(value).encode()
                self.send_response(status)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(body)))
                for name, value in (headers or {}).items():
                    self.send_header(name, value)
                try:
                    self.end_headers()
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                    pass

            def do_POST(self):
                size = int(self.headers.get('Content-Length', '0'))
                if not 0 <= size <= 16 * 1024 * 1024:
                    return self.reply(413, {'type': 'error'})
                try:
                    data = json.loads(self.rfile.read(size))
                except (ValueError, OSError):
                    return self.reply(400, {'type': 'error'})
                if '/messages' not in self.path or '/count_tokens' in self.path:
                    return self.reply(200, {'input_tokens': 1})
                mode = owner.mode
                owner.phase_calls += 1
                number = owner.phase_calls
                serialized = json.dumps(data.get('messages', []), ensure_ascii=False)
                auth_ok = (self.headers.get('x-api-key') == owner.key
                           or self.headers.get('Authorization') == 'Bearer ' + owner.key)
                owner.calls.append({'phase': mode, 'number': number, 'dummyAuthMatched': auth_ok,
                                    'markerPresent': bool(owner.marker and owner.marker in serialized),
                                    'stream': bool(data.get('stream')), 'status': 429 if mode == 'limit' else 200})
                if not auth_ok:
                    return self.reply(401, {'type': 'error', 'error': {'type': 'authentication_error', 'message': 'fixture auth mismatch'}})
                if mode == 'limit':
                    return self.reply(429, {'type': 'error', 'error': {'type': 'rate_limit_error',
                        'message': 'Local fixture usage limit reached; Continue after reset.'}},
                        {'retry-after': '0', 'anthropic-ratelimit-unified-status': 'rejected',
                         'anthropic-ratelimit-unified-reset': str(int(time.time()) + 1)})
                use_tool = mode in ('steer', 'service_insert', 'interrupt_insert') and number == 1
                if use_tool:
                    owner.started.set()
                    if not owner.release.wait(60):
                        return self.reply(503, {'type': 'error', 'error': {'type': 'overloaded_error', 'message': 'fixture gate timed out'}})
                text = 'LOCAL_FIXTURE_COMPLETE'
                content = ({'type': 'tool_use', 'id': 'tool_fixture_' + mode,
                            'name': 'Read', 'input': {'file_path': str(owner.readable)}} if use_tool
                           else {'type': 'text', 'text': text})
                stop = 'tool_use' if use_tool else 'end_turn'
                if not data.get('stream'):
                    return self.reply(200, {'id': 'msg_fixture', 'type': 'message', 'role': 'assistant',
                        'model': data.get('model', 'claude-sonnet-4-6'), 'content': [content],
                        'stop_reason': stop, 'stop_sequence': None, 'usage': {'input_tokens': 10, 'output_tokens': 5}})
                start = dict(content)
                start['input' if use_tool else 'text'] = {} if use_tool else ''
                delta = ({'type': 'input_json_delta', 'partial_json': json.dumps(content['input'])}
                         if use_tool else {'type': 'text_delta', 'text': text})
                events = [
                    ('message_start', {'type': 'message_start', 'message': {'id': f'msg_fixture_{mode}_{number}',
                        'type': 'message', 'role': 'assistant', 'model': data.get('model', 'claude-sonnet-4-6'),
                        'content': [], 'stop_reason': None, 'stop_sequence': None,
                        'usage': {'input_tokens': 10, 'output_tokens': 0}}}),
                    ('content_block_start', {'type': 'content_block_start', 'index': 0, 'content_block': start}),
                    ('content_block_delta', {'type': 'content_block_delta', 'index': 0, 'delta': delta}),
                    ('content_block_stop', {'type': 'content_block_stop', 'index': 0}),
                    ('message_delta', {'type': 'message_delta', 'delta': {'stop_reason': stop, 'stop_sequence': None},
                                       'usage': {'output_tokens': 5}}),
                    ('message_stop', {'type': 'message_stop'}),
                ]
                body = ''.join(f'event: {event}\ndata: {json.dumps(value)}\n\n' for event, value in events).encode()
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Content-Length', str(len(body)))
                try:
                    self.end_headers()
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                    pass

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        return f'http://127.0.0.1:{self.server.server_port}'

    def close(self):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=3)


class Rpc:
    def __init__(self, process, timeout):
        self.process = process
        self.timeout = timeout
        self.counter = 0
        self.pending = {}
        self.events = []
        self.closed = False
        self.stderr_bytes = 0
        self.reader = asyncio.create_task(self.read())
        self.stderr_reader = asyncio.create_task(self.drain_stderr())

    async def drain_stderr(self):
        while block := await self.process.stderr.read(4096):
            self.stderr_bytes += len(block)

    async def read(self):
        try:
            while line := await self.process.stdout.readline():
                value = json.loads(line)
                if value.get('id') in self.pending:
                    future = self.pending.pop(value['id'])
                    if not future.done():
                        future.set_result(value)
                else:
                    self.events.append(value)
        finally:
            self.closed = True
            for future in self.pending.values():
                if not future.done():
                    future.set_exception(CheckFailure('adapter exited before RPC response'))

    async def send(self, value):
        self.process.stdin.write((json.dumps(value) + '\n').encode())
        await self.process.stdin.drain()

    async def call(self, method, params=None):
        self.counter += 1
        future = asyncio.get_running_loop().create_future()
        self.pending[self.counter] = future
        await self.send({'id': self.counter, 'method': method, 'params': params or {}})
        answer = await asyncio.wait_for(future, self.timeout)
        require('error' not in answer, 'RPC rejected: ' + method)
        return answer['result']

    async def wait_turn(self, turn):
        async with asyncio.timeout(self.timeout):
            while True:
                for event in self.events:
                    if event.get('method') == 'turn/completed' and event['params']['turn']['id'] == turn:
                        return event['params']['turn']
                require(not self.closed, 'adapter exited before turn completed')
                await asyncio.sleep(.02)

    async def turn(self, tid, text, client_id=None):
        params = {'threadId': tid, 'input': [{'type': 'text', 'text': text}]}
        if client_id:
            params['clientUserMessageId'] = client_id
        return (await self.call('turn/start', params))['turn']['id']


async def run(args):
    import sys
    require(importlib.metadata.version('claude-agent-sdk') == '0.2.180', 'requires installed SDK 0.2.180')
    bundled = Path(importlib.util.find_spec('claude_agent_sdk').origin).parent / '_bundled'
    cli = bundled / ('claude.exe' if os.name == 'nt' else 'claude')
    require(cli.is_file(), 'bundled CLI missing')
    root = args.output_root.resolve() / ('smoke-' + secrets.token_hex(6))
    root.mkdir(parents=True, mode=0o700)
    dirs = {name: root / name for name in ('home', 'appdata', 'local', 'temp', 'config', 'workspace', 'state')}
    for directory in dirs.values():
        directory.mkdir(mode=0o700)
    readable = dirs['workspace'] / 'fixture.txt'
    readable.write_text('LOCAL_FIXTURE_FILE\n')
    key = 'fixture-only-' + secrets.token_hex(24)
    provider = Provider(key, readable)
    base = provider.start()
    key_file = root / 'dummy-key.json'
    key_file.write_text(json.dumps({'apiKey': key}))
    key_file.chmod(0o600)
    account = root / 'account.json'
    account.write_text(json.dumps({'version': 1, 'accountId': 'local-fixture', 'keyEnv': 'ANTHROPIC_API_KEY',
        'baseUrl': base, 'keyFile': str(key_file), 'env': {'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1',
        'DISABLE_TELEMETRY': '1', 'DISABLE_ERROR_REPORTING': '1'}}))
    allowed = ('PATH', 'SystemRoot', 'SYSTEMROOT', 'WINDIR', 'COMSPEC', 'PATHEXT', 'SYSTEMDRIVE')
    env = {name: os.environ[name] for name in allowed if name in os.environ}
    env.update({'HOME': str(dirs['home']), 'USERPROFILE': str(dirs['home']),
                'APPDATA': str(dirs['appdata']), 'LOCALAPPDATA': str(dirs['local']),
                'TMP': str(dirs['temp']), 'TEMP': str(dirs['temp']), 'TMPDIR': str(dirs['temp']),
                'XDG_CONFIG_HOME': str(dirs['home'] / '.config'), 'XDG_CACHE_HOME': str(dirs['home'] / '.cache'),
                'XDG_DATA_HOME': str(dirs['home'] / '.local/share'), 'XDG_STATE_HOME': str(dirs['home'] / '.local/state'),
                'PYTHONDONTWRITEBYTECODE': '1', 'PYTHONNOUSERSITE': '1',
                'PYTHONPATH': str(args.runtime_root.resolve() / 'src'),
                'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1', 'DISABLE_AUTOUPDATER': '1'})
    version_proc = await asyncio.create_subprocess_exec(str(cli), '--version', env=env,
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL)
    version_bytes, _ = await asyncio.wait_for(version_proc.communicate(), 30)
    require(version_proc.returncode == 0 and version_bytes.decode().split()[0] == '2.1.321', 'requires bundled CLI 2.1.321')
    command = [sys.executable, '-B', '-m', 'cute_claude', '--backend', 'subscription',
               '--claude-cli', str(cli), '--claude-config-dir', str(dirs['config']),
               '--api-account-config', str(account), '--workspace', str(dirs['workspace']),
               '--state-dir', str(dirs['state']), '--model', 'claude-sonnet-4-6', '--thinking', 'disabled']
    report = {'sdk': '0.2.180', 'cli': '2.1.321', 'adapterPid': None, 'cases': [], 'success': False,
              'scope': 'real SDK/CLI/adapter; loopback API fixture only, not claude.ai account quota'}
    process = rpc = None
    try:
        process = await asyncio.create_subprocess_exec(*command, env=env, cwd=dirs['workspace'],
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            limit=16 * 1024 * 1024)
        report['adapterPid'] = process.pid
        rpc = Rpc(process, args.timeout)
        await rpc.call('initialize', {'clientInfo': {'name': 'private-sdk175-smoke', 'version': '1'},
                                     'capabilities': {'claudeExtensions': True}})
        await rpc.send({'method': 'initialized'})
        tid = (await rpc.call('thread/start'))['thread']['id']
        online = await rpc.call('claude/runtime/start', {'threadId': tid})
        require(isinstance(online.get('processId'), int), 'native process PID missing')
        report['nativePid'] = online['processId']
        report['initialState'] = online['state']
        identity = tuple(online.get(k) for k in ('processId', 'sessionId', 'connectionId'))
        for case in args.cases.split(','):
            if case == 'simple':
                provider.phase('simple')
                turn = await rpc.turn(tid, 'Reply briefly. This is a local test fixture.')
                done = await rpc.wait_turn(turn)
                require(done['status'] == 'completed', 'simple turn did not complete')
                require(any(item.get('type') == 'agentMessage' and 'LOCAL_FIXTURE_COMPLETE' in item.get('text', '')
                            for item in done.get('items', [])), 'simple assistant text missing')
                report['cases'].append({'case': case, 'completed': True, 'requests': provider.phase_calls})
            elif case == 'limit':
                provider.phase('limit')
                turn = await rpc.turn(tid, 'Perform the local fixture task before quota reset.')
                done = await rpc.wait_turn(turn)
                require(done['status'] == 'failed', '429 must fail the turn')
                limits = provider.phase_calls
                await asyncio.sleep(.5)
                require(provider.phase_calls == limits, 'adapter retried quota failure without Continue')
                provider.phase('continue')
                continued = await rpc.turn(tid, 'Continue after the local fixture quota reset.')
                done = await rpc.wait_turn(continued)
                require(done['status'] == 'completed', 'Continue after 429 did not complete')
                report['cases'].append({'case': case, 'failedThenContinueCompleted': True, 'limitRequests': limits,
                                        'continueRequests': provider.phase_calls, 'distinctTurn': continued != turn})
            elif case == 'interrupt_insert':
                marker = 'LOCAL_OWNER_INSERT_' + secrets.token_hex(5)
                provider.phase(case)
                turn = await rpc.turn(tid, 'Read fixture.txt using Read, then finish the old instruction.')
                require(await asyncio.to_thread(provider.started.wait, args.timeout), 'owner insert request gate not reached')
                # Deliver interrupt while the real CLI awaits its first model
                # response. Release the HTTP fixture afterward so cancellation
                # can also finish on CLIs that drain the pending response.
                interrupt = asyncio.create_task(rpc.call('turn/interrupt', {'threadId': tid, 'turnId': turn}))
                await asyncio.sleep(.15)
                provider.release.set()
                await interrupt
                stopped = await rpc.wait_turn(turn)
                require(stopped['status'] == 'interrupted', 'owner insert did not interrupt the old turn')
                provider.phase('after_interrupt', marker)
                inserted = await rpc.turn(tid, 'Owner inserted instruction. ' + marker, 'owner-insert-fixture')
                done = await rpc.wait_turn(inserted)
                require(done['status'] == 'completed', 'owner inserted turn did not complete')
                require(inserted != turn, 'owner insert must start a distinct turn')
                require(any(row['phase'] == 'after_interrupt' and row['markerPresent'] for row in provider.calls),
                        'owner inserted instruction did not reach actual native model request')
                report['cases'].append({'case': case, 'oldTurnInterrupted': True, 'newTurnCompleted': True,
                                        'distinctTurn': True, 'insertRequests': provider.phase_calls})
            elif case in ('steer', 'service_insert'):
                marker = 'LOCAL_' + case.upper() + '_' + secrets.token_hex(5)
                provider.phase(case, marker)
                turn = await rpc.turn(tid, 'Read fixture.txt using the Read tool, then finish.')
                require(await asyncio.to_thread(provider.started.wait, args.timeout), 'tool request gate not reached')
                if case == 'steer':
                    await rpc.call('turn/steer', {'threadId': tid, 'expectedTurnId': turn,
                        'clientUserMessageId': 'steer-fixture', 'input': [{'type': 'text', 'text': marker}]})
                else:
                    # Insert a service/peer result at the next authenticated tool
                    # boundary. This is not the daemon's interrupt-and-send UI path.
                    accepted = await rpc.call('claude/input/submit', {'threadId': tid,
                        'messageId': 'cutex-ask-soon-fixture', 'source': {'kind': 'service', 'id': 'cutex-asks'},
                        'eventType': 'ANSWER', 'text': marker, 'admission': 'next_step',
                        'metadata': {'deliveryMode': 'soon', 'asks': ['local-ask']}})
                    require(not accepted.get('contextPersisted'), 'input acknowledged before native consumption')
                provider.release.set()
                done = await rpc.wait_turn(turn)
                require(done['status'] == 'completed', case + ' turn did not complete')
                require(any(row['phase'] == case and row['number'] > 1 and row['markerPresent'] for row in provider.calls),
                        case + ' marker did not reach next native model request')
                if case == 'service_insert':
                    receipt = await rpc.call('claude/input/status', {'threadId': tid, 'messageId': 'cutex-ask-soon-fixture'})
                    require(receipt.get('contextPersisted') and receipt.get('contextTurnId') == turn,
                            'insert receipt missing or attributed to wrong turn')
                report['cases'].append({'case': case, 'sameTurnNativeContext': True, 'requests': provider.phase_calls})
            else:
                raise CheckFailure('unsupported case')
            status = await rpc.call('claude/runtime/status', {'threadId': tid})
            require(tuple(status.get(k) for k in ('processId', 'sessionId', 'connectionId')) == identity,
                    'native PID/session/connection identity changed')
        closed = await rpc.call('claude/runtime/close', {'threadId': tid})
        require(closed.get('processExitConfirmed') is True, 'native exit not confirmed')
        report['nativeExitConfirmed'] = True
        report['success'] = True
    except BaseException as exc:
        # Never include raw RPC error bodies, CLI stderr, HTTP payloads, or key.
        report['failureType'] = type(exc).__name__
        report['failure'] = str(exc) if isinstance(exc, CheckFailure) else 'operation failed; raw diagnostics withheld'
    finally:
        provider.release.set()
        if process:
            if process.stdin:
                process.stdin.close()
            try:
                await asyncio.wait_for(process.wait(), 35)
            except TimeoutError:
                process.kill()
                await process.wait()
                report['adapterForcedKill'] = True
            report['adapterExitCode'] = process.returncode
        if rpc:
            await asyncio.gather(rpc.reader, rpc.stderr_reader, return_exceptions=True)
            report['stderrBytesSuppressed'] = rpc.stderr_bytes
            report['eventCounts'] = {name: sum(event.get('method') == name for event in rpc.events)
                                     for name in ('turn/started', 'turn/completed', 'claude/input/contextPersisted')}
        provider.close()
        report['requests'] = provider.calls
        key_file.unlink(missing_ok=True)
        report['dummyKeyDeleted'] = True
        (root / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps({'result': str(root / 'result.json'), **report}, indent=2))
    return 0 if report['success'] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--runtime-root', required=True, type=Path)
    parser.add_argument('--output-root', type=Path, default=Path(__file__).resolve().parent)
    parser.add_argument('--cases', default='simple', help='comma-separated simple,limit,steer,service_insert,interrupt_insert')
    parser.add_argument('--timeout', type=float, default=150)
    return asyncio.run(run(parser.parse_args()))


if __name__ == '__main__':
    raise SystemExit(main())
