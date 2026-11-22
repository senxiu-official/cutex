import asyncio
import json

import pytest

from cute_claude.queue_recovery import plan, read, remaining, transport_failure
from cute_claude.sdk_runtime import safe_error
from cute_claude.store import Store
from cute_claude.subscription import SdkResponseError
from test_transient_retry import failure, install_query
from test_persistent import ph, until

SUCCESS = {'type': 'assistant', 'content': [{'type': 'text', 'text': 'queued work completed'}]}
RESET = 'API Error: Connection dropped (ECONNRESET).'


def configure(monkeypatch):
    monkeypatch.setattr('cute_claude.transient_retry.retry_delay', lambda *args: None)
    monkeypatch.setattr('cute_claude.queue_recovery.BASE_DELAY', 0.02)


async def submit(ph, tid, mid, exclusive=False, error=False):
    params = {'threadId': tid, 'messageId': mid, 'source': {'kind': 'service', 'id': 'cutex-asks'},
              'eventType': 'ASK', 'text': 'do queued delegation ' + mid, 'metadata': {'asks': ['ask-' + mid], 'attempt': 1}}
    if exclusive:
        params['admission'] = 'exclusive_turn'
    return await ph.call('claude/input/submit', params, error=error)


@pytest.mark.parametrize('text', [RESET, 'API Error: Request timed out (ETIMEDOUT)', 'HTTP 503 temporarily unavailable',
                                 'HTTP 500 internal error', 'HTTP 529 overloaded_error'])
async def test_transport_failure_runs_queued_delegation_without_owner_input(ph, monkeypatch, text):
    configure(monkeypatch)
    tid, client = await install_query(ph, [failure(text), SUCCESS])
    first = await ph.turn(tid, 'original owner input')
    queued = await submit(ph, tid, 'next')
    await ph.drain()
    assert ph.store.turn(first)['status'] == 'completed'
    assert ph.store.turn(queued['turnId'])['status'] == 'completed'
    assert client.query_count == 3, 'retry original input once, then deliver the distinct queued input once'
    assert not ph.store.thread(tid)['halted'] and read(ph.store, tid) is None


async def test_backoff_is_busy_not_a_closed_gate_for_backend_retry(ph, monkeypatch):
    configure(monkeypatch)
    monkeypatch.setattr('cute_claude.queue_recovery.BASE_DELAY', 0.1)
    tid, client = await install_query(ph, [failure(RESET), SUCCESS])
    first = await ph.turn(tid, 'first')
    await until(lambda: tid in ph.engine.queue_backoffs)
    assert remaining(ph.store, tid) > 0 and not ph.store.thread(tid)['halted']
    busy = await submit(ph, tid, 'exclusive', exclusive=True, error=True)
    assert busy['data']['reason'] == 'active_turn'
    assert ph.store.db.execute("SELECT COUNT(*) FROM inputs WHERE message_id='exclusive'").fetchone()[0] == 0
    await ph.drain()
    queued = await submit(ph, tid, 'exclusive', exclusive=True)
    await ph.drain()
    assert ph.store.turn(queued['turnId'])['status'] == 'completed' and client.query_count == 3


async def test_empty_transport_failures_never_hold_then_recover_without_owner(ph, monkeypatch):
    configure(monkeypatch)
    tid, client = await install_query(ph, [failure(RESET)] * 5 + [SUCCESS])
    old = await ph.turn(tid, 'first')
    jobs = [await submit(ph, tid, str(n)) for n in range(3)]
    await ph.drain()
    assert client.query_count == 9 and not ph.store.thread(tid)['halted']
    assert ph.store.turn(old)['status'] == 'completed'
    assert all(ph.store.turn(job['turnId'])['status'] == 'completed' for job in jobs)
    assert not [e for e in ph.out if e.get('method') == 'claude/queue/holdChanged']
    assert not ph.store.db.execute("SELECT 1 FROM sdk_recovery_attempts").fetchone()


async def test_transport_failure_with_tool_in_flight_stays_held(ph, monkeypatch):
    configure(monkeypatch)
    async def pending_tool(tid, client):
        turn = ph.engine.retry_turns[tid]
        ph.store.db.execute("INSERT INTO tool_calls VALUES(?,?,?,'started','{}',NULL)", (tid, 'inflight', turn))
    tid, client = await install_query(ph, [failure(RESET)], pending_tool)
    await ph.turn(tid, 'first')
    queued = await submit(ph, tid, 'waiting')
    await ph.drain()
    assert ph.store.thread(tid)['halted'] and client.query_count == 1
    assert read(ph.store, tid) is None
    assert ph.store.input_status(tid, 'waiting')['state'] == 'queued'
    assert ph.store.db.execute("SELECT state FROM tool_calls WHERE call_id='inflight'").fetchone()[0] == 'uncertain'


@pytest.mark.parametrize('code,text', [('authentication_failed', 'API Error: 401 refused'),
    ('invalid_request', 'API Error: 400 invalid'), ('billing_error', 'quota exhausted')])
async def test_model_and_auth_errors_keep_their_existing_hold(ph, monkeypatch, code, text):
    configure(monkeypatch)
    tid, client = await install_query(ph, [failure(text, code)])
    await ph.turn(tid, 'first')
    await ph.drain()
    assert ph.store.thread(tid)['halted'] and read(ph.store, tid) is None


def test_guard_persists_but_window_expires_and_explicit_resume_resets(tmp_path):
    store = Store(tmp_path / 'state')
    tid = store.new_thread('model', str(tmp_path))
    err = SdkResponseError(failure(RESET), None)
    state = plan(store, tid, err, tool_in_flight=False, now=100)
    from cute_claude.queue_recovery import KEY
    store.db.execute('INSERT INTO meta VALUES(?,?)', (KEY + tid, json.dumps(state)))
    store.close()
    store = Store(tmp_path / 'state')
    assert plan(store, tid, err, tool_in_flight=False, now=101)['count'] == 2
    assert plan(store, tid, err, tool_in_flight=False, now=401)['count'] == 1
    store.resume_queue(tid)
    assert read(store, tid) is None
    store.close()


@pytest.mark.parametrize('error', [ConnectionResetError('private text'), TimeoutError('private text')])
def test_typed_transport_errors_are_sanitized(error):
    sanitized = safe_error(error, 'receive')
    assert transport_failure(sanitized) and 'private text' not in str(sanitized)


async def test_runtime_close_during_queue_backoff_does_not_wait_or_deliver(ph, monkeypatch):
    configure(monkeypatch)
    monkeypatch.setattr('cute_claude.queue_recovery.BASE_DELAY', 60)
    tid, client = await install_query(ph, [failure(RESET), SUCCESS])
    await ph.turn(tid, 'first')
    await submit(ph, tid, 'untouched')
    await until(lambda: tid in ph.engine.queue_backoffs)
    async with asyncio.timeout(1):
        await ph.engine.close_runtime(tid)
    assert client.query_count == 1 and not ph.engine.workers
    assert ph.store.input_status(tid, 'untouched')['state'] == 'queued'


async def test_close_interrupts_a_retained_input_without_waiting_for_transport(ph, monkeypatch):
    configure(monkeypatch)
    monkeypatch.setattr('cute_claude.queue_recovery.BASE_DELAY', 60)
    tid, client = await install_query(ph, [failure(RESET)])
    turn = await ph.turn(tid, 'one')
    await until(lambda: tid in ph.engine.queue_backoffs)
    assert read(ph.store, tid)['count'] == 0 and not read(ph.store, tid)['held']
    assert ph.store.turn(turn)['status'] == 'inProgress'
    await ph.engine.close_runtime(tid)
    assert ph.store.turn(turn)['status'] == 'interrupted'


async def test_five_minute_outage_auto_recovers_five_original_inputs_once_in_order(ph, monkeypatch):
    configure(monkeypatch)
    monkeypatch.setattr('cute_claude.queue_recovery.BASE_DELAY', 10)
    monkeypatch.setattr('cute_claude.queue_recovery.random.uniform', lambda lo, hi: 20)
    clock, waits, delivered, prompts = [0.0], [], [], []
    monkeypatch.setattr('cute_claude.queue_recovery.time_now', lambda: clock[0])
    async def advance(event, seconds):
        assert not ph.store.thread(tid)['halted']
        assert read(ph.store, tid)['count'] == 0 and not read(ph.store, tid)['held']
        waits.append(seconds)
        clock[0] += seconds
        await asyncio.sleep(0)
    monkeypatch.setattr('cute_claude.queue_recovery.wait_backoff', advance)
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    client = ph.sdk.clients[0]
    async def query(prompt):
        from fake_sdk import materialize
        text, _ = await materialize(prompt)
        prompts.append(text)
        client.query_count += 1
        row = ph.store.db.execute("SELECT message_id FROM inputs WHERE thread_id=? AND state='processing'", (tid,)).fetchone()
        failed = clock[0] < 300
        if not failed:
            delivered.append(row[0])
        await client.events.put(failure(RESET) if failed else SUCCESS)
        await client.events.put({'type':'result','session_id':ph.engine.session(tid)['session_id'],
            'subtype':'error_during_execution' if failed else 'success','is_error':failed,'num_turns':1,'usage':{}})
    client.query = query
    jobs = [await submit(ph, tid, 'outage-' + str(n)) for n in range(5)]
    before = [tuple(row) for row in ph.store.db.execute('SELECT seq,message_id,semantic_hash,payload,turn_id FROM inputs ORDER BY seq')]
    await ph.drain()
    assert waits == [10.0, 30.0, 60.0, 200.0] and clock[0] == 300
    assert not ph.store.thread(tid)['halted']
    assert not [e for e in ph.out if e.get('method') == 'claude/queue/holdChanged']
    assert ph.store.db.execute("SELECT COUNT(*) FROM turns WHERE state='failed'").fetchone()[0] == 0
    assert delivered == ['outage-' + str(n) for n in range(5)]
    assert sum('do queued delegation outage-0' in prompt for prompt in prompts) == 1
    from cute_claude.transient_retry import RETRY_CONTINUATION
    assert prompts[1:5] == [RETRY_CONTINUATION] * 4
    assert [tuple(row) for row in ph.store.db.execute('SELECT seq,message_id,semantic_hash,payload,turn_id FROM inputs ORDER BY seq')] == before
    assert all(ph.store.turn(job['turnId'])['status'] == 'completed' for job in jobs)
    assert len([event for event in ph.out if event.get('method') == 'turn/started']) == 5
    assert len([event for event in ph.out if event.get('method') == 'turn/completed']) == 5


def test_retained_input_survives_adapter_restart_without_failed_row_or_new_identity(tmp_path):
    store = Store(tmp_path / 'state')
    tid = store.new_thread('model', str(tmp_path))
    payload = {'source':{'kind':'user','id':'owner'},'eventType':'message','text':'same instruction','metadata':{}}
    receipt = store.enqueue(tid, 'retained', payload, 10)
    record = store.claim(tid)
    recovery = plan(store, tid, SdkResponseError(failure(RESET), None), tool_in_flight=False, now=100)
    assert store.db.execute("SELECT value FROM meta WHERE key='schema'").fetchone()[0] == '2'
    store.defer_silent_input(tid, record['turn_id'], recovery)
    assert store.db.execute("SELECT value FROM meta WHERE key='schema'").fetchone()[0] == '3'
    store.close()
    store = Store(tmp_path / 'state')
    assert store.db.execute("SELECT value FROM meta WHERE key='schema'").fetchone()[0] == '3'
    assert store.input_status(tid, 'retained')['state'] == 'queued'
    assert store.turn(receipt['turnId'])['status'] == 'inProgress'
    assert store.recovered_events[tid] == [('claude/queue/holdChanged', {
        'threadId':tid,'turnId':record['turn_id'],'held':True,'reason':'silentTransportGuard'})]
    store.resume_queue(tid)
    again = store.claim(tid)
    assert again['message_id'] == 'retained' and again['turn_id'] == record['turn_id'] and again['queue_retry']
    assert store.db.execute('SELECT COUNT(*) FROM turns').fetchone()[0] == 1
    store.close()


@pytest.mark.parametrize('progress', ['text', 'completed-tool'])
async def test_output_or_completed_tool_uses_consumed_failure_guard(ph, monkeypatch, progress):
    configure(monkeypatch)
    async def already_progressed(tid, client):
        if progress == 'text':
            await client.events.put({'type':'assistant','content':[{'type':'text','text':'visible progress'}]})
        else:
            ph.store.db.execute("INSERT INTO tool_calls VALUES(?,?,?,'completed','{}','{}')",
                (tid, 'completed-effect', ph.engine.retry_turns[tid]))
    tid, client = await install_query(ph, [failure(RESET)], already_progressed)
    turn = await ph.turn(tid, 'one input made progress')
    await ph.drain()
    assert client.query_count == 1 and ph.store.turn(turn)['status'] == 'failed'
    assert read(ph.store, tid)['count'] == 1 and not ph.store.thread(tid)['halted']


@pytest.mark.parametrize('change', ['processed-arrival', 'metadata-append', 'owner-generation'])
async def test_terminal_empty_receipt_invalidates_after_new_processed_activity(ph, monkeypatch, change):
    configure(monkeypatch)
    monkeypatch.setattr('cute_claude.queue_recovery.BASE_DELAY',60)
    injected=[]
    tid, client = await install_query(ph, [failure(RESET), SUCCESS])
    async def changed_during_backoff(event, seconds):
        injected.append(client.query_count)
        owner = ph.engine.driver.owners[tid]
        if change == 'processed-arrival':
            owner.recovery_arrivals += 1
            owner.recovery_revision = owner.recovery_arrivals
        elif change == 'owner-generation':
            owner.native_connection_generation += 1
        else:
            from pathlib import Path
            history = Path(owner.driver.config_dir) / 'projects' / 'fixture'
            history.mkdir(parents=True, exist_ok=True)
            (history / (owner.session_id + '.jsonl')).write_text('{}\n')
        await asyncio.sleep(0)
    monkeypatch.setattr('cute_claude.queue_recovery.wait_backoff', changed_during_backoff)
    turn = await ph.turn(tid, 'original')
    await ph.drain()
    assert injected == [1], 'activity must actually be injected before the second query'
    if change == 'metadata-append':
        assert client.query_count == 2 and ph.store.turn(turn)['status'] == 'completed'
        assert not ph.store.thread(tid)['halted']
        return
    assert client.query_count == 1
    assert ph.store.turn(turn)['status'] == 'inProgress' and ph.store.thread(tid)['halted']
    assert ph.store.db.execute("SELECT state FROM inputs WHERE turn_id=?", (turn,)).fetchone()[0] == 'queued'


@pytest.mark.parametrize('effect', ['text', 'thinking', 'tool_use'])
async def test_native_history_activity_without_sdk_arrival_blocks_retry(ph, monkeypatch, effect):
    import uuid
    from pathlib import Path
    configure(monkeypatch)
    monkeypatch.setattr('cute_claude.queue_recovery.BASE_DELAY', 60)
    user, error = str(uuid.uuid4()), str(uuid.uuid4())
    rows, paths, injected = [], [], []
    async def write_original(tid, client):
        sid = ph.engine.session(tid)['session_id']
        rows.extend([
            {'type':'user','uuid':user,'sessionId':sid,'parentUuid':None,
             'message':{'content':[{'type':'text','text':'original'}]}},
            {'type':'assistant','uuid':error,'sessionId':sid,'parentUuid':user,
             'isApiErrorMessage':True,'message':{'content':[{'type':'text','text':RESET}]}}])
        path = Path(ph.engine.driver.config_dir)/'projects'/'fixture'/f'{sid}.jsonl'
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(''.join(json.dumps(row)+'\n' for row in rows))
        paths.append(path)
    tid, client = await install_query(ph, [failure(RESET, uuid=error), SUCCESS], write_original)
    async def changed_during_backoff(event, seconds):
        injected.append(client.query_count)
        owner = ph.engine.driver.owners[tid]
        arrival = owner.recovery_arrivals
        rows.append({'type':'assistant','uuid':str(uuid.uuid4()),'sessionId':owner.session_id,
                     'parentUuid':error,'message':{'content':[{'type':effect}]}})
        paths[0].write_text(''.join(json.dumps(row)+'\n' for row in rows))
        assert owner.recovery_arrivals == arrival, 'only native history changes'
        await asyncio.sleep(0)
    monkeypatch.setattr('cute_claude.queue_recovery.wait_backoff', changed_during_backoff)
    turn = await ph.turn(tid, 'original')
    await ph.drain()
    assert injected == [1] and client.query_count == 1
    assert ph.store.turn(turn)['status'] == 'inProgress' and ph.store.thread(tid)['halted']
    assert ph.store.db.execute('SELECT state FROM inputs WHERE turn_id=?', (turn,)).fetchone()[0] == 'queued'


async def test_probe_dispatch_is_durable_before_native_query_and_terminal_receipt_replaces_it(ph, monkeypatch):
    configure(monkeypatch)
    tid, client = await install_query(ph, [failure(RESET), SUCCESS])
    original = client.query
    dispatches = []
    async def query(prompt):
        turn = ph.engine.retry_turns[tid]
        row = ph.store.db.execute('SELECT value FROM meta WHERE key=?', ('claudeQueueRetry:' + turn,)).fetchone()
        if row:
            dispatches.append(json.loads(row[0])['dispatch'])
        await original(prompt)
    client.query = query
    turn = await ph.turn(tid, 'original')
    await ph.drain()
    assert dispatches == ['dispatching'] and client.query_count == 2
    assert ph.store.turn(turn)['status'] == 'completed'
    assert not ph.store.db.execute('SELECT 1 FROM meta WHERE key=?', ('claudeQueueRetry:' + turn,)).fetchone()


async def test_resume_now_breaks_backoff_without_a_runtime_hold(ph, monkeypatch):
    configure(monkeypatch)
    monkeypatch.setattr('cute_claude.queue_recovery.BASE_DELAY', 60)
    tid, client = await install_query(ph, [failure(RESET), SUCCESS])
    turn = await ph.turn(tid, 'original')
    await until(lambda: tid in ph.engine.queue_backoffs)
    assert not ph.store.thread(tid)['halted'] and remaining(ph.store, tid) > 0
    async with asyncio.timeout(1):
        await ph.call('claude/queue/resume', {'threadId': tid, 'acknowledgeUncertainEffects': True})
        await ph.drain()
    assert client.query_count == 2 and ph.store.turn(turn)['status'] == 'completed'


async def test_three_consumed_failures_still_hold_the_later_untouched_input(ph, monkeypatch):
    configure(monkeypatch)
    async def progress(tid, client):
        await client.events.put(SUCCESS)
    tid, client = await install_query(ph, [failure(RESET)], progress)
    await ph.turn(tid, 'first made progress')
    jobs = [await submit(ph, tid, str(n)) for n in range(3)]
    await ph.drain()
    assert client.query_count == 3 and ph.store.thread(tid)['halted']
    assert read(ph.store, tid)['count'] == 3 and read(ph.store, tid)['held']
    assert ph.store.input_status(tid, '2')['state'] == 'queued'


@pytest.mark.parametrize('finish_state',['completed','interrupted','failed'])
def test_schema_three_returns_to_two_after_last_retained_input_finishes(tmp_path,finish_state):
    store=Store(tmp_path/'state')
    tid=store.new_thread('model',str(tmp_path))
    payload={'source':{'kind':'user','id':'owner'},'eventType':'message','text':'task','metadata':{}}
    store.enqueue(tid,'retained',payload,10);record=store.claim(tid)
    store.defer_silent_input(tid,record['turn_id'],{'count':0,'probeCount':1,'held':False,'windowStarted':0,'notBefore':10})
    assert store.db.execute("SELECT value FROM meta WHERE key='schema'").fetchone()[0]=='3'
    store.finish(tid,record['turn_id'],finish_state)
    assert store.db.execute("SELECT value FROM meta WHERE key='schema'").fetchone()[0]=='2'
    store.close()


def test_open_lowers_empty_schema_three_but_keeps_other_retained_threads_fenced(tmp_path):
    store=Store(tmp_path/'state')
    turns=[]
    for n in range(2):
        tid=store.new_thread('model',str(tmp_path))
        store.enqueue(tid,str(n),{'source':{'kind':'user','id':'owner'},'eventType':'message','text':str(n),'metadata':{}},10)
        record=store.claim(tid);turns.append((tid,record['turn_id']))
        store.defer_silent_input(tid,record['turn_id'],{'count':0,'probeCount':1,'held':False,'windowStarted':0,'notBefore':10})
    store.finish(*turns[0],'completed')
    assert store.db.execute("SELECT value FROM meta WHERE key='schema'").fetchone()[0]=='3'
    store.finish(*turns[1],'completed')
    store.db.execute("UPDATE meta SET value='3' WHERE key='schema'")
    store.close();store=Store(tmp_path/'state')
    assert store.db.execute("SELECT value FROM meta WHERE key='schema'").fetchone()[0]=='2'
    store.close()


def test_continuous_outage_slows_after_one_hour_and_keeps_epoch_across_restart(tmp_path,monkeypatch):
    from cute_claude.queue_recovery import KEY
    monkeypatch.setattr('cute_claude.queue_recovery.BASE_DELAY',10)
    monkeypatch.setattr('cute_claude.queue_recovery.random.uniform',lambda lo,hi:(lo+hi)/2)
    store=Store(tmp_path/'state');tid=store.new_thread('model',str(tmp_path))
    err=SdkResponseError(failure(RESET),None)
    first=plan(store,tid,err,tool_in_flight=False,now=100,empty=True)
    store.db.execute('INSERT INTO meta VALUES(?,?)',(KEY+tid,json.dumps(first)))
    before=plan(store,tid,err,tool_in_flight=False,now=3699,empty=True)
    assert before['notBefore']-3699 == 30 and before['outageStarted']==100
    store.db.execute('UPDATE meta SET value=? WHERE key=?',(json.dumps(before),KEY+tid))
    store.close();store=Store(tmp_path/'state')
    after=plan(store,tid,err,tool_in_flight=False,now=3700,empty=True)
    assert after['notBefore']-3700 == 1350 and after['outageStarted']==100
    assert after['count']==0 and not after['held']
    store.close()
