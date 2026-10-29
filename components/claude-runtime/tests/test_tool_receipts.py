import asyncio
import json
from types import SimpleNamespace

import pytest

from cute_claude.cutex_tools import receipt_arguments, tool_timeout
from cute_claude.tools import ToolRegistry
from cute_claude.cutex_tools import load


def test_budgets_outlive_preflight_and_bus():
    assert tool_timeout('agent_create', None, 30) == 150
    assert tool_timeout('agent_create', 120, 30) == 270
    assert tool_timeout('agent_delegate', 120, 30) == 180
    assert tool_timeout('agent_create', 120, 600) == 600


def test_stable_call_key_and_explicit_replay():
    ctx = SimpleNamespace(thread_id='t', turn_id='u', call_id='c')
    first = receipt_arguments('agent_create', {'name': 'one'}, ctx)
    assert first['action_id'] == receipt_arguments('agent_create', {'name': 'changed'}, ctx)['action_id']
    assert first['action_id'] != receipt_arguments('agent_delegate', {}, ctx)['action_id']
    assert receipt_arguments('agent_create', first, SimpleNamespace(thread_id='different')) == first
    assert receipt_arguments('agent_create', {}, SimpleNamespace(thread_id='t')) != receipt_arguments('agent_create', {}, SimpleNamespace(thread_id='t'))


@pytest.mark.parametrize('name', ['agent_create', 'agent_delegate'])
async def test_timeout_returns_the_exact_submitted_key_without_replay(name):
    registry = ToolRegistry(SimpleNamespace(tool_timeout=.01, max_tool_bytes=65536), include_builtins=False)
    calls = []
    async def accepted(args, ctx):
        calls.append(args.copy())
        await asyncio.sleep(1)
    registry.register(name, 'fixture', {'type': 'object'}, accepted)
    registry.tools[name].mcp_server = 'cutex'
    args = {}
    result = await registry.execute(name, args, SimpleNamespace(thread_id='t', turn_id='u', call_id='c'))
    receipt = json.loads(result.text)
    assert not result.success and receipt['outcomeUnknown'] is True
    assert receipt['action_id'] == calls[0]['action_id']
    assert len(calls) == 1 and args == {}


@pytest.mark.parametrize('payload,returncode', [(b'not-json', 0), (b'{"id":2}', 0), (b'', 1),
    (b'{"id":1,"error":{"code":-32603,"message":"secret"}}', 0)])
async def test_facade_loss_keeps_key_and_never_relaunches(tmp_path, monkeypatch, payload, returncode):
    executable=tmp_path/'facade'; executable.touch()
    descriptor=tmp_path/'tools.json'
    descriptor.write_text(json.dumps({'executable':str(executable),'tools':[
        {'name':'agent_create','description':'fixture','inputSchema':{'type':'object'}}]}))
    (tmp_path/'cutex-mcp-binding.json').write_text(json.dumps({'threadId':'t','runtimeAgentId':'runtime',
        'generation':1,'url':'http://fixture','token':'fixture-secret'}))
    writes=[]
    class Input:
        def write(self,data): writes.append(json.loads(data))
        async def drain(self): pass
        def close(self): pass
    class Output:
        sent=False
        async def read(self,size):
            if self.sent:return b''
            self.sent=True
            return payload
    class Process:
        def __init__(self):self.stdin=Input();self.stdout=Output();self.returncode=returncode
        async def wait(self):return self.returncode
    async def spawn(*args,**kwargs):return Process()
    monkeypatch.setattr('cute_claude.cutex_tools.asyncio.create_subprocess_exec',spawn)
    registry=ToolRegistry(SimpleNamespace(tool_timeout=30,max_tool_bytes=65536),include_builtins=False)
    load(registry,descriptor)
    result=await registry.execute('agent_create',{'name':'one'},SimpleNamespace(thread_id='t',turn_id='u',call_id='c'))
    receipt=json.loads(result.text)
    assert receipt['action_id']==writes[0]['params']['arguments']['action_id']
    assert receipt['outcomeUnknown'] and not result.success and len(writes)==1
    assert 'secret' not in result.text
