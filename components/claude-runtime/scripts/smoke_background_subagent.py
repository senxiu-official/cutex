#!/usr/bin/env python3
"""LIVE smoke of background subagents (claude-r23): uses your official login and some account usage.

A temporary workspace and adapter state; the model of your choice (haiku is enough). Checks that
a background subagent spawned in one turn keeps working after that turn ends (Read, Bash, Write),
that the main agent answers a second turn while it runs, that its task end carries `notifyParent`
(completed, with its reply), that its transcript is readable and closed, and that the runtime then
closes cleanly. Prints marker evidence only.
"""
from __future__ import annotations
import argparse
import asyncio
import json
import os
from pathlib import Path
import secrets
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


async def smoke(args):
    completed, task_events = {}, []
    with tempfile.TemporaryDirectory(prefix='cute-claude-background-smoke-') as temporary:
        base = Path(temporary)
        workspace = base / 'workspace'
        workspace.mkdir()
        nonce = secrets.token_hex(6)
        (workspace / 'a.txt').write_text(f'alpha {nonce}\n')
        cmd = [sys.executable, '-m', 'cute_claude', '--backend', 'subscription', '--model', args.model,
               '--workspace', str(workspace), '--state-dir', str(base / 'state'), '--allow-write', '--allow-exec']
        if args.claude_cli:
            cmd += ['--claude-cli', str(args.claude_cli)]
        if args.claude_config_dir:
            cmd += ['--claude-config-dir', str(args.claude_config_dir)]
        env = {**os.environ, 'PYTHONPATH': str(ROOT / 'src') + os.pathsep + os.environ.get('PYTHONPATH', '')}
        print('LIVE smoke: uses the official logged-in account; usage applies.')
        proc = await asyncio.create_subprocess_exec(*cmd, env=env, stdin=asyncio.subprocess.PIPE,
                                                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
                                                    limit=64 * 1024 * 1024)
        counter = 0
        started = time.monotonic()

        def mark(text):
            print(f'[{time.monotonic() - started:6.1f}s] {text}', flush=True)

        async def write(m):
            proc.stdin.write((json.dumps(m) + '\n').encode())
            await proc.stdin.drain()

        async def read():
            line = await asyncio.wait_for(proc.stdout.readline(), args.timeout)
            if not line:
                raise RuntimeError('Server closed: ' + (await proc.stderr.read()).decode(errors='replace')[-2000:])
            m = json.loads(line)
            if m.get('method') == 'turn/completed' and not str(m['params']['threadId']).startswith('claude:'):
                completed[m['params']['turn']['id']] = m['params']['turn']
            if m.get('method') == 'claude/task/updated':
                task_events.append(m['params']['task'])
            return m

        async def call(method, params=None):
            nonlocal counter
            counter += 1
            await write({'id': counter, 'method': method, 'params': params or {}})
            while True:
                m = await read()
                if m.get('id') == counter:
                    if 'error' in m:
                        raise RuntimeError(f'{method}: {m["error"]}')
                    return m['result']

        async def until(predicate, what):
            async with asyncio.timeout(args.timeout):
                while not predicate():
                    await read()
            mark(what)

        try:
            await call('initialize', {'clientInfo': {'name': 'background-live-smoke', 'version': '0.8.0'},
                                      'capabilities': {'claudeExtensions': True}})
            await write({'method': 'initialized'})
            caps = await call('claude/capabilities')
            assert caps['input']['backgroundSubagents'] is True, 'adapter lacks backgroundSubagents'
            tid = (await call('thread/start'))['thread']['id']
            await call('claude/runtime/start', {'threadId': tid})
            prompt = ('Use the Agent tool with run_in_background: true and subagent_type general-purpose to start ONE '
                      'background subagent. Its task: read a.txt, run `sleep 20` with Bash, then write the file '
                      'summary.txt containing the exact contents of a.txt, and reply with those contents. After '
                      'launching it, do not wait: reply only "launched" and end your turn.')
            first = (await call('turn/start', {'threadId': tid, 'input': [{'type': 'text', 'text': prompt}]}))['turn']['id']
            await until(lambda: first in completed, 'origin turn completed')
            assert completed[first]['status'] == 'completed', completed[first]
            agent_tasks = [t for t in task_events if t.get('taskType') == 'local_agent']
            assert agent_tasks and agent_tasks[0].get('background'), f'no background task: {task_events}'
            task_id = agent_tasks[0]['taskId']
            mark(f'background task {task_id} running after its turn')
            second = (await call('turn/start', {'threadId': tid, 'input': [{'type': 'text', 'text': 'Reply only PONG.'}]}))['turn']['id']
            await until(lambda: second in completed, 'second turn completed while the subagent runs')
            assert not any(t.get('notifyParent') for t in task_events), 'the subagent ended before the second turn'
            await until(lambda: any(t.get('notifyParent') for t in task_events), 'notifyParent received')
            notice = next(t['notifyParent'] for t in task_events if t.get('notifyParent'))
            assert notice['status'] == 'completed' and notice['taskId'] == task_id, notice
            assert notice['summary'] and nonce in notice['summary'], notice
            assert (workspace / 'summary.txt').read_text().strip().endswith(nonce), 'the subagent did not write after its turn'
            ref = notice['transcript']
            thread = (await call('thread/read', {'threadId': ref, 'includeTurns': True}))['thread']
            kinds = [i['type'] for i in thread['turns'][0]['items']]
            assert thread['status']['type'] == 'idle' and thread['turns'][0]['status'] == 'completed', thread['status']
            assert kinds.count('commandExecution') >= 2, kinds  # Read + Bash (the write may be Write or Bash)
            parent = (await call('thread/read', {'threadId': tid, 'includeTurns': True}))['thread']
            usage = [i for t in parent['turns'] for i in t['items'] if i.get('type') == 'commandExecution']
            assert not usage, 'child tools leaked into the parent timeline'
            mark(f'transcript {len(kinds)} items, parent timeline clean, duration {thread["turns"][0]["durationMs"]} ms')
            # Claude Code's own notification turn: give it time to end, then close.
            await asyncio.sleep(8)
            closed = await call('claude/runtime/close', {'threadId': tid})
            assert closed.get('processExitConfirmed') is True, 'child exit unverified'
            print('LIVE background-subagent smoke passed: ran after its turn, main agent free meanwhile, '
                  'notifyParent completed with its reply, transcript closed, parent timeline clean.')
        finally:
            if proc.stdin:
                proc.stdin.close()
            try:
                await asyncio.wait_for(proc.wait(), 45)
            except TimeoutError:
                proc.kill()
                await proc.wait()


if __name__ == '__main__':
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--model', required=True)
    p.add_argument('--claude-cli', type=Path)
    p.add_argument('--claude-config-dir', type=Path)
    p.add_argument('--timeout', type=float, default=240)
    args = p.parse_args()
    try:
        asyncio.run(smoke(args))
    except (AssertionError, RuntimeError, OSError, TimeoutError) as e:
        print('Live smoke failed: ' + str(e), file=sys.stderr)
        raise SystemExit(1) from None
