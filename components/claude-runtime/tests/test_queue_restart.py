"""Adapter restart of a proved empty outage; real fixture process/lease, fake SDK."""
import asyncio
import json
import uuid
from pathlib import Path

import pytest

from cute_claude.config import Settings
from cute_claude.queue_recovery import empty_history, read
from test_subscription import SubHarness
from test_persistent import ph, until
from test_queue_recovery import submit, failure, RESET, SUCCESS
from fake_sdk import install_fake_sdk, materialize


@pytest.mark.parametrize('activation', ['start-first', 'gate-first', 'paused', 'stop-during-proof', 'dispatch-during-proof', 'pause-during-proof', 'preconnect-restart'])
async def test_five_inputs_recover_automatically_across_adapter_restart(tmp_path, monkeypatch, activation):
    clock, waits, prompts, delivered = [0.0], [], [], []
    monkeypatch.setattr('cute_claude.queue_recovery.time_now', lambda: clock[0])
    monkeypatch.setattr('cute_claude.queue_recovery.BASE_DELAY', 10)
    monkeypatch.setattr('cute_claude.queue_recovery.random.uniform', lambda lo,hi:20)
    driver, sdk, _ = install_fake_sdk(monkeypatch, tmp_path)
    work = tmp_path/'work'; work.mkdir()
    settings = Settings(work,tmp_path/'state',model='sonnet',mock=False,backend='subscription')
    h = SubHarness(settings,driver)
    second = None
    old_closed = False
    frozen = asyncio.Event()
    history_rows = []
    async def wait_old(event, seconds):
        if read(h.store,tid)['probeCount'] == 2:
            frozen.set()
            await event.wait()
        else:
            waits.append(seconds); clock[0] += seconds
            await asyncio.sleep(0)
    monkeypatch.setattr('cute_claude.queue_recovery.wait_backoff', wait_old)
    def bind(harness,client,tid):
        async def query(prompt):
            text,_ = await materialize(prompt)
            prompts.append(text); client.query_count += 1
            sid = harness.engine.session(tid)['session_id']
            if clock[0] < 300:
                user = str(uuid.uuid4()); error = str(uuid.uuid4())
                history_rows.append({'type':'user','uuid':user,'sessionId':sid,
                    'parentUuid':history_rows[-1]['uuid'] if history_rows else None,
                    'message':{'content':[{'type':'text','text':text}]}})
                history_rows.append({'type':'assistant','uuid':error,'sessionId':sid,'parentUuid':user,
                    'isApiErrorMessage':True,'error':'server_error',
                    'message':{'content':[{'type':'text','text':'synthetic transport error'}]}})
                path = Path(harness.engine.driver.config_dir)/'projects'/'fixture'/f'{sid}.jsonl'
                path.parent.mkdir(parents=True,exist_ok=True)
                path.write_text(''.join(json.dumps(r)+'\n' for r in history_rows))
                await client.events.put(failure(RESET,uuid=error))
                failed = True
            else:
                row = harness.store.db.execute("SELECT message_id FROM inputs WHERE thread_id=? AND state='processing'",(tid,)).fetchone()
                delivered.append(row[0])
                await client.events.put(SUCCESS)
                failed = False
            await client.events.put({'type':'result','session_id':sid,'subtype':'error_during_execution' if failed else 'success',
                'is_error':failed,'num_turns':1,'usage':{}})
        client.query = query
    try:
        await h.init(); tid = await h.thread()
        await h.call('claude/runtime/start',{'threadId':tid})
        bind(h,sdk.clients[0],tid)
        if activation=='preconnect-restart':
            from cute_claude.sdk_runtime import safe_error
            original_run = driver.run
            runs = []
            async def connect_failure(**kwargs):
                runs.append(True)
                if len(runs)==2: raise safe_error(ConnectionResetError('synthetic'),'connect')
                await original_run(**kwargs)
            driver.run=connect_failure
        jobs = [await submit(h,tid,'restart-'+str(n)) for n in range(5)]
        before = [tuple(r) for r in h.store.db.execute('SELECT seq,message_id,semantic_hash,payload,turn_id FROM inputs ORDER BY seq')]
        await until(frozen.is_set)
        assert clock[0] == 10 and len(prompts) == (1 if activation=='preconnect-restart' else 2)
        assert not h.store.thread(tid)['halted']
        assert json.loads(h.store.db.execute('SELECT value FROM meta WHERE key=?',('claudeQueueRetry:'+jobs[0]['turnId'],)).fetchone()[0])['receipt']['history']
        # Crash at a quiescent backoff: no turn callback or query is in progress.
        # Confirm child exit and release locks, but do not issue owner Stop/Resume.
        h.engine.closing = True
        for worker in list(h.engine.workers.values()): worker.cancel()
        await asyncio.gather(*list(h.engine.workers.values()),return_exceptions=True)
        await driver.close(); await h.tools.close(); h.store.close(); old_closed = True
        driver2,sdk2,_ = install_fake_sdk(monkeypatch,tmp_path)
        second = SubHarness(settings,driver2); await second.init()
        assert tid in second.store.recovered_probes
        snapshot = await second.call('thread/read',{'threadId':tid,'includeTurns':True})
        restored = next(turn for turn in snapshot['thread']['turns'] if turn['id']==jobs[0]['turnId'])
        assert restored['error'] is None and restored['claudeRetry']['willRetry']
        assert restored['claudeRetry']['retryableInput'] and restored['claudeRetry']['turnId']==jobs[0]['turnId']
        await second.call('thread/resume',{'threadId':tid})
        assert not sdk2.clients and not second.engine.workers, 'read/resume alone is not activation'
        assert any(e.get('method')=='error' and e['params'].get('willRetry') for e in second.out)
        async def advance(event,seconds):
            assert not second.store.thread(tid)['halted']
            assert read(second.store,tid)['count'] == 0
            waits.append(seconds); clock[0] += seconds
            await asyncio.sleep(0)
        monkeypatch.setattr('cute_claude.queue_recovery.wait_backoff',advance)
        # Bind the client as soon as it connects, before post-response restore wakes.
        original_start = driver2.start
        async def start(engine,thread):
            result = await original_start(engine,thread)
            bind(second,sdk2.clients[0],tid)
            return result
        driver2.start = start
        if activation == 'gate-first':
            await second.call('claude/toolGate/set',{'threadId':tid,'held':False})
        await second.call('claude/runtime/start',{'threadId':tid})
        if activation != 'gate-first':
            await asyncio.sleep(0)
            assert sdk2.clients[0].query_count == 0, 'must wait for daemon pause reconciliation'
            if activation == 'paused':
                await second.call('claude/toolGate/set',{'threadId':tid,'held':True})
                await asyncio.sleep(.02)
                assert sdk2.clients[0].query_count == 0
            if activation.endswith('during-proof'):
                import threading
                import cute_claude.queue_recovery as recovery
                original_history = recovery.empty_history
                entered,release = threading.Event(),threading.Event()
                def blocked(*args,**kwargs):
                    entered.set()
                    assert release.wait(5)
                    return original_history(*args,**kwargs)
                monkeypatch.setattr(recovery,'empty_history',blocked)
                gate = asyncio.create_task(second.call('claude/toolGate/set',{'threadId':tid,'held':False}))
                await until(entered.is_set)
                if activation=='stop-during-proof':
                    await second.call('turn/interrupt',{'threadId':tid,'turnId':jobs[0]['turnId']})
                elif activation=='dispatch-during-proof':
                    key = 'claudeQueueRetry:'+jobs[0]['turnId']
                    retry = json.loads(second.store.db.execute('SELECT value FROM meta WHERE key=?',(key,)).fetchone()[0])
                    retry['dispatch']='submitted'
                    second.store.db.execute('UPDATE meta SET value=? WHERE key=?',(json.dumps(retry),key))
                else:
                    await second.call('claude/toolGate/set',{'threadId':tid,'held':True})
                release.set(); await gate
                assert sdk2.clients[0].query_count == 0
                assert second.store.thread(tid)['halted']
                if activation=='stop-during-proof':
                    assert second.store.turn(jobs[0]['turnId'])['status']=='interrupted'
                    assert all(second.store.input_status(tid,'restart-'+str(n))['state']=='queued' for n in range(1,5))
                    assert not second.engine.workers
                    return
                if activation=='dispatch-during-proof':
                    assert second.store.turn(jobs[0]['turnId'])['status']=='inProgress'
                    assert not second.engine.workers
                    return
                monkeypatch.setattr(recovery,'empty_history',original_history)
            await second.call('claude/toolGate/set',{'threadId':tid,'held':False})
        await second.drain()
        assert clock[0] == 300 and waits == [10,30,60,200]
        assert delivered == ['restart-'+str(n) for n in range(5)]
        assert sum('do queued delegation restart-0' in p for p in prompts) == 1
        assert [tuple(r) for r in second.store.db.execute('SELECT seq,message_id,semantic_hash,payload,turn_id FROM inputs ORDER BY seq')] == before
        assert all(second.store.turn(j['turnId'])['status']=='completed' for j in jobs)
        assert not second.store.db.execute("SELECT 1 FROM turns WHERE state='failed'").fetchone()
        assert not any(e.get('method')=='claude/queue/holdChanged' for e in second.out)
    finally:
        if second is not None: await second.close()
        if not old_closed: await h.close()


@pytest.mark.parametrize('effect',['output','tool','thinking','sidechain','new-tip'])
def test_restart_history_proof_rejects_nonempty_or_changed_chain(tmp_path,effect):
    from types import SimpleNamespace
    sid,user,err = (str(uuid.uuid4()) for _ in range(3))
    rows = [{'type':'user','uuid':user,'sessionId':sid,'parentUuid':None,'message':{'content':[{'type':'text','text':'original'}]}},
        {'type':'assistant','uuid':err,'sessionId':sid,'parentUuid':user,'isApiErrorMessage':True,'message':{'content':[{'type':'text','text':'error'}]}}]
    p = tmp_path/'projects'/'fixture'/f'{sid}.jsonl';p.parent.mkdir(parents=True)
    owner = SimpleNamespace(driver=SimpleNamespace(config_dir=tmp_path))
    p.write_text(''.join(json.dumps(r)+'\n' for r in rows))
    assert empty_history(owner,sid,err)['anchor']==user
    if effect=='output': rows[-1]['isApiErrorMessage']=False
    elif effect in {'tool','thinking'}: rows[0]['message']['content']=[{'type':'tool_result' if effect=='tool' else 'thinking'}]
    elif effect=='sidechain': rows[-1]['isSidechain']=True
    else: rows.append({'type':'assistant','uuid':str(uuid.uuid4()),'sessionId':sid,'parentUuid':err,'message':{'content':[]}})
    p.write_text(''.join(json.dumps(r)+'\n' for r in rows))
    assert empty_history(owner,sid,err) is None


@pytest.mark.parametrize('attachment', [
    {'type':'date','date':'2026-11-15'},
    {'type':'prompt_snapshot','systemPrompt':[],'reminderFold':False,'echoWireToolInputs':False,'contextRendering':'fixture'},
    {'type':'task_notification','text':'not metadata'},
])
def test_native_attachment_parent_chain_is_walked_and_checked(tmp_path, attachment):
    from types import SimpleNamespace
    sid,user,meta,err = (str(uuid.uuid4()) for _ in range(4))
    rows = [
        {'type':'user','uuid':user,'sessionId':sid,'parentUuid':None,'message':{'content':'original'}},
        {'type':'attachment','uuid':meta,'sessionId':sid,'parentUuid':user,'attachment':attachment},
        {'type':'assistant','uuid':err,'sessionId':sid,'parentUuid':meta,'isApiErrorMessage':True,'message':{'content':'error'}},
    ]
    p=tmp_path/'projects'/'fixture'/f'{sid}.jsonl';p.parent.mkdir(parents=True)
    p.write_text(''.join(json.dumps(r)+'\n' for r in rows))
    proof = empty_history(SimpleNamespace(driver=SimpleNamespace(config_dir=tmp_path)),sid,err)
    assert (proof is not None) == (attachment['type'] != 'task_notification')


async def test_pre_submission_probe_failure_keeps_the_saved_native_context(ph, monkeypatch):
    from cute_claude.sdk_runtime import safe_error
    from cute_claude.queue_recovery import restart_candidate
    monkeypatch.setattr('cute_claude.queue_recovery.BASE_DELAY',60)
    tid = await ph.thread(); await ph.engine.start_runtime(tid)
    client = ph.sdk.clients[0]
    sid = ph.engine.session(tid)['session_id']
    user,error = str(uuid.uuid4()),str(uuid.uuid4())
    seen = []
    async def query(prompt):
        text,_ = await materialize(prompt); seen.append(text);client.query_count+=1
        if client.query_count==1:
            path=Path(ph.engine.driver.config_dir)/'projects'/'fixture'/f'{sid}.jsonl';path.parent.mkdir(parents=True)
            path.write_text(json.dumps({'type':'user','uuid':user,'parentUuid':None,'sessionId':sid,'message':{'content':text}})+'\n'+json.dumps({'type':'assistant','uuid':error,'parentUuid':user,'sessionId':sid,'isApiErrorMessage':True,'message':{'content':'synthetic error'}})+'\n')
            await client.events.put(failure(RESET,uuid=error))
        else: await client.events.put(SUCCESS)
        await client.events.put({'type':'result','session_id':sid,'subtype':'error_during_execution' if client.query_count==1 else 'success','is_error':client.query_count==1,'num_turns':1,'usage':{}})
    client.query=query
    run=ph.engine.driver.run; runs=[]
    async def fail_before_submit(**kwargs):
        runs.append(True)
        if len(runs)==2: raise safe_error(ConnectionResetError('synthetic'),'connect')
        await run(**kwargs)
    monkeypatch.setattr(ph.engine.driver,'run',fail_before_submit)
    frozen,release=asyncio.Event(),asyncio.Event()
    async def backoff(event,seconds):
        if read(ph.store,tid)['probeCount']==2:
            frozen.set();await release.wait()
        else: await asyncio.sleep(0)
    monkeypatch.setattr('cute_claude.queue_recovery.wait_backoff',backoff)
    turn=await ph.turn(tid,'ORIGINAL saved context')
    await until(frozen.is_set)
    retry=json.loads(ph.store.db.execute('SELECT value FROM meta WHERE key=?',('claudeQueueRetry:'+turn,)).fetchone()[0])
    assert retry['continuation'] and retry['dispatch']=='not-submitted'
    assert retry['receipt']['state']=='terminal-empty' and retry['receipt']['errorUuid']==error
    assert retry['receipt']['history']['anchor']==user
    assert restart_candidate(ph.store,tid,turn,retry)
    release.set();await ph.drain()
    assert ph.store.turn(turn)['status']=='completed' and client.query_count==2
    assert sum('ORIGINAL saved context' in text for text in seen)==1
