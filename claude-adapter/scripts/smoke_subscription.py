#!/usr/bin/env python3
"""LIVE subscription smoke: uses your official login and may consume account usage.

This script is supplied for the target machine, NOT marked as validated against
an actual subscription. It uses a temporary workspace, read-only host tools, three
SDK turns, and no API key. Native SDK history remains in the official store.
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

ROOT = Path(__file__).resolve().parents[1]


async def smoke(args):
    completed = {}
    with tempfile.TemporaryDirectory(prefix='cute-claude-subscription-smoke-') as temporary:
        base=Path(temporary)
        workspace=base/'workspace'
        workspace.mkdir()
        nonce=secrets.token_hex(10)
        (workspace/'hello.txt').write_text('SMOKE_VALUE='+nonce+'\n')
        cmd=[sys.executable,'-m','cute_claude','--backend','subscription','--model',args.model,
             '--workspace',str(workspace),'--state-dir',str(base/'state')]
        if args.claude_cli: cmd+=['--claude-cli',str(args.claude_cli)]
        if args.claude_config_dir: cmd+=['--claude-config-dir',str(args.claude_config_dir)]
        env={**os.environ,'PYTHONPATH':str(ROOT/'src')+os.pathsep+os.environ.get('PYTHONPATH','')}
        print('LIVE smoke: uses the official logged-in account; organization usage/billing applies.')
        proc=await asyncio.create_subprocess_exec(*cmd,env=env,stdin=asyncio.subprocess.PIPE,
                    stdout=asyncio.subprocess.PIPE,stderr=asyncio.subprocess.PIPE)
        counter=0
        async def write(m):
            proc.stdin.write((json.dumps(m)+'\n').encode())
            await proc.stdin.drain()
        async def read():
            line=await asyncio.wait_for(proc.stdout.readline(),args.timeout)
            if not line:
                error=(await proc.stderr.read()).decode(errors='replace')
                raise RuntimeError('Server closed: '+error)
            m=json.loads(line)
            if m.get('method')=='turn/completed':
                completed[m['params']['turn']['id']]=m['params']['turn']
            return m
        async def call(method,params=None):
            nonlocal counter
            counter+=1
            await write({'id':counter,'method':method,'params':params or {}})
            while True:
                m=await read()
                if m.get('id')==counter:
                    if 'error' in m: raise RuntimeError(str(m['error']))
                    return m['result']
        async def wait(turn_id):
            async with asyncio.timeout(args.timeout):
                while turn_id not in completed: await read()
            t=completed[turn_id]
            if t['status']!='completed': raise RuntimeError('Turn did not complete: '+str(t.get('error')))
            return t
        try:
            await call('initialize',{'clientInfo':{'name':'subscription-live-smoke','version':'0.3.0'},
                  'capabilities':{'claudeExtensions':True}})
            await write({'method':'initialized'})
            caps=await call('claude/capabilities')
            if caps.get('backend')!='claude-subscription-sdk' or not caps.get('cliDependency'):
                raise RuntimeError('Wrong backend or misleading capability declaration')
            tid=(await call('thread/start'))['thread']['id']
            online=await call('claude/runtime/start',{'threadId':tid})
            if online['state']!='waiting' or not isinstance(online.get('processId'),int):
                raise RuntimeError('Runtime was not online with an observable native PID')
            # This Online request sends no business prompt; first query below.

            prompt='Use the provided read_file tool to read hello.txt. Pass its original parameters inside arguments. Reply with the file contents. Do not guess the contents.'
            turn=(await call('turn/start',{'threadId':tid,'input':[{'type':'text','text':prompt}]}))['turn']['id']
            t=await wait(turn)
            texts='\n'.join(i.get('text','') for i in t['items'] if i['type']=='agentMessage')
            if nonce not in texts: raise RuntimeError('Model did not return the actual test-file contents')
            info=await call('claude/thread/inspect',{'threadId':tid})
            if not any(c['state']=='completed' for c in info['toolCalls']):
                raise RuntimeError('No completed host-tool execution was recorded')
            native_id=info['nativeSession']['session_id']
            payload={'threadId':tid,'messageId':'smoke-job-complete','source':{'kind':'service','id':'smoke-job-service'},
                    'eventType':'job.completed','text':'Test Job completed with result 42. Briefly acknowledge receipt.'}
            receipt=await call('claude/input/submit',payload)
            duplicate=await call('claude/input/submit',payload)
            if not duplicate['duplicate'] or duplicate['turnId']!=receipt['turnId']:
                raise RuntimeError('Inbound deduplication failed')
            await wait(receipt['turnId'])
            after=await call('claude/thread/inspect',{'threadId':tid})
            if after['nativeSession']['session_id']!=native_id or len(after['inputs'])!=2:
                raise RuntimeError('Native session continuation did not match')
            alive=await call('claude/runtime/status',{'threadId':tid})
            if (alive['processId'],alive['sessionId'],alive['connectionId']) != (online['processId'],online['sessionId'],online['connectionId']):
                raise RuntimeError('PID/native identity/connection changed between turns')
            # Unsubscribing is display detach, not native runtime shutdown.
            await call('thread/unsubscribe',{'threadId':tid})
            detached=await call('claude/runtime/status',{'threadId':tid})
            if detached['processId']!=online['processId'] or detached['state']!='waiting':
                raise RuntimeError('Display detach stopped the runtime')
            snapshot=await call('thread/resume',{'threadId':tid})
            if len(snapshot['thread']['turns'])!=2: raise RuntimeError('Resume snapshot mismatch')
            closed=await call('claude/runtime/close',{'threadId':tid})
            if closed.get('processExitConfirmed') is not True:
                raise RuntimeError('SDK disconnect did not provide confirmed child exit')
            restarted=await call('claude/runtime/start',{'threadId':tid})
            if restarted['sessionId']!=native_id or restarted['connectionId']==alive['connectionId']:
                raise RuntimeError('Explicit restart changed native identity or reused old connection')
            check=(await call('turn/start',{'threadId':tid,'input':[{'type':'text','text':'Without reading files or using tools, repeat the SMOKE_VALUE previously read in this conversation.'}]}))['turn']['id']
            verify=await wait(check)
            if nonce not in '\n'.join(i.get('text','') for i in verify['items'] if i['type']=='agentMessage'):
                raise RuntimeError('Resumed native session did not recall prior test content')
            closed=await call('claude/runtime/close',{'threadId':tid})
            if closed.get('processExitConfirmed') is not True:
                raise RuntimeError('Final child exit unverified')
            print('LIVE subscription smoke passed: actual tool, two turns same PID, display detach, explicit close/reap, restart same native ID with third-turn recall, inbox dedup.')
            print('Native SDK transcript remains in the official store; no credential or transcript was copied into this package.')
        finally:
            if proc.stdin: proc.stdin.close()
            try: await asyncio.wait_for(proc.wait(),45)
            except TimeoutError:
                proc.kill()
                await proc.wait()


if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--model',required=True)
    p.add_argument('--claude-cli',type=Path)
    p.add_argument('--claude-config-dir',type=Path)
    p.add_argument('--timeout',type=float,default=240)
    args=p.parse_args()
    try: asyncio.run(smoke(args))
    except (RuntimeError, OSError, TimeoutError) as e:
        print('Live smoke failed: '+str(e),file=sys.stderr)
        raise SystemExit(1) from None
