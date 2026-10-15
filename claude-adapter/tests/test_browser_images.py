"""Pure/fake-SDK image plumbing evidence, not live Claude/model acceptance."""
import base64
import json
from types import SimpleNamespace

import pytest

from cute_claude.cutex_tools import load
from cute_claude.tools import ToolRegistry, ToolResult
from cute_claude.subscription import SdkToolBridge
from cute_claude.store import Store, dump
from cute_claude.browser_content import journal_result
from test_subscription import sh, emit_reply

# Fixed non-sensitive one-pixel PNG fixture.
PNG = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jvN8AAAAASUVORK5CYII='
BLOCKS = [{'type':'text','text':'Fixture screenshot'}, {'type':'image','mimeType':'image/png','data':PNG}]

def test_browser_image_conversions_and_budgets():
    result = ToolResult.from_mcp(BLOCKS)
    assert result.mcp_content() == BLOCKS
    assert result.model_content()[1] == {'type':'image','source':{'type':'base64','media_type':'image/png','data':PNG}}
    assert result.timeline_content()[1] == {'type':'inputImage','imageUrl':'data:image/png;base64,' + PNG}
    copied = result.mcp_content(); copied[1]['data'] = 'bad'
    assert result.mcp_content()[1]['data'] == PNG
    for bad in [[], [BLOCKS[1], BLOCKS[1]], [{'type':'image','mimeType':'image/svg+xml','data':PNG}],
                [{'type':'image','mimeType':'image/png','data':'!bad'}],
                [{'type':'text','text':'x' * 65537}], [{'type':'text','text':'x' * 65536}],
                [{'type':'text','text':'\n' * 40000}], [{'type':'resource','uri':'file:///secret'}],
                [{'type':'image','mimeType':'image/png','data':base64.b64encode(b'not png').decode()}]]:
        with pytest.raises(ValueError): ToolResult.from_mcp(bad)

async def test_browser_mcp_bridge_preserves_image_and_trusted_call_meta(tmp_path, monkeypatch):
    executable=tmp_path/'facade'; executable.touch()
    descriptor=tmp_path/'tools.json'
    descriptor.write_text(json.dumps({'executable':str(executable),'tools':[{'name':'browser_screenshot','description':'shot','inputSchema':{'type':'object'}}]}))
    (tmp_path/'cutex-mcp-binding.json').write_text(json.dumps({'threadId':'thread','runtimeAgentId':'runtime','generation':1,'url':'http://fixture','token':'fixture-token'}))
    requests=[]
    class Input:
        def write(self,data): requests.append(json.loads(data))
        async def drain(self): pass
        def close(self): pass
    class Output:
        sent=False
        async def read(self,size):
            if self.sent: return b''
            self.sent=True
            return json.dumps({'jsonrpc':'2.0','id':1,'result':{'content':BLOCKS}}).encode()
    class Process:
        stdin=Input(); stdout=Output(); returncode=0
        async def wait(self): return 0
    async def spawn(*a,**kw): return Process()
    monkeypatch.setattr('cute_claude.cutex_tools.asyncio.create_subprocess_exec',spawn)
    registry=ToolRegistry(None,include_builtins=False); load(registry,descriptor)
    result=await registry.tools['browser_screenshot'].handler({'title':'Shot','guestId':'g'},SimpleNamespace(thread_id='thread',call_id='native-call'))
    assert result.mcp_content()==BLOCKS
    assert requests[0]['params']['_meta']=={'threadId':'thread','callId':'native-call'}
    assert requests[0]['params']['arguments']=={'title':'Shot','guestId':'g'}

async def test_subscription_image_sdk_cache_and_full_history(sh):
    calls=[]
    async def screenshot(args,ctx):
        calls.append(ctx.call_id)
        return ToolResult.from_mcp(BLOCKS)
    sh.tools.register('browser_screenshot','shot',{'type':'object'},screenshot,read_only=True)
    sh.tools.tools['browser_screenshot'].mcp_server='cutex'
    async def behavior(**kw):
        bridge=SdkToolBridge(kw['engine'],kw['tid'],kw['turn_id'],kw['session_id'])
        data={'session_id':kw['session_id'],'tool_name':'mcp__cutex__browser_screenshot','tool_input':{'arguments':{}}}
        results=[]
        for _ in range(2):
            permit=await bridge.pre_tool(data,'native-image-call',{})
            results.append(await bridge.execute('browser_screenshot',permit['hookSpecificOutput']['updatedInput']))
        assert results[0]==results[1]=={'content':BLOCKS,'is_error':False}
        cached = json.loads(sh.store.db.execute('SELECT result FROM tool_calls').fetchone()[0])
        assert cached['browserImagesOmitted'] and cached['browserReplayItemId']
        assert PNG not in dump(cached)
        bridge.revoke()
        assert 'browserReplayItemId' not in sh.store.db.execute('SELECT result FROM tool_calls').fetchone()[0]
        await emit_reply(kw['on_message'],kw['session_id'],'[FAKE SDK] image content received')
    sh.provider.behavior=behavior
    tid=await sh.thread(); turn=await sh.turn(tid,'shot'); await sh.drain()
    assert sh.store.turn(turn)['status']=='completed'
    assert calls==['native-image-call']
    item=next(i for i in sh.store.items(turn) if i['type']=='mcpToolCall')
    assert item['result']['content']==BLOCKS
    cached=json.loads(sh.store.db.execute('SELECT result FROM tool_calls').fetchone()[0])
    assert cached['browserImagesOmitted']
    assert cached['mcpContent']==[BLOCKS[0]]
    assert 'browserReplayItemId' not in cached and PNG not in dump(cached)


@pytest.mark.parametrize('mutation', ['missing', 'wrong-tool', 'wrong-server', 'invalid-image', 'oversized'])
async def test_subscription_image_missing_or_invalid_replay_is_not_reexecuted(sh, mutation):
    calls = []
    async def screenshot(args, ctx):
        calls.append(ctx.call_id)
        return ToolResult.from_mcp(BLOCKS)
    sh.tools.register('browser_screenshot', 'shot', {'type': 'object'}, screenshot, read_only=True)
    sh.tools.tools['browser_screenshot'].mcp_server = 'cutex'
    async def behavior(**kw):
        bridge = SdkToolBridge(kw['engine'], kw['tid'], kw['turn_id'], kw['session_id'])
        data = {'session_id': kw['session_id'], 'tool_name': 'mcp__cutex__browser_screenshot', 'tool_input': {'arguments': {}}}
        permit = await bridge.pre_tool(data, 'image-call', {})
        assert (await bridge.execute('browser_screenshot', permit['hookSpecificOutput']['updatedInput']))['content'] == BLOCKS
        saved = json.loads(sh.store.db.execute('SELECT result FROM tool_calls').fetchone()[0])
        item = next(i for i in sh.store.items(kw['turn_id']) if i['id'] == saved['browserReplayItemId'])
        if mutation == 'missing':
            sh.store.db.execute('DELETE FROM items WHERE id=?', (item['id'],))
        else:
            if mutation == 'wrong-tool': item['tool'] = 'browser_find'
            if mutation == 'wrong-server': item['server'] = 'other'
            if mutation == 'invalid-image': item['result']['content'][1]['data'] = '!bad'
            if mutation == 'oversized': item['padding'] = 'x' * (8 * 1024 * 1024)
            sh.store.put_item(kw['turn_id'], item)
        permit = await bridge.pre_tool(data, 'image-call', {})
        result = await bridge.execute('browser_screenshot', permit['hookSpecificOutput']['updatedInput'])
        assert result['is_error'] and 'not re-executed' in result['content'][0]['text']
        bridge.revoke()
        await emit_reply(kw['on_message'], kw['session_id'], '[FAKE SDK] replay rejected')
    sh.provider.behavior = behavior
    tid = await sh.thread(); await sh.turn(tid, 'shot'); await sh.drain()
    assert calls == ['image-call']


@pytest.mark.parametrize('terminal', ['completed', 'failed', 'interrupted', 'recover'])
def test_browser_image_journal_cleanup_preserves_canonical_history(tmp_path, terminal):
    store = Store(tmp_path / 'state')
    try:
        tid = store.new_thread('fixture', str(tmp_path))
        receipt = store.enqueue(tid, 'input', {'source': {'kind': 'user', 'id': 'owner'}, 'text': 'fixture'}, 1)
        turn = receipt['turnId']; assert store.claim(tid)
        call = {'id': 'image-call', 'name': 'browser_screenshot', 'input': {}}
        slot = store.append_assistant(tid, turn, [{'type': 'tool_use', **call}])
        store.tool_started(tid, turn, call, slot)
        result = ToolResult.from_mcp(BLOCKS)
        store.tool_finished(tid, call['id'], slot, result.model_content(), True)
        journal = json.loads(store.db.execute('SELECT result FROM tool_calls').fetchone()[0])
        assert journal['browserImagesOmitted'] and PNG not in dump(journal)
        assert PNG in dump(store.model_history(tid, 1_000_000))
        item = {'type': 'mcpToolCall', 'id': 'image-item', 'server': 'cutex', 'tool': call['name'],
                'status': 'completed', 'result': {'content': BLOCKS}}
        store.put_item(turn, item)
        journal['browserReplayItemId'] = item['id']
        store.db.execute('UPDATE tool_calls SET result=?', (dump(journal),))
        if terminal == 'recover': store.recover()
        else: store.finish(tid, turn, terminal)
        saved = store.db.execute('SELECT result FROM tool_calls').fetchone()[0]
        assert 'browserReplayItemId' not in saved and PNG not in saved
        assert store.items(turn)[-1] == item
        assert PNG in dump(store.model_history(tid, 1_000_000))
    finally:
        store.close()


def test_nonbrowser_replay_journal_and_text_error_shapes_are_unchanged():
    result = {'content': 'fixture', 'mcpContent': BLOCKS, 'is_error': True}
    assert journal_result('other_tool', result) is result
    assert journal_result('browser_unknown', result) is result
    text = {'content': 'permission_denied', 'mcpContent': [BLOCKS[0]], 'is_error': True}
    assert journal_result('browser_screenshot', text) is text
    scrubbed = journal_result('browser_screenshot', result)
    assert scrubbed['is_error'] is True and scrubbed['content'] == result['content']
    assert result['mcpContent'] == BLOCKS, 'canonical content must not be mutated'
