import asyncio
from dataclasses import replace
import json
from pathlib import Path
from types import SimpleNamespace
import uuid

import pytest

from cute_claude import checkpoint_recovery as recovery
from cute_claude.subscription import CALL_FIELD
from cute_claude.tools import ToolResult
from test_persistent import ph, until


def uid():
    return str(uuid.uuid4())


async def scenario(ph, monkeypatch, *, fault=None, parallel=False, errors=1):
    monkeypatch.setattr(recovery, 'delay', lambda n: 0)
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    owner = ph.engine.driver.owners[tid]
    client = ph.sdk.clients[0]
    sid = owner.session_id
    folder = ph.engine.driver.config_dir / 'projects' / 'fixture'
    folder.mkdir(parents=True, exist_ok=True)
    transcript = folder / f'{sid}.jsonl'
    entries, queries, effects = [], [], []
    state = {}

    def append(typ, content, *, native_uuid=None, **extra):
        row = {'type': typ, 'uuid': native_uuid or uid(), 'sessionId': sid,
               'parentUuid': entries[-1]['uuid'] if entries else None,
               'message': {'content': content}, **extra}
        entries.append(row)
        transcript.write_text(''.join(json.dumps(x) + '\n' for x in entries), encoding='utf-8')
        return row['uuid']

    async def tool(args, ctx):
        effects.append(ctx.call_id)
        return ToolResult('COUNTER-RESULT')
    ph.tools.register('counter', 'fixture', {'type': 'object', 'properties': {}}, tool)
    original_execute = ph.tools.execute
    if fault == 'effect-before-journal':
        async def gap(*args, **kwargs):
            await original_execute(*args, **kwargs)
            raise RuntimeError('injected effect-to-journal crash')
        monkeypatch.setattr(ph.tools, 'execute', gap)

    async def query(prompt):
        client.query_count += 1
        messages = [x async for x in prompt] if hasattr(prompt, '__aiter__') else [prompt]
        queries.append(messages)
        if len(queries) == 1:
            append('user', [{'type': 'text', 'text': 'ORIGINAL'}])
            ids = ['fixture-a', 'fixture-b'] if parallel else ['fixture-a']
            uses = [{'type': 'tool_use', 'id': cid, 'name': 'mcp__cutex__counter', 'input': {'arguments': {}}} for cid in ids]
            if fault == 'empty-success':
                uses.insert(0, {'type': 'text', 'text': 'EARLIER COMMENTARY'})
            use_uuid = append('assistant', uses)
            await client.events.put({'type': 'assistant', 'uuid': use_uuid, 'content': uses})
            async def execute(cid):
                grant = await owner.bridge.pre_tool({'session_id': sid, 'tool_name': 'mcp__cutex__counter',
                    'tool_input': {'arguments': {}}}, cid, {})
                if fault == 'before-effect':
                    return
                try:
                    await owner.bridge.execute('counter', grant['hookSpecificOutput']['updatedInput'])
                except RuntimeError:
                    pass
            await asyncio.gather(*(execute(cid) for cid in ids))
            if fault not in {'before-effect', 'effect-before-journal', 'journal-before-sdk'}:
                blocks = [{'type': 'tool_result', 'tool_use_id': cid, 'content': 'COUNTER-RESULT',
                           'is_error': fault == 'error-result'} for cid in ids]
                result_uuid = uid()
                if fault != 'sdk-before-native':
                    append('user', blocks, native_uuid=result_uuid)
                await client.events.put({'type': 'user', 'uuid': result_uuid, 'content': blocks})
            if fault in {'partial', 'partial-duplicate', 'partial-thinking', 'partial-redacted'}:
                await client.events.put({'type': 'stream_event', 'event': {'type': 'content_block_start',
                    'index': 0, 'content_block': {'type': {'partial-thinking': 'thinking',
                        'partial-redacted': 'redacted_thinking'}.get(fault, 'text'), 'text': ''}}})
                if fault == 'partial-duplicate':
                    await client.events.put({'type': 'user', 'uuid': result_uuid, 'content': blocks})
            if fault == 'hook':
                await client.events.put({'type': 'system', 'subtype': 'hook_started'})
            if fault == 'finished-hook':
                await client.events.put({'type': 'system', 'subtype': 'hook_started'})
                await client.events.put({'type': 'system', 'subtype': 'hook_response'})
            if fault in {'subagent', 'background-shell'}:
                owner.tasks.rows['active'] = {'status': 'running', 'taskType': 'local_agent' if fault == 'subagent' else 'local_bash'}
            if fault == 'approval':
                ph.engine.permissions.pending['fixture'] = SimpleNamespace(thread_id=tid, future=asyncio.get_running_loop().create_future())
        failing = len(queries) <= errors
        if failing:
            error_uuid = append('assistant', [{'type': 'text', 'text': 'API Error: Connection dropped (ECONNRESET).'}],
                                isApiErrorMessage=True, error='server_error')
            await client.events.put({'type': 'assistant', 'uuid': error_uuid, 'error': 'server_error',
                                    'content': [{'type': 'text', 'text': 'API Error: Connection dropped (ECONNRESET).'}]})
        elif fault != 'empty-success':
            await client.events.put({'type': 'assistant', 'uuid': uid(), 'content': [{'type': 'text', 'text': 'DONE'}]})
        await client.events.put({'type': 'result', 'session_id': sid, 'subtype': 'error_during_execution' if failing else 'success',
                                'is_error': failing, 'num_turns': 2 if len(queries) == 1 else 1,
                                'usage': {'input_tokens': 10, 'output_tokens': 3 if len(queries) == 1 else 1}})
    client.query = query
    return tid, client, queries, effects, transcript


@pytest.mark.parametrize('parallel', [False, True])
async def test_completed_tools_continue_once_with_same_owner_and_durable_note(ph, monkeypatch, parallel):
    tid, client, queries, effects, _ = await scenario(ph, monkeypatch, parallel=parallel)
    before = ph.engine.runtime_status(tid)
    turn = await ph.turn(tid, 'ORIGINAL')
    await ph.drain()
    assert ph.store.turn(turn)['status'] == 'completed'
    assert len(effects) == (2 if parallel else 1) and len(queries) == 2
    assert recovery.CONTINUATION in json.dumps(queries[1]) and 'ORIGINAL' not in json.dumps(queries[1])
    after = ph.engine.runtime_status(tid)
    assert (after['processId'], after['sessionId'], after['connectionId']) == (before['processId'], before['sessionId'], before['connectionId'])
    notes = [e for e in ph.out if e.get('method') == 'thread/presentation/appended' and '已自动继续' in json.dumps(e, ensure_ascii=False)]
    assert len(notes) == 1 and notes[0]['params']['item']['presentation']['body'] == '连接中断，已自动继续 (1/2)'
    assert any(e['type'] == 'presentation' and e['item'] == notes[0]['params']['item'] for e in ph.store.timeline(tid, None, 200)['data'])
    assert ph.store.db.execute('SELECT count(*) FROM inputs WHERE turn_id=?', (turn,)).fetchone()[0] == 1
    assert len([e for e in ph.out if e.get('method') == 'turn/completed']) == 1
    usage = json.loads(ph.store.db.execute('SELECT usage FROM turns WHERE id=?', (turn,)).fetchone()[0])
    assert len(usage['modelRequests']) == 2
    assert [x['outcome'] for x in usage['modelRequests']] == ['received', 'received']
    assert client.disconnect_count == 0


@pytest.mark.parametrize('fault', ['before-effect', 'effect-before-journal', 'journal-before-sdk', 'sdk-before-native',
                                  'error-result', 'partial', 'partial-duplicate', 'partial-thinking', 'partial-redacted',
                                  'hook', 'finished-hook', 'subagent', 'background-shell', 'approval'])
async def test_uncertain_or_unsettled_boundary_never_continues(ph, monkeypatch, fault):
    tid, client, queries, effects, _ = await scenario(ph, monkeypatch, fault=fault)
    turn = await ph.turn(tid, 'ORIGINAL')
    await ph.drain()
    assert len(queries) == 1 and ph.store.turn(turn)['status'] == 'failed'
    assert len(effects) == (0 if fault == 'before-effect' else 1)
    assert not any('已自动继续' in json.dumps(e, ensure_ascii=False) for e in ph.out)
    if fault in {'before-effect', 'effect-before-journal', 'journal-before-sdk', 'hook', 'approval', 'subagent', 'background-shell'}:
        assert ph.store.thread(tid)['halted'], 'real effects uncertainty must also block distinct queued-work recovery'
    ph.engine.permissions.pending.clear()
    ph.engine.driver.owners[tid].tasks.rows.clear()


async def test_exhaustion_has_two_attempts_then_enters_r49_queue_policy(ph, monkeypatch):
    tid, client, queries, effects, _ = await scenario(ph, monkeypatch, errors=100)
    turn = await ph.turn(tid, 'ORIGINAL')
    await ph.drain()
    assert len(queries) == 3 and len(effects) == 1
    assert ph.store.turn(turn)['status'] == 'failed' and not ph.store.thread(tid)['halted']
    assert recovery.attempts(ph.store, turn) == 2
    assert sum('已自动继续' in json.dumps(e, ensure_ascii=False) for e in ph.out) == 2


async def test_owner_stop_during_wait_never_submits_or_claims_continuation(ph, monkeypatch):
    tid, client, queries, effects, _ = await scenario(ph, monkeypatch)
    monkeypatch.setattr(recovery, 'delay', lambda n: 60)
    turn = await ph.turn(tid, 'ORIGINAL')
    await until(lambda: turn in ph.engine.retry_waiting)
    await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
    assert len(queries) == 1 and ph.store.turn(turn)['status'] == 'interrupted'
    assert not any('已自动继续' in json.dumps(e, ensure_ascii=False) for e in ph.out)


async def test_checkpoint_changed_during_backoff_never_continues(ph, monkeypatch):
    tid, client, queries, effects, _ = await scenario(ph, monkeypatch)
    monkeypatch.setattr(recovery, 'delay', lambda n: .05)
    turn = await ph.turn(tid, 'ORIGINAL')
    await until(lambda: turn in ph.engine.retry_waiting)
    await client.events.put({'type': 'stream_event', 'event': {'type': 'message_start'}})
    await ph.drain()
    assert len(queries) == 1 and ph.store.turn(turn)['status'] == 'failed'


async def test_configured_hooks_veto_even_clean_tool_checkpoint(ph, monkeypatch):
    tid, client, queries, effects, _ = await scenario(ph, monkeypatch)
    monkeypatch.setattr(ph.engine, '_loaded_settings_hooks', lambda tid: True)
    turn = await ph.turn(tid, 'ORIGINAL')
    await ph.drain()
    assert len(queries) == 1


def test_budget_survives_recreated_checkpoint_and_backoff_bounds(ph, monkeypatch):
    # Covered asynchronously below: fixture setup itself owns an event loop.
    monkeypatch.setattr(recovery.random, 'uniform', lambda lo, hi: hi)
    assert recovery.delay(1) == 2.5 and recovery.delay(2) == 6.25


async def test_restart_never_replays_reserved_original_turn(ph, monkeypatch):
    tid, client, queries, effects, _ = await scenario(ph, monkeypatch)
    monkeypatch.setattr(recovery, 'delay', lambda n: 60)
    turn = await ph.turn(tid, 'ORIGINAL')
    await until(lambda: turn in ph.engine.retry_waiting)
    assert recovery.attempts(ph.store, turn) == 1
    restored = recovery.Checkpoint(ph.engine, tid, turn, ph.engine.session(tid)['session_id'])
    assert restored.unsafe and not await restored.verified(ph.engine.driver.owners[tid])
    await ph.engine.close()
    assert len(queries) == 1


@pytest.mark.parametrize('damage', ['truncated', 'malformed', 'wrong-session', 'result-missing', 'result-changed',
                                    'wrong-parent', 'sidechain', 'wrong-tip', 'cycle', 'oversized'])
async def test_persisted_proof_rejects_damaged_or_unrelated_history(ph, monkeypatch, damage):
    tid, client, queries, effects, transcript = await scenario(ph, monkeypatch)
    monkeypatch.setattr(recovery, 'delay', lambda n: 60)
    turn = await ph.turn(tid, 'ORIGINAL')
    await until(lambda: turn in ph.engine.retry_waiting)
    checkpoint = ph.engine.checkpoints[turn]
    entries = [json.loads(line) for line in transcript.read_text().splitlines()]
    if damage == 'truncated':
        transcript.write_text(transcript.read_text()[:-2])
    elif damage == 'malformed':
        transcript.write_text('{broken}\n')
    elif damage == 'oversized':
        monkeypatch.setattr(recovery, 'MAX_TRANSCRIPT_BYTES', 64)
    else:
        if damage == 'wrong-session':
            entries[2]['sessionId'] = uid()
        elif damage == 'result-missing':
            entries.pop(2)
        elif damage == 'result-changed':
            entries[2]['message']['content'][0]['content'] = 'different effect receipt'
        elif damage == 'wrong-parent':
            entries[2]['parentUuid'] = uid()
        elif damage == 'sidechain':
            entries[2]['isSidechain'] = True
        elif damage == 'wrong-tip':
            entries.append({**entries[-1], 'uuid': uid(), 'parentUuid': entries[-1]['uuid']})
        elif damage == 'cycle':
            entries[0]['parentUuid'] = entries[-1]['uuid']
        transcript.write_text(''.join(json.dumps(x) + '\n' for x in entries))
    assert not await checkpoint.verified(ph.engine.driver.owners[tid])
    await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
    assert len(queries) == 1 and len(effects) == 1


async def test_short_native_flush_delay_is_observed_without_reexecuting_tools(ph, monkeypatch):
    tid, client, queries, effects, _ = await scenario(ph, monkeypatch)
    original = recovery.Checkpoint.persisted
    reads = []
    def behind(self, config):
        reads.append(True)
        return len(reads) > 1 and original(self, config)
    monkeypatch.setattr(recovery.Checkpoint, 'persisted', behind)
    turn = await ph.turn(tid, 'ORIGINAL')
    await ph.drain()
    assert len(reads) >= 3 and len(queries) == 2 and len(effects) == 1
    assert ph.store.turn(turn)['status'] == 'completed'


async def test_empty_transport_probe_leaves_checkpoint_budget_available(ph, monkeypatch):
    tid, client, queries, effects, _ = await scenario(ph, monkeypatch)
    original = client.query
    monkeypatch.setattr('cute_claude.transient_retry.retry_delay', lambda error, n: 0)
    intercepted = []
    async def first_silent(prompt):
        if not intercepted:
            intercepted.append(True)
            client.query_count += 1
            await client.events.put({'type': 'assistant', 'error': 'server_error', 'content': [
                {'type': 'text', 'text': 'API Error: Connection dropped (ECONNRESET).'}]})
            await client.events.put({'type': 'result', 'session_id': ph.engine.session(tid)['session_id'],
                'subtype': 'error_during_execution', 'is_error': True, 'num_turns': 1, 'usage': {}})
        else:
            await original(prompt)
    client.query = first_silent
    turn = await ph.turn(tid, 'ORIGINAL')
    await ph.drain()
    assert client.query_count == 3 and len(effects) == 1
    assert ph.store.turn(turn)['status'] == 'completed'
    rows = ph.store.db.execute('SELECT ordinal,reason FROM sdk_recovery_attempts WHERE turn_id=? ORDER BY ordinal', (turn,)).fetchall()
    assert [tuple(row) for row in rows] == [(1, 'checkpoint-connection')]


@pytest.mark.parametrize('close', ['runtime', 'engine'])
async def test_close_during_checkpoint_wait_does_not_query(ph, monkeypatch, close):
    tid, client, queries, effects, _ = await scenario(ph, monkeypatch)
    monkeypatch.setattr(recovery, 'delay', lambda n: 60)
    turn = await ph.turn(tid, 'ORIGINAL')
    await until(lambda: turn in ph.engine.retry_waiting)
    async with asyncio.timeout(2):
        await ph.engine.close_runtime(tid) if close == 'runtime' else await ph.engine.close()
    assert len(queries) == 1 and ph.store.turn(turn)['status'] == 'interrupted'


async def test_stop_immediately_before_query_has_no_false_submitted_note(ph, monkeypatch):
    tid, client, queries, effects, _ = await scenario(ph, monkeypatch)
    driver = ph.engine.driver
    original = driver.run
    async def stopped(**kwargs):
        checkpoint = ph.engine.checkpoints[kwargs['turn_id']]
        if checkpoint.pending_note:
            driver.owners[tid].cancelled_turns.add(kwargs['turn_id'])
        await original(**kwargs)
    monkeypatch.setattr(driver, 'run', stopped)
    turn = await ph.turn(tid, 'ORIGINAL')
    await ph.drain()
    assert len(queries) == 1 and ph.store.turn(turn)['status'] == 'interrupted'
    assert not any('已自动继续' in json.dumps(x, ensure_ascii=False) for x in ph.out)


async def test_exhausted_failed_turn_does_not_replay_when_next_queued_input_runs(ph, monkeypatch):
    tid, client, queries, effects, _ = await scenario(ph, monkeypatch, errors=3)
    monkeypatch.setattr('cute_claude.queue_recovery.BASE_DELAY', 0)
    first = await ph.turn(tid, 'ORIGINAL')
    from test_queue_recovery import submit
    second = (await submit(ph, tid, 'second-distinct'))['turnId']
    await ph.drain()
    assert ph.store.turn(first)['status'] == 'failed'
    assert ph.store.turn(second)['status'] == 'completed'
    assert len(queries) == 4 and len(effects) == 1
    assert recovery.attempts(ph.store, first) == 2 and recovery.attempts(ph.store, second) == 0


async def test_empty_recovery_success_never_promotes_earlier_commentary(ph, monkeypatch):
    tid, client, queries, effects, _ = await scenario(ph, monkeypatch, fault='empty-success')
    turn = await ph.turn(tid, 'ORIGINAL')
    await ph.drain()
    assert ph.store.turn(turn)['status'] == 'completed' and len(queries) == 2
    messages = [x for x in ph.store.items(turn) if x['type'] == 'agentMessage']
    assert len(messages) == 1 and messages[0]['phase'] == 'commentary'


async def test_pause_during_backoff_never_submits_continuation(ph, monkeypatch):
    tid, client, queries, effects, _ = await scenario(ph, monkeypatch)
    monkeypatch.setattr(recovery, 'delay', lambda n: .05)
    turn = await ph.turn(tid, 'ORIGINAL')
    await until(lambda: turn in ph.engine.retry_waiting)
    await ph.engine.tool_gate.set(tid, True)
    await ph.drain()
    assert len(queries) == 1


async def test_stop_during_working_notification_is_checked_before_stdin_write(ph, monkeypatch):
    tid, client, queries, effects, _ = await scenario(ph, monkeypatch)
    notify = ph.engine.notify
    async def stop(tid, method, params):
        checkpoint = next(iter(ph.engine.checkpoints.values()), None)
        if (method == 'claude/runtime/statusChanged' and checkpoint and checkpoint.pending_note
                and params.get('state') == 'working'):
            ph.engine.driver.owners[tid].cancelled_turns.add(checkpoint.turn_id)
        await notify(tid, method, params)
    monkeypatch.setattr(ph.engine, 'notify', stop)
    turn = await ph.turn(tid, 'ORIGINAL')
    await ph.drain()
    assert len(queries) == 1 and ph.store.turn(turn)['status'] == 'interrupted'
