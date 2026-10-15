import asyncio
import sys
from dataclasses import replace
import pytest
from cute_claude.config import Settings
from cute_claude.store import identifier
from cute_claude.subscription import SdkToolBridge
from test_subscription import SubHarness, sh
from fake_sdk import install_fake_sdk

async def wait_for_request(h):
    async with asyncio.timeout(2):
        while not any(x.get('method') in {'mcpServer/elicitation/request','item/commandExecution/requestApproval'} for x in h.out):
            await asyncio.sleep(.001)
    return next(x for x in h.out if x.get('method') in {'mcpServer/elicitation/request','item/commandExecution/requestApproval'})

@pytest.mark.parametrize('tool', ['write_file', 'exec'])
@pytest.mark.parametrize('answer,exists', [(True,True),(False,False),('disconnect',False),('cancel',False)])
async def test_actual_write_waits_for_correct_peer_and_decision(tmp_path, monkeypatch, answer, exists, tool):
    work=tmp_path/'work'; work.mkdir()
    driver,sdk,_=install_fake_sdk(monkeypatch,tmp_path)
    h=SubHarness(Settings(work,tmp_path/'state',mock=False,backend='subscription',model='sonnet',allow_write=True,allow_exec=True),driver)
    task=None
    try:
        await h.init(); h.peer.interactive=True
        tid=await h.thread()
        await h.call('claude/approvals/set',{'threadId':tid,'mode':'default'})
        await h.call('claude/runtime/start',{'threadId':tid})
        receipt=h.store.enqueue(tid,identifier(),{'text':'test','eventType':'message','source':{'kind':'user','id':'fixture'}},10)
        row=h.store.claim(tid); turn=row['turn_id']
        bridge=driver.owners[tid].bridge
        bridge.begin_turn(turn)
        args={'path':'approved.txt','content':'reviewed'} if tool=='write_file' else {'argv':[sys.executable,'-c',"from pathlib import Path; Path('approved.txt').write_text('reviewed')"]}
        call={'id':identifier(),'name':tool,'input':args}
        grant=await bridge.pre_tool({'session_id':bridge.sid,'tool_name':'mcp__cutex__'+tool,'tool_input':{'arguments':args}},call['id'],{})
        assert 'permissionDecision' not in grant['hookSpecificOutput']
        native_args=grant['hookSpecificOutput']['updatedInput']
        async def invoke():
            permit=await sdk.clients[0].options.can_use_tool('mcp__cutex__'+tool,native_args,{})
            if permit.behavior!='allow':return {'is_error':True}
            return await bridge.execute(tool,permit.updated_input)
        task=asyncio.create_task(invoke())
        request=await wait_for_request(h)
        assert not (work/'approved.txt').exists()
        other=h.server.connect(h.send)
        await h.server.handle(other,{'id':request['id'],'result':({'decision':'accept'} if tool=='exec' else {'action':'accept','content':{'approved':True}})})
        await asyncio.sleep(.01)
        assert not task.done() and not (work/'approved.txt').exists()
        if answer=='disconnect':h.server.disconnect(h.peer)
        elif answer=='cancel':
            await h.server.handle(h.peer,{'id':request['id'],'result':({'decision':'cancel'} if tool=='exec' else {'action':'cancel','content':{'approved':True}})})
        else:
            await h.server.handle(h.peer,{'id':request['id'],'result':({'decision':'accept' if answer else 'decline'} if tool=='exec' else {'action':'accept','content':{'approved':answer}})})
        result=await asyncio.wait_for(task,2)
        assert (work/'approved.txt').exists() is exists
        assert result['is_error'] is not exists
        assert not h.server.approvals.pending
        assert any(x.get('method')=='serverRequest/resolved' for x in h.out) or answer=='disconnect'
        # Exactly the same completed invocation does not prompt or execute twice.
        if exists:
            before=len(h.out)
            assert await bridge._execute_bound(turn,tool,call)==result
            assert len(h.out)==before
    finally:
        if task and not task.done():task.cancel();await asyncio.gather(task,return_exceptions=True)
        await h.close()

async def test_approval_no_tui_and_cancellation(h):
    tid=await h.thread(); a=h.server.approvals; await a.configure(tid,'default')
    assert not await a.approve(tid,'turn','exec',{'argv':['echo','x']})
    h.peer.interactive=True
    task=asyncio.create_task(a.approve(tid,'turn','exec',{'argv':['echo','x']}))
    await wait_for_request(h);task.cancel()
    with pytest.raises(asyncio.CancelledError):await task
    assert not a.pending
    assert any(x.get('method')=='serverRequest/resolved' for x in h.out)


async def test_modes_persist_and_legacy_models_are_explicit(sh):
    tid=await sh.thread()
    await sh.call('claude/approvals/set',{'threadId':tid,'mode':'default'})
    from cute_claude.approvals import Approvals
    assert Approvals(sh.server).mode(tid)=='default'
    models=(await sh.call('model/list'))['data']
    for version in ('4-8','4-7','4-6'):
        row=next(m for m in models if m['id']=='claude-opus-'+version)
        assert 'availability' in row['description']

from test_persistent import ph

async def test_permission_settings_roundtrip_and_sdk_failure_keeps_previous_mode(ph):
    tid=await ph.thread()
    await ph.call('claude/runtime/start',{'threadId':tid})
    await ph.call('thread/settings/update',{'threadId':tid,'effort':'high'})
    client=ph.sdk.clients[-1]
    for policy,reviewer,mode in [('on-request','user','default'),('on-request','auto_review','auto'),('never','user','bypassPermissions')]:
        await ph.call('thread/settings/update',{'threadId':tid,'approvalPolicy':policy,'approvalsReviewer':reviewer,'effort':None})
        assert ph.engine.thread_effort(tid)=='high'
        assert client.options.permission_mode==mode
        assert ph.server.approvals.mode(tid)==mode
        resumed=await ph.call('thread/resume',{'threadId':tid})
        assert (resumed['approvalPolicy'],resumed['approvalsReviewer'])==(policy,reviewer)
    async def reject(mode):raise RuntimeError('fixture SDK rejects mode')
    client.set_permission_mode=reject
    result=await ph.call('thread/settings/update',{'threadId':tid,'approvalPolicy':'on-request','approvalsReviewer':'auto_review'},error=True)
    assert result['code'] == -32020
    assert ph.server.approvals.mode(tid)=='bypassPermissions'
    assert client.options.permission_mode=='bypassPermissions'
