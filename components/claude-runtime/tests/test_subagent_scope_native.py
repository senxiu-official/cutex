"""Real bundled CLI, fake loopback model, inert parent management/read handlers."""
import asyncio
import json
import os
from pathlib import Path
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import pytest
from cute_claude.config import Settings
from cute_claude.subscription import OfficialSdkDriver
from cute_claude.subscription_auth import ApiAccount, CONFLICTING_ENV
from cute_claude.tools import ToolResult
from test_subscription import SubHarness

pytestmark=pytest.mark.skipif(os.getenv('CUTEX_SUBAGENT_SCOPE_NATIVE')!='1',reason='opt-in real loopback CLI fixture')

@pytest.mark.parametrize('background,agent_type,nested',[
    (False,'general-purpose',False),(True,'general-purpose',False),
    (False,'Explore',False),(True,'Explore',False),
    (False,'Plan',False),(True,'Plan',False),
    (False,'custom-reader',False),(True,'custom-reader',False),
    (False,'general-purpose',True),
])
async def test_real_child_catalog_and_readonly_identity(tmp_path,monkeypatch,background,agent_type,nested,restart=False):
    for name in CONFLICTING_ENV:monkeypatch.delenv(name,raising=False)
    for key in ('HOME','USERPROFILE','APPDATA','LOCALAPPDATA','TEMP','TMP'):
        path=tmp_path/key.lower();path.mkdir();monkeypatch.setenv(key,str(path))
    work=tmp_path/'workspace';work.mkdir()
    requests=[];steps={'parent':0,'child':0,'grandchild':0};effects=[];reads=[];epoch=[0]
    if agent_type=='custom-reader':
        agents=work/'.claude'/'agents';agents.mkdir(parents=True)
        (agents/'custom-reader.md').write_text('---\nname: custom-reader\ndescription: Synthetic custom reader\n---\nCUSTOM_ROLE_SENTINEL Preserve this custom role while completing the task.\n')
    class Handler(BaseHTTPRequestHandler):
        def log_message(self,*args):pass
        def do_POST(self):
            data=json.loads(self.rfile.read(int(self.headers.get('Content-Length','0'))))
            if '/messages' not in self.path or 'count_tokens' in self.path:
                raw=json.dumps({'input_tokens':1}).encode();ctype='application/json'
            else:
                child='cc_is_subagent=true' in json.dumps(data.get('system',[]))
                first_user=next((m for m in data.get('messages',[]) if m.get('role')=='user'),{})
                kind=('grandchild' if 'NESTED_SCOPE_FIXTURE' in json.dumps(first_user) else 'child') if child else 'parent'
                steps[kind]+=1;stage=steps[kind]
                requests.append({'kind':kind,'stage':stage,'data':data})
                if not child and stage==1:name,args='ToolSearch',{'query':'+cutex agent_create'}
                elif not child and stage==2:name,args='mcp__cutex__agent_create',{'arguments':{}}
                elif not child and stage==3:name,args='Agent',{'description':'Native scope fixture','prompt':'SUBTASK_SCOPE_FIXTURE Complete only this synthetic subtask.', 'subagent_type':agent_type,'run_in_background':background}
                elif child and kind=='child' and nested and stage==1:name,args='Agent',{'description':'Nested native scope fixture','prompt':'NESTED_SCOPE_FIXTURE Complete only this synthetic subtask.', 'subagent_type':'general-purpose','run_in_background':False}
                elif child and stage==1:name,args='mcp__cutex__agent_create',{'arguments':{}}
                elif child and stage==2:name,args='ToolSearch',{'query':'+cutex agent_list'}
                elif child and stage==3:name,args='mcp__cutex__agent_list',{'arguments':{}}
                else:name,args=None,None
                block=({'type':'tool_use','id':f'toolu_{epoch[0]}_{kind}_{stage}','name':name,'input':args}
                       if name else {'type':'text','text':f'{kind.upper()}_SCOPE_OK'})
                stop='tool_use' if name else 'end_turn'
                msg={'id':f'msg_{epoch[0]}_{kind}_{stage}','type':'message','role':'assistant','model':'claude-sonnet-4-6','content':[block],
                    'stop_reason':stop,'stop_sequence':None,'usage':{'input_tokens':20,'output_tokens':10}}
                if data.get('stream'):
                    empty=({'type':'tool_use','id':block['id'],'name':name,'input':{}} if name else {'type':'text','text':''})
                    delta=({'type':'input_json_delta','partial_json':json.dumps(args)} if name else {'type':'text_delta','text':block['text']})
                    events=[('message_start',{'type':'message_start','message':{**msg,'content':[],'stop_reason':None}}),
                        ('content_block_start',{'type':'content_block_start','index':0,'content_block':empty}),
                        ('content_block_delta',{'type':'content_block_delta','index':0,'delta':delta}),
                        ('content_block_stop',{'type':'content_block_stop','index':0}),
                        ('message_delta',{'type':'message_delta','delta':{'stop_reason':stop,'stop_sequence':None},'usage':{'output_tokens':10}}),
                        ('message_stop',{'type':'message_stop'})]
                    raw=''.join(f'event: {n}\ndata: {json.dumps(v)}\n\n' for n,v in events).encode();ctype='text/event-stream'
                else:raw=json.dumps(msg).encode();ctype='application/json'
            self.send_response(200);self.send_header('Content-Type',ctype);self.send_header('Content-Length',str(len(raw)));self.end_headers();self.wfile.write(raw)
    server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
    threading.Thread(target=server.serve_forever,daemon=True).start()
    key=tmp_path/'fixture-key.json';key.write_text(json.dumps({'apiKey':'local-fixture-not-real'}))
    account=ApiAccount('fixture','ANTHROPIC_API_KEY',f'http://127.0.0.1:{server.server_address[1]}',
        {'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC':'1','DISABLE_TELEMETRY':'1','DISABLE_ERROR_REPORTING':'1'},'local-fixture-not-real',key)
    cli=Path(os.environ['CUTEX_TEST_CLAUDE_CLI'])
    driver=OfficialSdkDriver(cli,tmp_path/'native-config',api_account=account)
    h=SubHarness(Settings(work,tmp_path/'adapter',model='claude-sonnet-4-6',backend='subscription',mock=False,thinking='disabled',
                         setting_sources=('project',) if agent_type=='custom-reader' else ()),driver)
    async def managed(args,context):
        effects.append(context.native_subagent_id)
        return ToolResult('inert parent fixture: no agent created')
    async def listed(args,context):
        reads.append(context.native_subagent_id)
        return ToolResult('[]')
    h.tools.register('agent_create','Inert parent management fixture',{'type':'object','properties':{},'additionalProperties':False},managed)
    h.tools.register('agent_list','Read-only child fixture',{'type':'object','properties':{},'additionalProperties':False},listed,read_only=True)
    try:
        await h.init();tid=await h.thread()
        h.store.db.execute('UPDATE threads SET instructions=? WHERE id=?',('PARENT_IDENTITY_ONLY You are a persistent Cutex Director.',tid))
        await h.turn(tid,'PARENT_SCOPE_FIXTURE Run the synthetic child scope case.')
        async with asyncio.timeout(65):
            while h.engine.workers or steps['child']<4:
                await asyncio.sleep(.02)
        parent=next(x['data'] for x in requests if x['kind']=='parent')
        child=next(x['data'] for x in requests if x['kind']=='child')
        assert 'PARENT_IDENTITY_ONLY' in json.dumps(parent['system'])
        assert 'PARENT_IDENTITY_ONLY' not in json.dumps(child['system'])
        assert 'no independent Cutex session' in json.dumps(child['system'])+json.dumps(child['messages'])
        catalog_hidden='mcp__cutex__agent_create' not in json.dumps(child['tools'])+json.dumps(child['messages'])
        if agent_type=='general-purpose':assert catalog_hidden
        if agent_type=='custom-reader':assert 'CUSTOM_ROLE_SENTINEL' in json.dumps(child['system'])
        assert 'mcp__cutex__agent_create' in json.dumps(parent['tools'])+json.dumps(parent['messages'])
        assert effects==[None], 'child must never invoke even the inert management handler'
        assert len(reads)==(2 if nested else 1) and all(isinstance(x,str) and x for x in reads)
        if nested:assert len(set(reads))==2 and steps['grandchild']>=4
        evidence={'background':background,'agentType':agent_type,'nested':nested,'catalogHidden':catalog_hidden,'parentManagementEffects':effects,'childReadonlyIds':reads,
                  'requestStages':[(x['kind'],x['stage']) for x in requests],'noAgentCreated':True}
        if restart:
            old_child=reads[0];settings=h.settings
            await h.close()
            h=SubHarness(settings,OfficialSdkDriver(cli,tmp_path/'native-config',api_account=account))
            h.tools.register('agent_create','Inert parent management fixture',{'type':'object','properties':{},'additionalProperties':False},managed)
            h.tools.register('agent_list','Read-only child fixture',{'type':'object','properties':{},'additionalProperties':False},listed,read_only=True)
            epoch[0]+=1;steps.update(parent=0,child=0,grandchild=0)
            await h.init();await h.call('thread/resume',{'threadId':tid})
            assert not (await h.call('claude/subagent/verify',{'threadId':tid,'childId':old_child}))['verified']
            await h.turn(tid,'PARENT_SCOPE_FIXTURE Complete another native child after cold restart.')
            async with asyncio.timeout(65):
                while h.engine.workers or steps['child']<4:await asyncio.sleep(.02)
            assert effects==[None,None] and len(reads)==2 and reads[1]!=old_child
            evidence['restartResume']={'oldChildRejected':True,'newChild':reads[1],'parentManagementRetained':True}
        (tmp_path/'scope-proof.json').write_text(json.dumps(evidence,indent=2))
    finally:
        (tmp_path/'requests.json').write_text(json.dumps(requests,indent=2))
        await h.close();server.shutdown();server.server_close()

async def test_real_child_cold_restart_resume(tmp_path,monkeypatch):
    await test_real_child_catalog_and_readonly_identity(tmp_path,monkeypatch,False,'general-purpose',False,restart=True)
