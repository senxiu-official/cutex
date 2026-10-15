from __future__ import annotations

import asyncio
import inspect
import json
from dataclasses import replace
from datetime import date, datetime, timezone

import pytest

from cute_claude.config import Settings
from cute_claude.errors import RpcError
from cute_claude.external_input import ExternalInputService, make_receipt, semantic_digest
from cute_claude.inputs import parse_turn_input
from cute_claude.r8_services import read_usage
from cute_claude.skills import ManagedSkills
from cute_claude.store import dump


def envelope(tid='thread', mid='message', *, passive=False, version=1, view=None):
    value = {'version': version, 'ownerId': 'owner', 'threadId': tid, 'runtimeGeneration': 7,
             'message': {'id': mid, 'source': {'kind': 'service', 'id': 'service-id'},
                         'type': 'opaque.v1', 'delivery': 'passive' if passive else 'after_turn', 'text': 'hello\n世界'}}
    if view is not None:
        value['view'] = view
    value['semanticSha256'] = semantic_digest(value)
    return value


def install(h, *, subscription=False):
    settings = replace(h.settings, external_owner_id='owner', external_runtime_generation=7)
    if subscription:
        settings = replace(settings, backend='subscription')
    h.server.settings = settings
    service = ExternalInputService(h.server)
    h.server.external_inputs = service

    async def notify(tid, method, params):
        await service.observe_event(tid, method, params)
        await h.server.broadcast(tid, method, params)

    h.engine.notify = notify
    return service


async def invoke(service, h, action, params, *, after=True):
    result, callback = await service.dispatch(h.peer, 'thread/externalInput/' + action, params)
    if after and callback:
        value = callback()
        if inspect.isawaitable(value):
            await value
    return result


def binding(tid):
    return {'version': 1, 'ownerId': 'owner', 'threadId': tid, 'runtimeGeneration': 7}


def test_native_reference_digest_and_receipt_vectors():
    value = envelope()
    assert value['semanticSha256'] == '5b066f6337afec950bb316090397702df3c239477ac13768d8a1fd6bf86a400e'
    assert make_receipt(value, 'turn', 1)['receiptId'] == 'eir1_3692b2f248c7c1a6e14112bbf3e6d5c498ee0c737d098c3aed89e42ec7dd93fa'
    value['runtimeGeneration'] = 90
    assert semantic_digest(value) == value['semanticSha256']
    a = envelope(version=2, view={'schema': 'test.v1', 'data': {'z': [True, None, 7], 'a': '🦀\n'}})
    b = envelope(version=2, view={'data': {'a': '🦀\n', 'z': [True, None, 7]}, 'schema': 'test.v1'})
    assert a['semanticSha256'] == b['semanticSha256']
    assert a['semanticSha256'] == '871d2fc606fe6d2230e1c3cfc63c2f568f2105a214e6bf2668bf6ecfa2e0f05c'
    assert make_receipt(a, 'turn', 1)['receiptId'] == 'eir1_2b71c1110ad1352db5f49428c9c68da23f7356449519eccb961e3431d783eeb2'
    assert a['semanticSha256'] != envelope()['semanticSha256']


async def test_native_submit_receipt_status_notifications_and_conflict(h):
    service = install(h)
    tid = await h.thread()
    value = envelope(tid)
    result = await invoke(service, h, 'submit', value)
    assert result['statuses'][0]['deliveryState'] == 'pending'
    assert result['statuses'][0]['receipt'] is None
    await h.drain()
    keys = [{'messageId': 'message', 'semanticSha256': value['semanticSha256']},
            {'messageId': 'absent', 'semanticSha256': '0' * 64}]
    status = (await invoke(service, h, 'status', {**binding(tid), 'messages': keys}))['statuses']
    assert [item['deliveryState'] for item in status] == ['context_persisted', 'unknown']
    receipt = status[0]['receipt']
    assert receipt == make_receipt(value, receipt['turnId'], 1)
    assert status[0]['processing']['state'] == 'output_observed'
    assert any(item['id'] == 'message' and item['type'] == 'functionCallOutput' for item in h.store.items(receipt['turnId']))
    duplicate = await invoke(service, h, 'submit', value)
    assert duplicate['statuses'][0] == status[0]
    changed = json.loads(json.dumps(value))
    changed['message']['text'] = 'changed'
    changed['semanticSha256'] = semantic_digest(changed)
    assert (await invoke(service, h, 'submit', changed))['statuses'][0]['deliveryState'] == 'conflict'
    assert any(event.get('method') == 'thread/externalInput/statusChanged' for event in h.out)
    assert h.store.db.execute('SELECT count(*) FROM turns WHERE thread_id=?', (tid,)).fetchone()[0] == 1


async def test_native_requires_binding_and_validates_digest_and_view(h):
    tid = await h.thread()
    service = ExternalInputService(h.server)
    with pytest.raises(RpcError, match='trusted host'):
        await invoke(service, h, 'submit', envelope(tid))
    service = install(h)
    value = envelope(tid)
    with pytest.raises(RpcError, match='binding mismatch'):
        await invoke(service, h, 'submit', {**value, 'runtimeGeneration': 8})
    with pytest.raises(RpcError, match='digest mismatch'):
        await invoke(service, h, 'submit', {**value, 'semanticSha256': '0' * 64})
    value = envelope(tid, version=2, view={'schema': 'test.v1', 'data': {'x': 1.5}})
    with pytest.raises(RpcError, match='integers only'):
        await invoke(service, h, 'submit', value)
    assert not h.store.pending(tid)


async def test_native_passive_notes_do_not_start_turns_and_join_next_active(h):
    service = install(h)
    tid = await h.thread()
    passive = envelope(tid, 'note', passive=True)
    await invoke(service, h, 'submit', passive)
    assert not h.engine.workers
    assert h.store.pending_notes(tid) == 1
    await invoke(service, h, 'submit', envelope(tid, 'active'))
    await h.drain()
    key = {'messageId': 'note', 'semanticSha256': passive['semanticSha256']}
    status = (await invoke(service, h, 'status', {**binding(tid), 'messages': [key]}))['statuses'][0]
    assert status['deliveryState'] == 'context_persisted'
    assert status['processing'] == {'state': 'none', 'attemptId': None, 'reason': None}
    assert h.store.pending_notes(tid) == 0


async def test_native_exclusive_refuses_busy_and_retry_never_replays_claimed(h):
    service = install(h)
    tid = await h.thread()
    value = envelope(tid, 'queued')
    await invoke(service, h, 'submit', value, after=False)
    with pytest.raises(RpcError) as refusal:
        await invoke(service, h, 'submit', {**envelope(tid, 'refused'), 'admission': 'exclusive_turn'})
    assert refusal.value.data['externalInputError'] == 'busy'
    assert not h.store.input_known(tid, 'refused')
    retry = {**binding(tid), 'messageId': 'queued', 'semanticSha256': value['semanticSha256'],
             'expectedAttemptId': None, 'retryId': 'retry-1'}
    released = await invoke(service, h, 'retry', retry, after=False)
    assert released['disposition'] == 'released'
    assert await invoke(service, h, 'retry', retry) == released
    record = h.store.claim(tid)
    h.store.finish(tid, record['turn_id'], 'uncertain')
    await invoke(service, h, 'resume', binding(tid), after=False)
    with pytest.raises(RpcError, match='never replayed'):
        await invoke(service, h, 'retry', {**retry, 'retryId': 'retry-2'})
    assert h.store.input_status(tid, 'queued')['state'] == 'uncertain'
    assert h.store.claim(tid) is None


async def test_native_subscription_claim_does_not_fabricate_context_receipt(h):
    tid = await h.thread()
    service = install(h, subscription=True)
    value = envelope(tid)
    await invoke(service, h, 'submit', value, after=False)
    record = h.store.claim(tid)
    await service.observe_event(tid, 'turn/started', {'threadId': tid, 'turn': h.store.turn(record['turn_id'])})
    status = service._status(tid, 'message', value['semanticSha256'])
    assert status['deliveryState'] == 'pending'
    assert status['processing']['state'] == 'claimed'
    await service.observe_event(tid, 'claude/input/contextPersisted', {'threadId': tid, 'turnId': record['turn_id'], 'messageId': 'message'})
    assert service._status(tid, 'message', value['semanticSha256'])['deliveryState'] == 'context_persisted'
    h.store.finish(tid, record['turn_id'], 'completed')


async def test_usage_totals_caches_daily_streaks_and_unknown_prices(h):
    tid = await h.thread()
    for index, day in enumerate((7, 8, 10)):
        started = datetime(2026, 10, day, tzinfo=timezone.utc).timestamp()
        counts = {'input_tokens': 10, 'output_tokens': 5, 'cache_creation_input_tokens': 3, 'cache_read_input_tokens': 7}
        usage = {'modelRequests': [{'outcome': 'received', 'usage': counts}]}
        h.store.db.execute("INSERT INTO turns(id,thread_id,state,started,completed,usage) VALUES(?,?,'completed',?,?,?)",
                           (f'usage-{index}', tid, started, started + index + 1, dump(usage)))
    h.store.db.execute("INSERT INTO turns(id,thread_id,state,started,usage) VALUES('missing',?,'failed',?,'{}')", (tid, started))
    result = read_usage(h.store, {}, today=date(2026, 10, 10))
    assert result['summary'] == {'lifetimeTokens': 75, 'peakDailyTokens': 25, 'longestRunningTurnSec': 3,
                                  'currentStreakDays': 1, 'longestStreakDays': 2}
    assert len(result['dailyUsageBuckets']) == 3
    assert result['threadUsage'] is None
    assert read_usage(h.store, {'threadId': tid}, today=date(2026, 10, 10)) == result


def test_managed_skills_catalog_injection_and_path_boundaries(tmp_path):
    workspace = tmp_path / 'project'
    root = workspace / '.cutex' / 'skills'
    skill = root / 'review' / 'SKILL.md'
    skill.parent.mkdir(parents=True)
    skill.write_text('---\nname: review\ndescription: >\n  Review code carefully\n  with context.\n---\nCheck the diff.\n', encoding='utf-8')
    settings = Settings(workspace, tmp_path / 'state', managed_skills_roots=(root,))
    manager = ManagedSkills(settings)
    listed = manager.list({})['data'][0]['skills']
    assert listed[0]['description'] == 'Review code carefully with context.'
    assert listed[0]['scope'] == 'repo'
    item = {'type': 'skill', 'name': 'review', 'path': str(skill)}
    prompt, images, display = parse_turn_input([{'type': 'text', 'text': 'Review it'}, item, item], images=False, skills=manager, cwd=workspace)
    assert prompt.count('Check the diff.') == 1
    assert images == [] and display == [item]
    outside = tmp_path / 'private' / 'SKILL.md'
    outside.parent.mkdir()
    outside.write_text('private', encoding='utf-8')
    with pytest.raises(RpcError, match='managed catalog'):
        manager.resolve({**item, 'path': str(outside)})
    with pytest.raises(RpcError, match='outside'):
        manager.list({'cwds': [str(tmp_path)]})
    assert ManagedSkills(replace(settings, managed_skills_roots=())).list({})['data'][0]['skills'] == []


def test_managed_skills_rejects_oversized_injection(tmp_path):
    skill = tmp_path / 'skills' / 'review' / 'SKILL.md'
    skill.parent.mkdir(parents=True)
    skill.write_text('x' * (49 * 1024), encoding='utf-8')
    manager = ManagedSkills(Settings(tmp_path, tmp_path / 'state', managed_skills_roots=(skill.parent.parent,)))
    result = manager.list({})['data'][0]
    assert result['skills'] == [] and len(result['errors']) == 1
    with pytest.raises(RpcError, match='managed catalog'):
        manager.resolve({'type': 'skill', 'name': 'review', 'path': str(skill)})


async def test_native_rpc_end_to_end_and_binding_rotation(h):
    install(h)
    tid = await h.thread()
    value = envelope(tid)
    result = await h.call('thread/externalInput/submit', value)
    assert result['statuses'][0]['deliveryState'] == 'pending'
    await h.drain()
    rotated = await h.call('claude/externalInput/bind', {'ownerId': 'owner', 'runtimeGeneration': 8})
    assert rotated['runtimeGeneration'] == 8
    stale = await h.call('thread/externalInput/status', {**binding(tid), 'messages': [
        {'messageId': 'message', 'semanticSha256': value['semanticSha256']}]}, error=True)
    assert stale['code'] == -32011
    current = await h.call('thread/externalInput/status', {**binding(tid), 'runtimeGeneration': 8, 'messages': [
        {'messageId': 'message', 'semanticSha256': value['semanticSha256']}]})
    assert current['statuses'][0]['deliveryState'] == 'context_persisted'
    assert len(h.provider.requests) == 1
    backwards = await h.call('claude/externalInput/bind', {'ownerId': 'owner', 'runtimeGeneration': 7}, error=True)
    assert backwards['code'] == -32011
    usage = await h.call('account/usage/read', {'threadId': tid})
    assert usage['summary']['lifetimeTokens'] == 18


async def test_native_interrupt_keeps_gate_and_runs_only_untouched_queue(h):
    install(h)
    tid = await h.thread()
    entered = asyncio.Event()
    original = h.provider.sample

    async def blocking(*args, **kwargs):
        if not entered.is_set():
            entered.set()
            await asyncio.Event().wait()
        return await original(*args, **kwargs)

    h.provider.sample = blocking
    first = await h.call('thread/externalInput/submit', {**envelope(tid, 'first'), 'admission': 'exclusive_turn'})
    await asyncio.wait_for(entered.wait(), 3)
    await h.call('thread/externalInput/submit', envelope(tid, 'second'))
    assert await h.call('thread/externalInput/interrupt', {**binding(tid), 'turnId': first['turnId']}) == {}
    await h.drain()
    assert h.store.input_status(tid, 'first')['state'] == 'interrupted'
    assert h.store.input_status(tid, 'second')['state'] == 'completed'
    assert not h.store.thread(tid)['halted']
    assert len(h.provider.requests) == 1


async def test_managed_skills_rpc_turn_and_no_implicit_global_settings(h):
    root = h.settings.workspace / 'managed'
    skill = root / 'review' / 'SKILL.md'
    skill.parent.mkdir(parents=True)
    skill.write_text('---\nname: review\ndescription: review the code\n---\nCheck boundary conditions.', encoding='utf-8')
    h.server.skills = ManagedSkills(replace(h.settings, managed_skills_roots=(root,)))
    listed = await h.call('skills/list')
    assert listed['data'][0]['skills'][0]['name'] == 'review'
    tid = await h.thread()
    await h.call('turn/start', {'threadId': tid, 'input': [{'type': 'text', 'text': 'Review'},
                    {'type': 'skill', 'name': 'review', 'path': str(skill)}]})
    await h.drain()
    assert 'Check boundary conditions.' in h.provider.requests[0]['messages'][-1]['content'][0]['text']


async def test_native_recovery_does_not_ack_or_replay_interrupted_sdk_input(h):
    tid = await h.thread()
    service = install(h, subscription=True)
    value = envelope(tid, 'uncertain')
    await invoke(service, h, 'submit', value, after=False)
    h.store.claim(tid)
    # Simulate startup recovery between durable claim and a model acknowledgement.
    h.store.recover()
    recovered = ExternalInputService(h.server)
    status = recovered._status(tid, 'uncertain', value['semanticSha256'])
    assert status['receipt'] is None and status['deliveryState'] == 'pending'
    assert status['processing']['state'] == 'held'
    assert status['processing']['reason'] == 'request_uncertain'
    await invoke(recovered, h, 'resume', binding(tid), after=False)
    retry = {**binding(tid), 'messageId': 'uncertain', 'semanticSha256': value['semanticSha256'],
             'expectedAttemptId': status['processing']['attemptId'], 'retryId': 'recover'}
    with pytest.raises(RpcError, match='never replayed'):
        await invoke(recovered, h, 'retry', retry)
    assert h.store.claim(tid) is None and h.provider.requests == []


async def test_native_submit_ledger_and_inbox_are_one_transaction(h):
    service = install(h)
    tid = await h.thread()
    h.store.db.execute("CREATE TRIGGER reject_external BEFORE INSERT ON external_inputs BEGIN SELECT RAISE(ABORT, 'test rollback'); END")
    import sqlite3
    with pytest.raises(sqlite3.IntegrityError):
        await invoke(service, h, 'submit', envelope(tid), after=False)
    assert not h.store.input_known(tid, 'message')
    assert h.store.db.execute('SELECT count(*) FROM presentations WHERE thread_id=?', (tid,)).fetchone()[0] == 0


async def test_native_reverted_context_never_returns_stale_receipt(h):
    service = install(h)
    tid = await h.thread()
    value = envelope(tid)
    await invoke(service, h, 'submit', value)
    await h.drain()
    h.store.db.execute("UPDATE inputs SET state='reverted' WHERE thread_id=?", (tid,))
    status = service._status(tid, 'message', value['semanticSha256'])
    assert status['receipt'] is None and status['deliveryState'] == 'unknown'
    assert status['processing']['reason'] == 'context_missing'


async def test_native_view_stays_display_only_and_event_type_remains_exact(h):
    service = install(h)
    tid = await h.thread()
    value = envelope(tid, version=2, view={'schema': 'cutex.agent-message.v1',
                'data': {'senderName': 'Display Sender', 'privateViewData': 'view must stay out of model text'}})
    value['message']['type'] = 'agent_message'
    value['semanticSha256'] = semantic_digest(value)
    await invoke(service, h, 'submit', value)
    await h.drain()
    submitted = h.provider.requests[0]['messages'][-1]['content'][0]['text']
    assert 'privateViewData' not in submitted and 'Display Sender' not in submitted
    turn_id = h.store.input_status(tid, 'message')['turnId']
    item = next(item for item in h.store.items(turn_id) if item['id'] == 'message')
    assert json.loads(item['output'])['type'] == 'agent_message'
