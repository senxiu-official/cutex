import asyncio
import json
from dataclasses import replace
from types import SimpleNamespace
import pytest
from test_persistent import ph
from test_questions_plan import begin
from cute_claude.native_tools import preview, exclusions

async def test_native_result_is_observation_not_mcp_execution(ph):
    tid,turn,b=await begin(ph)
    args={'query':'Claude SDK'}
    assert await b.pre_tool({'session_id':b.sid,'tool_name':'WebSearch','tool_input':args},'search',{})=={}
    assert not b.grants
    assert await b.pre_tool({'session_id':b.sid,'tool_name':'WebSearch','tool_input':args},'search',{})=={}
    assert len([i for i in ph.store.items(turn) if i.get("type")=='webSearch'])==1
    await b.native.post({'session_id':'wrong','tool_response':'bad'},'search',{})
    assert 'search' in b.native.calls
    await b.native.post({'session_id':b.sid,'hook_event_name':'PostToolUse','tool_response':{'results':['ok']}},'search',{})
    item=ph.store.items(turn)[-1]
    assert item['type']=='webSearch' and item['query']=='Claude SDK' and item['results']==['ok']
    assert item['action']=={'type':'search','query':'Claude SDK','queries':None}
    await b.native.post({'session_id':b.sid,'tool_response':'overwrite'},'search',{})
    assert ph.store.items(turn)[-1]==item
    assert ph.store.db.execute('SELECT count(*) FROM tool_calls').fetchone()[0]==0

async def test_native_permissions_correlate_and_deny_without_ui(ph):
    tid,turn,b=await begin(ph);ph.peer.interactive=False
    args={'url':'https://example.com','prompt':'title'}
    await b.pre_tool({'session_id':b.sid,'tool_name':'WebFetch','tool_input':args},'fetch',{})
    assert not (await b.native.approve('WebFetch',{'url':'wrong'},SimpleNamespace(tool_use_id='fetch')))['allow']
    assert not (await b.native.approve('WebFetch',args,SimpleNamespace(tool_use_id='fetch')))['allow']
    assert ph.store.items(turn)[-1]['success'] is False

async def test_native_callbacks_after_revoke_do_not_complete_another_turn(ph):
    tid,turn,b=await begin(ph)
    await b.pre_tool({'session_id':b.sid,'tool_name':'Read','tool_input':{'file_path':'hello.txt'}},'read',{})
    before=ph.store.items(turn)
    b.revoke()
    await b.native.post({'session_id':b.sid,'tool_response':'late'},'read',{})
    assert ph.store.items(turn)==before

async def test_disabled_native_tools_and_diagnostics(ph):
    tid,turn,b=await begin(ph)
    for name in ['Write','Bash','SendMessage','ListAgents','SendFile']:
        denied=await b.pre_tool({'session_id':b.sid,'tool_name':name,'tool_input':{}},name,{})
        assert denied['hookSpecificOutput']['permissionDecision']=='deny'
    assert 'WebSearch' not in exclusions(ph.settings)
    # Cross-session messaging stays off even with full write / exec.
    full=replace(ph.settings,allow_write=True,allow_exec=True)
    assert {'SendMessage','ListAgents','SendFile'} <= set(exclusions(full))
    assert 'Bash' not in exclusions(replace(ph.settings,allow_exec=True))
    result=await ph.call('claude/tools/list',{'threadId':tid})
    assert result['preset']=='claude_code' and result['observedTools'] is None


def test_preview_bounds_large_native_outputs_and_omits_binary():
    result=preview({'text':'x'*1000000,'image':{'data':'secret-base64'}})
    assert len(json.dumps(result))<17000
    assert 'secret-base64' not in json.dumps(result)


def test_subscription_registry_can_omit_duplicate_basic_tools(tmp_path):
    from cute_claude.config import Settings
    from cute_claude.tools import ToolRegistry
    settings=Settings(tmp_path,tmp_path/'state',backend='subscription',mock=False)
    assert not ToolRegistry(settings,include_builtins=False).tools
    assert 'read_file' in ToolRegistry(settings).tools

async def test_transcript_validation_failure_without_execution_hook(ph):
    tid,turn,b=await begin(ph)
    await b.native.message({'content':[{'type':'tool_use','id':'bad-call','name':'Bash','input':{'command':'invalid'}}]})
    await b.native.message({'content':[{'type':'tool_result','tool_use_id':'bad-call','is_error':True,'content':'Native validation rejected command'}]})
    item=ph.store.items(turn)[-1]
    assert item['type']=='commandExecution' and item['status']=='failed'
    assert item['command']=='invalid' and 'validation' in item['aggregatedOutput']
    await b.native.message({'content':[{'type':'tool_use','id':'bad-call','name':'Bash','input':{'command':'invalid'}}]})
    assert ph.store.items(turn)[-1]==item
    assert not b.native.calls

async def test_native_bash_uses_existing_command_approval_ui(ph):
    ph.engine.settings=replace(ph.settings,allow_exec=True)
    tid,turn,b=await begin(ph)
    args={'command':'printf approval-test'}
    await b.pre_tool({'session_id':b.sid,'tool_name':'Bash','tool_input':args},'bash-approve',{})
    task=asyncio.create_task(b.native.approve('Bash',args,SimpleNamespace(tool_use_id='bash-approve')))
    try:
        async with asyncio.timeout(2):
            while not ph.server.approvals.pending:await asyncio.sleep(.001)
        req=next(x for x in ph.out if x.get('method')=='item/commandExecution/requestApproval')
        assert req['params']['command']=='printf approval-test'
        assert req['params']['kind']=='command' and 'acceptForSession' in req['params']['availableDecisions']
        await ph.server.handle(ph.peer,{'id':req['id'],'result':{'decision':'accept'}})
        assert (await task)['allow']
    finally:
        if not task.done():task.cancel();await asyncio.gather(task,return_exceptions=True)
