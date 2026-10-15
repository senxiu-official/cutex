"""Native question correlation, real protocol replies and persisted plan mode."""
import asyncio
from dataclasses import replace
from types import SimpleNamespace
import pytest
from cute_claude.approvals import Approvals, unattended_question
from cute_claude.config import Settings
from cute_claude.questions import questions_wire, read_answers
from cute_claude.store import identifier
from test_persistent import ph

QUESTIONS = {'questions': [{'question':'Which color?', 'header':'Color', 'multiSelect':False,
    'options':[{'label':'Pink','description':'Theme pink'},{'label':'Blue','description':'Theme blue'}]}]}

async def begin(ph, source=None, metadata=None):
    tid=await ph.thread()
    await ph.call('claude/runtime/start', {'threadId':tid})
    ph.peer.interactive=True
    payload={'text':'fixture','eventType':'message','source':source or {'kind':'user','id':'test'}}
    if metadata:payload['metadata']=metadata
    ph.store.enqueue(tid, identifier(), payload, 10)
    turn=ph.store.claim(tid)['turn_id']
    bridge=ph.engine.driver.owners[tid].bridge
    bridge.begin_turn(turn)
    return tid,turn,bridge

AGENT='cutex.00000000-0000-0000-0000-000000000001'

async def test_agent_turn_question_is_answered_at_once_without_a_human(ph):
    tid,turn,bridge=await begin(ph,{'kind':'agent','id':AGENT},{'sourceName':'director'})
    assert ph.store.turn_source(turn)=={'kind':'agent','id':AGENT,'name':'director'}
    call=identifier()
    assert await bridge.pre_tool({'session_id':bridge.sid,'tool_name':'AskUserQuestion','tool_input':QUESTIONS},call,{})=={}
    invoke=ph.sdk.clients[0].options.can_use_tool
    result=await asyncio.wait_for(invoke('AskUserQuestion',QUESTIONS,SimpleNamespace(tool_use_id=call)),2)
    assert result.behavior=='deny'
    assert result.message.startswith('No human is available to answer in this turn')
    assert f'agent director ({AGENT}) via the Agent Bus' in result.message
    assert 'ask the requesting agent' in result.message
    assert not getattr(result,'interrupt',False)
    # Nobody was asked and nothing waits.
    assert not any(x.get('method')=='item/tool/requestUserInput' for x in ph.out)
    assert not ph.server.approvals.pending and not bridge.host_tasks
    item=next(i for i in ph.store.items(turn) if i.get('tool')=='AskUserQuestion')
    assert item['status']=='failed' and item['success'] is False
    assert item['contentItems'][0]['text']==result.message
    completed=[x for x in ph.out if x.get('method')=='item/completed' and x['params']['item'].get('tool')=='AskUserQuestion']
    assert completed and completed[-1]['params']['item']['status']=='failed'
    # The correlation was consumed: a replay is refused.
    assert (await invoke('AskUserQuestion',QUESTIONS,SimpleNamespace(tool_use_id=call))).behavior=='deny'

async def test_agent_turn_question_waits_when_configured(ph):
    ph.server.settings=replace(ph.server.settings,agent_turn_questions='wait')
    tid,turn,bridge=await begin(ph,{'kind':'service','id':'job-runner'})
    call=identifier()
    await bridge.pre_tool({'session_id':bridge.sid,'tool_name':'AskUserQuestion','tool_input':QUESTIONS},call,{})
    invoke=ph.sdk.clients[0].options.can_use_tool
    task=asyncio.create_task(invoke('AskUserQuestion',QUESTIONS,SimpleNamespace(tool_use_id=call)))
    try:
        async with asyncio.timeout(2):
            while not ph.server.approvals.pending:await asyncio.sleep(.001)
        req=next(x for x in ph.out if x.get('method')=='item/tool/requestUserInput')
        await ph.server.handle(ph.peer,{'id':req['id'],'result':{'answers':{'q0':{'answers':['Blue']}}}})
        result=await asyncio.wait_for(task,2)
        assert result.behavior=='allow' and result.updated_input['answers']=={'Which color?':'Blue'}
    finally:
        if not task.done():task.cancel();await asyncio.gather(task,return_exceptions=True)

async def test_owner_turn_question_still_waits_by_default(ph):
    assert ph.server.settings.agent_turn_questions=='decide'
    tid,turn,bridge=await begin(ph)
    assert await ph.server.approvals.decline_unattended(tid,turn,QUESTIONS,identifier()) is None
    assert ph.store.items(turn)==[] or all(i.get('tool')!='AskUserQuestion' for i in ph.store.items(turn))

def test_unattended_question_text_and_modes():
    assert unattended_question(None,'decide') is None
    assert unattended_question({'kind':'user','id':'local-controller','name':None},'decide') is None
    assert unattended_question({'kind':None,'id':None,'name':None},'decide') is None
    assert unattended_question({'kind':'agent','id':AGENT,'name':'x'},'wait') is None
    assert 'agent cutex.' in unattended_question({'kind':'agent','id':AGENT,'name':None},'decide')
    assert 'input from service jobs (svc-1)' in unattended_question({'kind':'service','id':'svc-1','name':'jobs'},'decide')
    assert 'started by other input from z' in unattended_question({'kind':'other','id':'z','name':None},'decide')

def test_agent_turn_questions_setting_and_flag(tmp_path):
    from cute_claude.__main__ import parser
    assert parser().parse_args(['--workspace',str(tmp_path)]).agent_turn_questions=='decide'
    assert parser().parse_args(['--workspace',str(tmp_path),'--agent-turn-questions','wait']).agent_turn_questions=='wait'
    with pytest.raises(SystemExit):
        parser().parse_args(['--workspace',str(tmp_path),'--agent-turn-questions','ask'])
    with pytest.raises(ValueError):
        Settings(tmp_path,tmp_path/'state',agent_turn_questions='ask').validate()

@pytest.mark.parametrize('answer', ['selected','custom','cancel','disconnect','interrupt'])
async def test_native_question_returns_actual_answer_and_persists_projection(ph, answer):
    tid,turn,bridge=await begin(ph)
    call=identifier()
    assert await bridge.pre_tool({'session_id':bridge.sid,'tool_name':'AskUserQuestion','tool_input':QUESTIONS},call,{})=={}
    invoke=ph.sdk.clients[0].options.can_use_tool
    task=asyncio.create_task(invoke('AskUserQuestion',QUESTIONS,SimpleNamespace(tool_use_id=call)))
    try:
        async with asyncio.timeout(2):
            while not ph.server.approvals.pending:await asyncio.sleep(.001)
        req=next(x for x in ph.out if x.get('method')=='item/tool/requestUserInput')
        other=ph.server.connect(ph.send)
        await ph.server.handle(other,{'id':req['id'],'result':{'answers':{'q0':{'answers':['spoof']}}}})
        assert not task.done()
        if answer=='disconnect':ph.server.disconnect(ph.peer)
        elif answer=='interrupt':
            bridge.revoke();await bridge.cancel_tools()
        else:
            result={'answers':{}} if answer=='cancel' else {'answers':{'q0':{'answers':['Pink' if answer=='selected' else 'a custom color']}}}
            await ph.server.handle(ph.peer,{'id':req['id'],'result':result})
        if answer=='interrupt':
            with pytest.raises(asyncio.CancelledError):await task
        else:
            result=await asyncio.wait_for(task,2)
            assert result.behavior==('allow' if answer in {'selected','custom'} else 'deny')
            if result.behavior=='allow':
                assert result.updated_input['answers']=={'Which color?':'Pink' if answer=='selected' else 'a custom color'}
        items=ph.store.items(turn)
        question=next(i for i in items if i.get('tool')=='AskUserQuestion')
        assert question['success'] is (answer in {'selected','custom'})
        assert not ph.server.approvals.pending
        assert not bridge.host_tasks
        duplicate=await invoke('AskUserQuestion',QUESTIONS,SimpleNamespace(tool_use_id=call))
        assert duplicate.behavior=='deny'
    finally:
        if not task.done():task.cancel();await asyncio.gather(task,return_exceptions=True)

async def test_native_question_no_ui_does_not_invent_answer(ph):
    tid,turn,bridge=await begin(ph);ph.peer.interactive=False
    assert await ph.server.approvals.ask(tid,turn,QUESTIONS,identifier()) is None
    assert not ph.server.approvals.pending


def test_multiselect_preserves_choices_and_free_text():
    args={'questions':[{**QUESTIONS['questions'][0],'multiSelect':True}]}
    wire=questions_wire(args)
    assert wire[0]['options'] is None
    assert 'Pink: Theme pink' in wire[0]['question'] and 'Blue: Theme blue' in wire[0]['question']
    assert read_answers({'answers':{'q0':{'answers':['Pink, Blue']}}},args['questions'])=={'Which color?':'Pink, Blue'}
    assert read_answers({'answers':{'q0':{'answers':[]}}},args['questions']) is None

async def test_plan_sdk_mode_persistence_and_permissions_restore(ph):
    tid=await ph.thread()
    await ph.call('claude/runtime/start',{'threadId':tid})
    await ph.server.approvals.configure(tid,'auto')
    client=ph.sdk.clients[-1]
    before=ph.engine.runtime_status(tid)['sessionId']
    await ph.call('thread/settings/update',{'threadId':tid,'collaborationMode':{'mode':'plan','settings':{}}})
    assert client.options.permission_mode=='plan'
    assert ph.server.approvals.mode(tid)=='auto'
    assert Approvals(ph.server).planning(tid)
    await ph.call('thread/resume',{'threadId':tid})
    assert ph.out[-1]['params']['threadSettings']['collaborationMode']['mode']=='plan'
    await ph.server.approvals.configure(tid,'default')
    assert client.options.permission_mode=='plan'
    await ph.call('claude/runtime/close',{'threadId':tid})
    await ph.call('claude/runtime/start',{'threadId':tid})
    assert ph.sdk.clients[-1].options.permission_mode=='plan'
    assert ph.engine.runtime_status(tid)['sessionId']==before
    await ph.call('thread/settings/update',{'threadId':tid,'collaborationMode':{'mode':'default','settings':{}}})
    assert ph.sdk.clients[-1].options.permission_mode=='default'
    assert not ph.server.approvals.planning(tid)

async def test_plan_failure_does_not_persist_and_plan_hook_blocks_effects(ph):
    tid=await ph.thread();await ph.call('claude/runtime/start',{'threadId':tid})
    client=ph.sdk.clients[-1];original=client.set_permission_mode
    async def reject(mode):raise RuntimeError('SDK refused')
    client.set_permission_mode=reject
    result=await ph.call('thread/settings/update',{'threadId':tid,'collaborationMode':{'mode':'plan'}},error=True)
    assert result['code']==-32020 and not ph.server.approvals.planning(tid)
    client.set_permission_mode=original
    await ph.server.approvals.set_plan(tid,True)
    bridge=ph.engine.driver.owners[tid].bridge;bridge.begin_turn(identifier())
    for name,expected in [('read_file',None),('exec','deny')]:
        reply=await bridge.pre_tool({'session_id':bridge.sid,'tool_name':'mcp__cutex__'+name,'tool_input':{'arguments':{'path':'hello.txt'}}},identifier(),{})
        assert reply['hookSpecificOutput'].get('permissionDecision')==expected
    await bridge.end_turn()

from test_subscription import sh, emit_reply

async def test_plan_projection_is_saved_without_markup_and_no_duplicate_stream(sh):
    tid=await sh.thread()
    await sh.server.approvals.set_plan(tid,True)
    async def response(**kw):
        await emit_reply(kw['on_message'],kw['session_id'],'Before\n<proposed_plan>1. Read\n2. Edit</proposed_plan>\nAfter')
    sh.provider.behavior=response
    turn=await sh.turn(tid,'Plan a change');await sh.drain()
    items=[i for i in sh.store.items(turn) if i['type']!='userMessage']
    assert [(i['type'],i['text']) for i in items]==[('agentMessage','Before'),('plan','1. Read\n2. Edit'),('agentMessage','After')]
    assert sh.store.turn(turn)['status']=='completed'
    assert not any(x.get('method')=='item/agentMessage/delta' for x in sh.out)
    assert 'no plan-file writing' in sh.provider.requests[-1]['prompt']
    read=await sh.call('thread/read',{'threadId':tid,'includeTurns':True})
    assert next(i for i in read['thread']['turns'][0]['items'] if i['type']=='plan')['text']=='1. Read\n2. Edit'

async def test_start_plan_and_busy_mode_change(ph):
    result=await ph.call('thread/start',{'collaborationMode':{'mode':'plan'}})
    tid=result['thread']['id']
    assert ph.server.approvals.planning(tid)
    await ph.call('claude/runtime/start',{'threadId':tid})
    assert ph.sdk.clients[-1].options.permission_mode=='plan'
    ph.store.enqueue(tid,identifier(),{'text':'pending','eventType':'message','source':{'kind':'user','id':'test'}},10)
    result=await ph.call('thread/settings/update',{'threadId':tid,'collaborationMode':{'mode':'default'}},error=True)
    assert result['code']==-32009
    assert ph.server.approvals.planning(tid)
