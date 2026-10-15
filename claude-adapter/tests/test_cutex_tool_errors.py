import json
from types import SimpleNamespace

import pytest
from cute_claude.cutex_tools import load
from cute_claude.tools import ToolRegistry

@pytest.mark.parametrize('payload,expected', [
    ({'jsonrpc':'2.0','id':1,'error':{'code':-32602,'message':'Invalid target'}}, 'Cutex MCP error -32602: Invalid target'),
    ({'jsonrpc':'2.0','id':1,'result':{'isError':True,'content':[{'type':'text','text':'{"status":"no_write","code":"invalid_durable_target"}'}]}}, '"status":"no_write"'),
])
async def test_bridge_preserves_error_details(tmp_path, monkeypatch, payload, expected):
    executable=tmp_path/'facade'; executable.touch()
    descriptor=tmp_path/'tools.json'
    descriptor.write_text(json.dumps({'executable':str(executable),'tools':[{'name':'send','description':'send','inputSchema':{'type':'object'}}]}))
    (tmp_path/'cutex-mcp-binding.json').write_text(json.dumps({'threadId':'thread','runtimeAgentId':'runtime','generation':1,'url':'http://fixture','token':'fixture-token'}))
    class Stream:
        def write(self, data): pass
        async def drain(self): pass
        def close(self): pass
    class Output:
        sent=False
        async def read(self, size):
            if self.sent:return b''
            self.sent=True
            return json.dumps(payload).encode()
    class Process:
        stdin=Stream();stdout=Output();returncode=0
        async def wait(self):return 0
    async def spawn(*a,**kw):return Process()
    monkeypatch.setattr('cute_claude.cutex_tools.asyncio.create_subprocess_exec',spawn)
    registry=ToolRegistry(None,include_builtins=False);load(registry,descriptor)
    result=await registry.tools['send'].handler({},SimpleNamespace(thread_id='thread'))
    assert not result.success and expected in result.text
    assert 'fixture-token' not in result.text


@pytest.mark.parametrize('name', ['send', 'member_runtime', 'human_ask', 'browser_unknown'])
async def test_nonbrowser_bridge_keeps_legacy_envelope_and_launcher_binding(tmp_path, monkeypatch, name):
    executable = tmp_path / 'facade'; executable.touch()
    descriptor = tmp_path / 'tools.json'
    descriptor.write_text(json.dumps({'executable': str(executable), 'tools': [
        {'name': name, 'description': 'fixture', 'inputSchema': {'type': 'object'}}]}))
    (tmp_path / 'cutex-mcp-binding.json').write_text(json.dumps({
        'threadId': 'thread', 'runtimeAgentId': 'runtime', 'generation': 1,
        'url': 'http://fixture', 'token': 'fixture-token'}))
    requests, environments = [], []
    class Input:
        def write(self, data): requests.append(json.loads(data))
        async def drain(self): pass
        def close(self): pass
    class Output:
        sent = False
        async def read(self, size):
            if self.sent: return b''
            self.sent = True
            return json.dumps({'id': 1, 'result': {'content': [{'type': 'text', 'text': 'ok'}]}}).encode()
    class Process:
        def __init__(self): self.stdin, self.stdout, self.returncode = Input(), Output(), 0
        async def wait(self): return 0
    async def spawn(*args, **kwargs):
        environments.append(kwargs['env'])
        return Process()
    monkeypatch.setattr('cute_claude.cutex_tools.asyncio.create_subprocess_exec', spawn)
    registry = ToolRegistry(None, include_builtins=False); load(registry, descriptor)
    arguments = {'fixture': 'unchanged'}
    for _ in range(2):
        result = await registry.tools[name].handler(arguments, SimpleNamespace(thread_id='thread', call_id='native-call'))
        assert result.success and result.text == 'ok'
    assert requests == [{'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call',
                         'params': {'name': name, 'arguments': arguments}}] * 2
    assert all(env['CUTEX_MCP_THREAD_ID'] == 'thread' for env in environments)
    with pytest.raises(ValueError, match='thread binding mismatch'):
        await registry.tools[name].handler(arguments, SimpleNamespace(thread_id='other', call_id='native-call'))
    assert len(requests) == 2, 'mismatched context never launches a facade'
