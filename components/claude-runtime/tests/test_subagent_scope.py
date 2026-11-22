"""Child capability checks happen before grants and never affect parent tools."""
from types import SimpleNamespace
import pytest
from cute_claude.subagent_policy import READ_ONLY_TOOLS, DENIED
from cute_claude.subscription import CALL_FIELD
from cute_claude.tools import ToolResult
from test_persistent import ph
from test_questions_plan import begin

MANAGEMENT = ('agent_create','agent_archive','agent_restore','member_runtime','member_replace',
              'agent_delegate','agent_tell','owner_notify','alarm_set','workflow_start',
              'artifact_publish','service_register','service_update','service_unregister','service_restart')

def call(bridge, child, name):
    return {'session_id':bridge.sid, 'agent_id':child, 'tool_name':f'mcp__cutex__{name}',
            'tool_input':{'arguments':{}}}

async def test_child_management_denied_before_grant_and_parent_retains_tools(ph):
    tid, turn, bridge = await begin(ph)
    effects=[]
    async def fake(args, ctx):
        effects.append(ctx.native_subagent_id)
        return ToolResult('inert fixture')
    for name in MANAGEMENT:
        ph.tools.register(name, 'inert management fixture', {'type':'object'}, fake)
    context = await bridge.subagent_start({'session_id':bridge.sid,'agent_id':'child'},None,{})
    text=context['hookSpecificOutput']['additionalContext']
    assert 'no independent Cutex session' in text and 'return the result' in text
    for index,name in enumerate(MANAGEMENT):
        denied = await bridge.pre_tool(call(bridge,'child',name), f'child-{index}', {})
        assert denied['hookSpecificOutput']['permissionDecisionReason'] == DENIED
    assert bridge.grants == {} and effects == []
    parent = await bridge.pre_tool(call(bridge,None,'agent_create'),'parent-create',{})
    assert parent['hookSpecificOutput']['updatedInput'][CALL_FIELD]
    result = await bridge.execute('agent_create',parent['hookSpecificOutput']['updatedInput'])
    assert not result['is_error'] and effects == [None]

async def test_explicit_readonly_set_preserves_trusted_child_id(ph):
    tid, turn, bridge = await begin(ph)
    seen=[]
    async def read(args, ctx):
        seen.append((ctx.thread_id,ctx.turn_id,ctx.native_subagent_id))
        return ToolResult('fixture read')
    for name in READ_ONLY_TOOLS:
        ph.tools.register(name, 'fixture read', {'type':'object'}, read, read_only=True)
    await bridge.subagent_start({'session_id':bridge.sid,'agent_id':'child'},None,{})
    for index,name in enumerate(sorted(READ_ONLY_TOOLS)):
        permit = await bridge.pre_tool(call(bridge,'child',name), f'read-{index}', {})
        result = await bridge.execute(name,permit['hookSpecificOutput']['updatedInput'])
        assert not result['is_error']
    assert seen == [(tid,turn,'child')]*len(READ_ONLY_TOOLS)
    # Merely labeling a tool read-only cannot expand the fixed scope.
    ph.tools.register('owner_pin','not an allowed read',{'type':'object'},read,read_only=True)
    assert (await bridge.pre_tool(call(bridge,'child','owner_pin'),'write-pin',{}))['hookSpecificOutput']['permissionDecisionReason'] == DENIED

async def test_background_read_after_parent_turn_ends_and_stale_child_denied(ph):
    tid, turn, bridge = await begin(ph)
    seen=[]
    async def read(args, ctx):
        seen.append(ctx.native_subagent_id)
        return ToolResult('background read')
    ph.tools.register('agent_list','fixture list',{'type':'object'},read,read_only=True)
    bridge.background['background'] = bridge.binding
    permit = await bridge.pre_tool(call(bridge,'background','agent_list'),'background-read',{})
    args=permit['hookSpecificOutput']['updatedInput']
    assert bridge.child_grant_valid(args,'background','background-read')
    bridge.revoke(); bridge.binding=None
    result = await bridge.execute('agent_list',args)
    assert not result['is_error'] and seen==['background']
    assert not any(i.get('tool')=='agent_list' for i in ph.store.items(turn))
    denied=await bridge.pre_tool(call(bridge,'background','agent_create'),'bg-manage',{})
    assert denied['hookSpecificOutput']['permissionDecisionReason']==DENIED
    again=await bridge.pre_tool(call(bridge,'background','agent_list'),'stale-read',{})
    bridge.background.pop('background')
    assert (await bridge.execute('agent_list',again['hookSpecificOutput']['updatedInput']))['is_error']

async def test_proof_is_passive_and_fails_after_child_or_owner_ends(ph):
    tid, turn, bridge = await begin(ph)
    await bridge.subagent_start({'session_id':bridge.sid,'agent_id':'child'},None,{})
    before=ph.sdk.clients[0].query_count
    result=await ph.call('claude/subagent/verify',{'threadId':tid,'childId':'child'})
    assert result['verified'] and result['parentThreadId']==tid and result['nativeSessionId']==bridge.sid
    assert not (await ph.call('claude/subagent/verify',{'threadId':tid,'childId':'unknown'}))['verified']
    await bridge.subagent_stop({'session_id':bridge.sid,'agent_id':'child'},None,{})
    assert not (await ph.call('claude/subagent/verify',{'threadId':tid,'childId':'child'}))['verified']
    assert ph.sdk.clients[0].query_count==before
