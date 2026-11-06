"""Private end-to-end scaling gate; Python stdlib only, Windows/Linux.

Exit 0: gate passes; 1: measured scale violation; 2: invalid/unmeasured run.
"""
import argparse, concurrent.futures, ctypes, datetime, hashlib, json, os, pathlib, socket, statistics, subprocess, sys, threading, time, urllib.request, uuid
from fixture import generate, secure

FORBIDDEN={24270,24260,24250,24670,24660,25370}

def audit_seed(home):
    """Check seeded business predicates before the daemon can migrate the files."""
    root=home/'.cutex'; fixture=json.loads((home/'fixture.json').read_text()); expected=fixture['expected']
    sessions=json.loads((root/'cutex-sessions.json').read_text())['sessions']
    active={sid:r for sid,r in sessions.items() if r['lifecycle']=='active'}
    if set(active)!=set(expected['active_session_ids']) or any(not r.get('agent_enabled') or r.get('registration_class')!='persistent' or not r.get('exposed_to_backend') for r in active.values()):
        raise RuntimeError('fixture seed: sessions would be hidden or excluded by managed predicates')
    roster_dir=root/'runtime/agent-management/v1'
    roster=json.loads((roster_dir/'agent-management-v1.json').read_text())
    if not (roster_dir/'agent-management-v1.lock').exists(): raise RuntimeError('fixture seed: missing roster lock makes snapshot_shared return an empty roster')
    if len(roster['agents'])!=expected['roster_agents'] or len(roster['actions'])!=expected['roster_actions']: raise RuntimeError('fixture seed: roster counts mismatch')
    if any(not r['created_at'].endswith('Z') or not r['updated_at'].endswith('Z') for r in roster['actions'].values()): raise RuntimeError('fixture seed: noncanonical typed roster timestamp')
    messages=json.loads((root/'runtime/management-v2/agent-bus-message-state.json').read_text())['messages']
    if len(messages)!=expected['bus_history_rows']+1 or sum(r['snapshot']['state']=='delivered' for r in messages.values())!=expected['bus_history_rows']: raise RuntimeError('fixture seed: bus history count mismatch')
    notes=json.loads((root/'runtime/owner-notify/owner-notify-v1.json').read_text())['notifications']
    if len(notes)!=expected['final_notifications']+1: raise RuntimeError('fixture seed: notification history count mismatch')
    return dict(visible_active_sessions=len(active),roster_agents=len(roster['agents']),roster_actions=len(roster['actions']),bus_completed_history=expected['bus_history_rows'],final_notifications=expected['final_notifications'],roster_lock=True,canonical_roster_timestamps=True)

def validate_response(path,result,expected,anchor):
    """Fail closed on successful-but-filtered responses, using fixture identities."""
    def require(ok,message):
        if not ok: raise RuntimeError(f'fixture visibility: {path}: {message}')
    summary={}
    if path=='/v2/sessions':
        rows=result.get('sessions',[]); ids={r.get('cutexSessionId') for r in rows}
        require(len(rows)==166 and ids==set(expected['active_session_ids']),'expected exactly 166 visible active synthetic sessions')
        require(all(r.get('visible') is True for r in rows),'session resources must explicitly be visible')
        summary=dict(rows=len(rows),first_id=sorted(ids)[0],last_id=sorted(ids)[-1])
    elif path=='/v2/host/sessions':
        rows=result.get('data',[])
        require(len(rows)==1 and rows[0].get('id')==expected['host_match'],'expected exactly the selected host match')
        require(rows[0].get('kind')=='Agent' and rows[0].get('state')=='Offline','expected an offline managed Agent')
        require(rows[0].get('projectId')=='scale-project','roster project association missing')
        summary=dict(rows=1,id=rows[0]['id'],project=rows[0]['projectId'])
    elif path.endswith('/cutex/requests'):
        require(result.get('cutex',{}).get('method')=='cutex/userInput/queue/list','wrong queue dispatch branch')
        queue=result.get('cutex',{}).get('result',{}); rows=queue.get('items',[])
        require(len(rows)==1 and rows[0].get('queueId')==expected['queue_id'],'selected queued sentinel missing')
        require(queue.get('paused') is True,'fixture queue must remain paused')
        summary=dict(rows=1,id=rows[0]['queueId'],paused=True)
    elif path.startswith('/v2/usage?'):
        rows=result.get('rows',[]); wanted=3 if 'source=' in path else expected['usage_total']
        require(len(rows)==1 and result.get('totals',{}).get('total')==wanted,'expected native/external selected samples and exact token total')
        require(not any('invalid usage' in x or 'ignored invalid' in x for x in result.get('warnings',[])),'invalid usage rows skipped')
        summary=dict(rows=1,total=wanted)
    elif path.endswith('/quota'):
        require(result.get('source')=='claudeApi','wrong quota source')
        native=result.get('nativeQuota',{})
        require(native.get('claudeAccount',{}).get('id')=='ca-scale','wrong API account')
        require(native.get('apiUsage',{}).get('last30Days',{}).get('total')==expected['usage_total'],'selected quota evidence missing (anchor must be in current 30-day window)')
        summary=dict(source='claudeApi',last30DaysTotal=expected['usage_total'])
    elif path.startswith('/v2/asks?'):
        rows=result.get('asks',[])
        require(len(rows)==10 and {r.get('askId') for r in rows}=={f'ask-scale-{i:06}' for i in range(10)},'expected ten exact closed ask identities')
        require(all(r.get('closed')=='completed' and r.get('answerId')==f"answer-{int(r['askId'].rsplit('-',1)[1])}" and r.get('answerPreview','').startswith('Synthetic answer') for r in rows),'wrong delegation state / missing answer evidence')
        summary=dict(rows=10,closed='completed')
    elif path.startswith('/v2/owner-notifications?'):
        rows=result.get('notifications',[])
        require(len(rows)==1 and rows[0].get('notificationId')==expected['pending_notification'],'pending notification sentinel filtered out')
        require(rows[0].get('needsReply') is True and rows[0].get('state')=='open','wrong notification predicate')
        summary=dict(rows=1,id=rows[0]['notificationId'])
    elif path=='/v2/health':
        rows=result.get('findings',[])
        require(any(r.get('code')=='stuck_bus_messages' and r.get('detail',{}).get('count')==1 and r.get('detail',{}).get('liveQueue',{}).get('available') is True for r in rows),'health did not see the aged held bus sentinel / live private bus')
        summary=dict(stuck_bus_messages=1)
    elif path.startswith('/v2/agent-bus/stuck-messages?'):
        rows=result.get('messages',[])
        require(result.get('total')==1 and len(rows)==1 and rows[0].get('messageId')==expected['bus_sentinel'],'pending bus sentinel absent from actual read route')
        require(result.get('liveQueue',{}).get('available') is True,'private bus queue observation unavailable')
        summary=dict(rows=1,id=rows[0]['messageId'])
    elif path.startswith('/v2/agent-bus/stuck-messages/content?'):
        require(result.get('messageId')=='bus-scale-0' and result.get('durable') is True and bool(result.get('text')),'completed historical bus content unavailable')
        summary=dict(id='bus-scale-0',bytes=len(result['text']))
    else:
        raise RuntimeError(f'no fixture-result validator for {path}')
    return summary
def private_bytes(pid):
    if os.name=='nt':
        from ctypes import wintypes as w
        class Counters(ctypes.Structure):
            _fields_=[('cb',w.DWORD),('faults',w.DWORD)]+[(n,ctypes.c_size_t) for n in ['peak_ws','ws','peak_paged','paged','peak_nonpaged','nonpaged','pagefile','peak_pagefile','private']]
        k=ctypes.WinDLL('kernel32',use_last_error=True); ps=ctypes.WinDLL('psapi',use_last_error=True)
        k.OpenProcess.restype=w.HANDLE; k.OpenProcess.argtypes=[w.DWORD,w.BOOL,w.DWORD]; k.CloseHandle.argtypes=[w.HANDLE]
        ps.GetProcessMemoryInfo.argtypes=[w.HANDLE,ctypes.c_void_p,w.DWORD]
        h=k.OpenProcess(0x1000|0x10,False,pid)
        if not h: raise OSError(ctypes.get_last_error(),'OpenProcess')
        try:
            c=Counters(); c.cb=ctypes.sizeof(c)
            if not ps.GetProcessMemoryInfo(h,ctypes.byref(c),c.cb): raise OSError(ctypes.get_last_error(),'GetProcessMemoryInfo')
            return c.private
        finally: k.CloseHandle(h)
    values=pathlib.Path(f'/proc/{pid}/smaps_rollup').read_text().splitlines()
    return sum(int(x.split()[1])*1024 for x in values if x.startswith(('Private_Clean:','Private_Dirty:','Private_Hugetlb:')))

def env_for(home,metrics):
    env={k:v for k,v in os.environ.items() if not k.upper().startswith(('CUTEX','CODEX','CLAUDE','ANTHROPIC','OPENAI','DEEPSEEK','KIMI')) and k.upper() not in ('HOME','USERPROFILE','TEMP','TMP','TMPDIR')}
    env.update(HOME=str(home),CUTEX_TEST_PRIVATE_HOME=str(home),CUTEX_TEST_FORBID_REAL_HOME='1',CUTEX_SCALE_METRICS=str(metrics),TEMP=str(home/'tmp'),TMP=str(home/'tmp'),TMPDIR=str(home/'tmp'))
    (home/'tmp').mkdir(exist_ok=True)
    return env

class Instance:
    def __init__(self,exe,home,out,mp,bp):
        self.home=home; self.out=out; self.mp=mp; self.bp=bp; self.processes=[]; self.logs=[]
        self.token=json.loads((home/'.cutex/config.json').read_text())['management_api_token']
        fixture=json.loads((home/'fixture.json').read_text()); self.expected=fixture['expected']; self.anchor=fixture['anchor']
        self.opener=urllib.request.build_opener(urllib.request.ProxyHandler({}))
        for port in (mp,bp):
            if port in FORBIDDEN or not 1024<=port<65536: raise ValueError('unsafe port')
            with socket.socket() as s: s.bind(('127.0.0.1',port))
        self.exe=exe
    def launch(self,args,name):
        log=(self.out/f'{name}.log').open('wb'); self.logs.append(log)
        p=subprocess.Popen([str(self.exe),*args],env=env_for(self.home,self.out/f'{name}.jsonl'),stdout=log,stderr=log,creationflags=getattr(subprocess,'CREATE_NO_WINDOW',0)); self.processes.append(p)
        return p
    def start(self):
        self.launch(['agent','serve','--port',str(self.bp)],'bus')
        deadline=time.monotonic()+120
        while time.monotonic()<deadline:
            if self.processes[0].poll() is not None: raise RuntimeError('bus exited; inspect bus.log')
            with socket.socket() as s:
                if s.connect_ex(('127.0.0.1',self.bp))==0: break
            time.sleep(.1)
        else: raise TimeoutError('bus startup')
        self.launch(['management','serve','--port',str(self.mp),'--bind',f'127.0.0.1:{self.mp}'],'management')
        deadline=time.monotonic()+180
        while time.monotonic()<deadline:
            if self.processes[-1].poll() is not None: raise RuntimeError('management exited; inspect management.log')
            try:
                self.request('/v2/health'); return
            except (OSError,TimeoutError): time.sleep(.2)
        raise TimeoutError('management startup')
    def request(self,path,body=None):
        data=None if body is None else json.dumps(body).encode()
        req=urllib.request.Request(f'http://127.0.0.1:{self.mp}{path}',data=data,headers={'Authorization':f'Bearer {self.token}','Content-Type':'application/json'})
        before=time.perf_counter()
        with self.opener.open(req,timeout=180) as r: payload=r.read(); status=r.status
        result=json.loads(payload)
        if status!=200 or isinstance(result,dict) and result.get('error'): raise RuntimeError(f'endpoint error: {path}: {result}')
        latency=(time.perf_counter()-before)*1000
        verified=validate_response(path,result,self.expected,self.anchor)
        return dict(ms=latency,response_bytes=len(payload),result=result,verified=verified)
    def memory(self): return sum(private_bytes(p.pid) for p in self.processes if p.poll() is None)
    def audit_routes(self):
        paths=['/v2/agent-bus/stuck-messages?limit=10','/v2/agent-bus/stuck-messages/content?messageId=bus-scale-0']
        paths += [f'/v2/usage?from={self.anchor}&to={self.anchor}&groupBy=day&source={source}' for source in ('cutex','scale-external')]
        return {path:self.request(path)['verified'] for path in paths}
    def close(self):
        # Recorded Popen handles only. Never process-name or port based termination.
        for p in reversed(self.processes):
            if p.poll() is None: p.terminate()
        for p in self.processes:
            try: p.wait(timeout=15)
            except subprocess.TimeoutExpired: p.kill(); p.wait()
        for log in self.logs: log.close()

def metrics(path):
    if not path.exists(): return []
    text=path.read_text()
    # A sampler may observe an unfinished final write. Earlier corrupt lines
    # are fatal, rather than silently losing request counters.
    return [json.loads(x) for x in text[:text.rfind('\n')+1].splitlines() if x.strip()]

def batch(instance,path,body,n,repeats,memory_limit):
    baseline=instance.memory(); peak=[baseline]; stop=threading.Event(); error=[]
    def sample():
        while not stop.wait(.01):
            try:
                peak[0]=max(peak[0],instance.memory())
                if peak[0]>memory_limit:
                    error.append('private-memory ceiling exceeded');
                    for p in instance.processes:
                        if p.poll() is None: p.terminate()
                    return
            except OSError as e: error.append(str(e)); return
    log=instance.out/'management.jsonl'; before=len(metrics(log)); thread=threading.Thread(target=sample); thread.start()
    results=[]
    try:
        for _ in range(repeats):
            barrier=threading.Barrier(n)
            def one(_): barrier.wait(); return instance.request(path,body)
            with concurrent.futures.ThreadPoolExecutor(max_workers=n) as pool: results.extend(pool.map(one,range(n)))
        deadline=time.monotonic()+5
        while time.monotonic()<deadline:
            rows=[x for x in metrics(log)[before:] if x.get('kind')=='request' and x['path']==path]
            if len(rows)==n*repeats: break
            time.sleep(.01)
        if len(rows)!=n*repeats: raise RuntimeError('missing request parse counters')
        if error: raise RuntimeError(error[0])
    finally: stop.set(); thread.join()
    times=sorted(r['ms'] for r in results)
    return dict(concurrency=n,requests=len(results),parse_bytes_per_request=statistics.mean(x['parse_bytes'] for x in rows),latency_median_ms=statistics.median(times),latency_p95_ms=times[min(len(times)-1,int(len(times)*.95))],peak_private_bytes=peak[0],idle_private_bytes=baseline,transient_private_bytes=max(0,peak[0]-baseline),response_bytes=statistics.median(r['response_bytes'] for r in results),verified=results[0]['verified'])

CASES=[('health','/v2/health',None),('sessions','/v2/sessions',None),('host-session-page','/v2/host/sessions',{'limit':10,'query':'Scale-00000'}),('queue-list','/v2/sessions/cutex.scale-00000/cutex/requests',{'requestId':'scale-read','method':'cutex/userInput/queue/list','params':{}}),('usage-day','/v2/usage?from=2026-10-30&to=2026-10-30&groupBy=day',None),('claude-api-quota','/v2/profiles/scale-api/quota',None),('delegation-restore','/v2/asks?state=closed&target=cutex.scale-00000&limit=10',None),('notifications','/v2/owner-notifications?filter=pending&limit=10',None)]

def compare(small,large):
    failures=[]
    for key,b in small.items():
        a=large[key]; n=a['concurrency']
        # Tenfold unrelated history may not grow parser work beyond small
        # constant configuration (16 KiB), nor transient allocations beyond 16 MiB/reader.
        for metric,budget in [('parse_bytes_per_request',b['parse_bytes_per_request']*1.5+16*1024),('transient_private_bytes',b['transient_private_bytes']*2+n*16*1024**2),('latency_median_ms',b['latency_median_ms']*4+50)]:
            if a[metric]>budget: failures.append(dict(case=key,metric=metric,actual=a[metric],budget=budget))
    return failures

def idle(instance,seconds):
    before=metrics(instance.out/'bus.jsonl'); peak=instance.memory(); deadline=time.monotonic()+seconds
    while time.monotonic()<deadline:
        peak=max(peak,instance.memory()); time.sleep(.01)
    after=metrics(instance.out/'bus.jsonl')
    totals=lambda xs:[x['parse_bytes'] for x in xs if x['kind']=='total']
    if not totals(before) or not totals(after): raise RuntimeError('missing bus idle counters')
    return dict(seconds=seconds,parse_bytes=totals(after)[-1]-totals(before)[-1],peak_private_bytes=peak)

def main():
    p=argparse.ArgumentParser(); p.add_argument('--exe',type=pathlib.Path,required=True); p.add_argument('--output',type=pathlib.Path,required=True); p.add_argument('--home-root',type=pathlib.Path,default=pathlib.Path.home()); p.add_argument('--scales',default='0.1,1'); p.add_argument('--concurrency',default='1,2,4'); p.add_argument('--repeats',type=int,default=3); p.add_argument('--history',type=int,default=1); p.add_argument('--anchor',default=str(datetime.date.today())); p.add_argument('--histories',help='paired history multipliers, e.g. 1,10 with --scales 1,1'); p.add_argument('--management-port',type=int,default=24870); p.add_argument('--bus-port',type=int,default=24860); p.add_argument('--memory-limit-mib',type=int,default=4096); p.add_argument('--inventory',type=pathlib.Path); a=p.parse_args()
    a.output.mkdir(parents=True,exist_ok=True)
    report=dict(started=datetime.datetime.now(datetime.timezone.utc).isoformat(),exe=str(a.exe.resolve()),exe_sha256=hashlib.file_digest(a.exe.open('rb'),'sha256').hexdigest(),cases={},failures=[],errors=[])
    manifest=a.exe.with_name('scale-gate-build.json')
    if manifest.exists(): report['build']=json.loads(manifest.read_text())
    try:
        scales=list(map(float,a.scales.split(','))); histories=list(map(int,a.histories.split(','))) if a.histories else [a.history]*len(scales)
        if len(scales)!=2 or len(histories)!=2 or scales[1]*histories[1]<scales[0]*histories[0]*9.99: raise ValueError('requires exactly two fixtures and >=10x unrelated history')
        for scale,history in zip(scales,histories):
            label=str(scale) if not a.histories else f'{scale}:h{history}'
            home=a.home_root/f'.scale-gate-{uuid.uuid4().hex[:10]}'
            out=a.output/(f'scale-{scale:g}' if not a.histories else f'scale-{scale:g}-history-{history}'); out.mkdir()
            report.setdefault('fixtures',{})[label]=generate(home,scale,history,a.management_port,a.bus_port,a.inventory,a.anchor)
            report.setdefault('homes',[]).append(str(home))
            report.setdefault('fixture_audit',{})[label]=dict(seed=audit_seed(home))
            instance=Instance(a.exe.resolve(),home,out,a.management_port,a.bus_port)
            try:
                instance.start(); time.sleep(2)
                report['fixture_audit'][label]['routes']=instance.audit_routes()
                report.setdefault('startup',{})[label]=dict(private_bytes=instance.memory(),pids=[p.pid for p in instance.processes],parse_bytes={name:[x['parse_bytes'] for x in metrics(out/f'{name}.jsonl') if x['kind']=='total'][-1] for name in ('bus','management')})
                cases={}
                for name,path,body in CASES:
                    path=path.replace('2026-10-30',a.anchor)
                    report.setdefault('first_request',{}).setdefault(label,{})[name]=batch(instance,path,body,1,1,a.memory_limit_mib*1024**2)
                    for n in map(int,a.concurrency.split(',')):
                        cases[f'{name}/N={n}']=batch(instance,path,body,n,a.repeats,a.memory_limit_mib*1024**2)
                    report['cases'][label]=cases
                    (a.output/'results.json').write_text(json.dumps(report,indent=2))
                report.setdefault('bus_idle',{})[label]=idle(instance,6)
            finally: instance.close()
        scales=list(report['cases'])
        if len(scales)!=2: raise ValueError('gate requires exactly two scales')
        report['failures']=compare(report['cases'][scales[0]],report['cases'][scales[1]])
        if report['bus_idle'][scales[1]]['parse_bytes']>report['bus_idle'][scales[0]]['parse_bytes']*1.5+16*1024:
            report['failures'].append(dict(case='bus-idle',metric='parse_bytes',actual=report['bus_idle'][scales[1]]['parse_bytes']))
    except Exception as e: report['errors'].append(f'{type(e).__name__}: {e}')
    (a.output/'results.json').write_text(json.dumps(report,indent=2))
    print(json.dumps(dict(failures=report['failures'],errors=report['errors']),indent=2))
    return 2 if report['errors'] else 1 if report['failures'] else 0
if __name__=='__main__': sys.exit(main())
