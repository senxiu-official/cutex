"""Actual CLI child -> facade -> Bus -> Management -> adapter passive proof."""
import asyncio,json,os,subprocess,sys,threading
from pathlib import Path
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
import httpx,pytest
from cute_claude.config import Settings
from cute_claude.subscription import OfficialSdkDriver
from cute_claude.subscription_auth import ApiAccount
from cute_claude.cutex_tools import load
from cute_claude.transport import websocket,rpc_token
from test_subscription import SubHarness
from test_cutex_management import port,finish_process,sqlite_sessions

@pytest.mark.skipif(not os.getenv('CUTEX_INTEGRATION_BINARY') or not os.getenv('CUTEX_SUBAGENT_SCOPE_NATIVE'),reason='private real daemon/CLI opt-in')
async def test_real_child_read_roundtrips_passive_proof_without_deadlock(tmp_path,monkeypatch):
    binary=Path(os.environ['CUTEX_INTEGRATION_BINARY']).resolve(strict=True)
    facade=binary.with_name('cutex-mcp'+binary.suffix)
    cli_path=Path(os.environ['CUTEX_TEST_CLAUDE_CLI']).resolve(strict=True)
    (tmp_path/'.cutex-test-private-home').touch();config=tmp_path/'.cutex';config.mkdir()
    bp,mp=port(24710),port(24760)
    (config/'config.json').write_text(json.dumps({'agent_bus_enabled':True,'agent_bus_port':bp,'management_port':mp,
        'agent_bus_token':'private-fixture-bus-token','management_api_token':'private-fixture-management-token','new_session_notification':'important'}))
    env={k:os.environ[k] for k in ('PATH','PATHEXT','SystemRoot','SystemDrive','WINDIR','COMSPEC') if k in os.environ}
    env.update({k:str(tmp_path) for k in ('HOME','USERPROFILE','APPDATA','LOCALAPPDATA','TEMP','TMP')})
    env.update(CUTEX_TEST_PRIVATE_HOME=str(tmp_path),CUTEX_TEST_FORBID_REAL_HOME='1',
        CUTEX_MANAGEMENT_URL=f'http://127.0.0.1:{mp}',CUTEX_AGENT_BUS_URL=f'http://127.0.0.1:{bp}/',NO_PROXY='127.0.0.1,localhost')
    monkeypatch.setattr(os,'environ',env)
    flags={'creationflags':subprocess.CREATE_NO_WINDOW} if os.name=='nt' else {}
    async def command(*args):
        p=await asyncio.create_subprocess_exec(str(binary),*args,env=env,stdout=asyncio.subprocess.PIPE,stderr=asyncio.subprocess.PIPE,**flags)
        out,err=await finish_process(p)
        assert p.returncode==0,err.decode()
        return json.loads(out) if out.strip() else None
    work=tmp_path/'workspace';work.mkdir();account_home=tmp_path/'account';account_home.mkdir()
    deployment=tmp_path/'deployment.json';deployment.write_text(json.dumps({'python':sys.executable,
        'adapter_source':str(Path(__file__).resolve().parents[1]),'claude_cli':str(cli_path),
        'claude_home':str(account_home),'tui_binary':sys.executable}))
    await command('human','claude','install',str(deployment))
    record=await command('human','claude','new','native-child-scope-fixture','--cwd',str(work),'--model','claude-sonnet-4-6')
    identity=record['cutex_session_id'];state=Path(record['claude_session']['state_dir']);tid=record['claude_session']['thread_id']
    requests=[];steps={'parent':0,'child':0}
    class Handler(BaseHTTPRequestHandler):
        def log_message(self,*args):pass
        def do_POST(self):
            body=json.loads(self.rfile.read(int(self.headers.get('Content-Length','0'))))
            if '/messages' not in self.path or 'count_tokens' in self.path:
                raw=b'{"input_tokens":1}';ctype='application/json'
            else:
                child='cc_is_subagent=true' in json.dumps(body.get('system',[]));kind='child' if child else 'parent';steps[kind]+=1;n=steps[kind]
                requests.append({'kind':kind,'stage':n,'body':body})
                if not child and n==1:name,args='Agent',{'description':'Real Cutex child read','prompt':'Read the visible Cutex agent list for this subtask.', 'subagent_type':'general-purpose','run_in_background':False}
                elif child and n==1:name,args='ToolSearch',{'query':'+cutex agent_list'}
                elif child and n==2:name,args='mcp__cutex__agent_list',{'arguments':{}}
                else:name,args=None,None
                stop='tool_use' if name else 'end_turn'
                block=({'type':'tool_use','id':f'toolu_{kind}_{n}','name':name,'input':{}} if name else {'type':'text','text':''})
                delta=({'type':'input_json_delta','partial_json':json.dumps(args)} if name else {'type':'text_delta','text':f'{kind}_proof_complete'})
                msg={'id':f'msg_{kind}_{n}','type':'message','role':'assistant','model':'claude-sonnet-4-6','content':[],'stop_reason':None,'stop_sequence':None,'usage':{'input_tokens':10,'output_tokens':0}}
                events=[('message_start',{'type':'message_start','message':msg}),('content_block_start',{'type':'content_block_start','index':0,'content_block':block}),
                    ('content_block_delta',{'type':'content_block_delta','index':0,'delta':delta}),('content_block_stop',{'type':'content_block_stop','index':0}),
                    ('message_delta',{'type':'message_delta','delta':{'stop_reason':stop,'stop_sequence':None},'usage':{'output_tokens':10}}),('message_stop',{'type':'message_stop'})]
                raw=''.join(f'event: {a}\ndata: {json.dumps(b)}\n\n' for a,b in events).encode();ctype='text/event-stream'
            self.send_response(200);self.send_header('Content-Type',ctype);self.send_header('Content-Length',str(len(raw)));self.end_headers();self.wfile.write(raw)
    model=ThreadingHTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=model.serve_forever,daemon=True).start()
    key=tmp_path/'fake-api-key.json';key.write_text('{"apiKey":"private-local-fixture"}')
    api=ApiAccount('fixture','ANTHROPIC_API_KEY',f'http://127.0.0.1:{model.server_address[1]}',
        {'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC':'1','DISABLE_TELEMETRY':'1','DISABLE_ERROR_REPORTING':'1'},'private-local-fixture',key)
    driver=OfficialSdkDriver(cli_path,account_home,api_account=api)
    h=SubHarness(Settings(Path(record['cwd']),state,model='claude-sonnet-4-6',mock=False,backend='subscription',thinking='disabled',
        external_owner_id=identity,external_runtime_generation=record['runtime_generation']+1),driver)
    await h.init()
    mcp_env=dict(env,CUTEX_AGENT_ID='claude.fixture',CUTEX_RUNTIME_GENERATION='1',CUTEX_AGENT_BUS_TOKEN='private-fixture-bus-token')
    p=await asyncio.create_subprocess_exec(str(facade),env=mcp_env,stdin=asyncio.subprocess.PIPE,stdout=asyncio.subprocess.PIPE,stderr=asyncio.subprocess.PIPE,**flags)
    out,err=await finish_process(p,b'{"jsonrpc":"2.0","id":1,"method":"tools/list"}\n')
    assert p.returncode==0,err.decode()
    descriptor=json.loads(out)['result'];(state/'cutex-mcp-tools.json').write_text(json.dumps({'executable':str(facade),'tools':descriptor['tools'],'parentName':'Fixture Parent'}))
    load(h.tools,state/'cutex-mcp-tools.json')
    actual=h.tools.tools['agent_list'].handler;observed=[];proofs=[]
    original_dispatch=h.server.dispatch
    async def dispatch(peer,method,params):
        result=await original_dispatch(peer,method,params)
        if method=='claude/subagent/verify':proofs.append({'params':params,'result':result[0]})
        return result
    h.server.dispatch=dispatch
    async def read(args,context):
        local=(await original_dispatch(h.peer,'claude/subagent/verify',{'threadId':tid,'childId':context.native_subagent_id}))[0]
        result=await actual(args,context);observed.append({'childId':context.native_subagent_id,'success':result.success,'result':result.text,'localProof':local})
        return result
    h.tools.tools['agent_list'].handler=read
    token=rpc_token(state,'CUTEX_UNUSED_SUBAGENT_FIXTURE_TOKEN');stop=asyncio.Event()
    transport=asyncio.create_task(websocket(h.server,'ws://127.0.0.1:0',token,stop,ready_file=state/'server.json'))
    processes=[];logs=[]
    try:
        async with asyncio.timeout(8):
            while not (state/'server.json').exists():await asyncio.sleep(.01)
        async with httpx.AsyncClient(trust_env=False,timeout=30) as http:
            for service,portnum in [('agent',bp),('management',mp)]:
                log=open(tmp_path/(service+'.log'),'wb');logs.append(log)
                process=await asyncio.create_subprocess_exec(str(binary),service,'serve','--port',str(portnum),env=env,stdout=log,stderr=log,**flags);processes.append(process)
                async with asyncio.timeout(15):
                    while True:
                        assert process.returncode is None,(tmp_path/(service+'.log')).read_text()
                        try:
                            if (await http.get(f'http://127.0.0.1:{portnum}/')).is_success:break
                        except httpx.TransportError:pass
                        await asyncio.sleep(.05)
            started=await command('human','start',identity)
            assert started['cutex']['result']['status']=='online'
            await h.turn(tid,'Run the synthetic read-only native child proof.')
            async with asyncio.timeout(60):
                while h.engine.workers:await asyncio.sleep(.02)
            assert len(observed)==1 and observed[0]['childId']
            assert observed[0]['success'],observed
            result=json.loads(observed[0]['result'])
            assert result.get('ok') is not False and result.get('status')!='no_write',result
            assert steps=={'parent':2,'child':3},'passive proof must never generate a model request'
            assert len(sqlite_sessions(config))==1,'no extra persistent Cutex agents'
            evidence={'identity':identity,'observed':observed,'stages':steps,'noNewAgents':True,'noProofModelRequests':True}
            (tmp_path/'integration-proof.json').write_text(json.dumps(evidence,indent=2))
    finally:
        (tmp_path/'proof-trace.json').write_text(json.dumps({'proofs':proofs,'reads':observed},indent=2))
        (tmp_path/'model-requests.json').write_text(json.dumps(requests,indent=2))
        await h.close();stop.set();await transport
        for process in reversed(processes):
            if process.returncode is None:
                process.terminate()
                try:await asyncio.wait_for(process.wait(),5)
                except TimeoutError:process.kill();await process.wait()
        for log in logs:log.close()
        model.shutdown();model.server_close()
