"""External input hot-path and identity-preserving retention regressions."""
import json
import time
from types import SimpleNamespace

import pytest

from cute_claude.errors import RpcError
from cute_claude.config import Settings
from cute_claude.external_input import ExternalInputService, semantic_digest
from cute_claude.store import Store
from test_r8_services import binding, envelope, install, invoke


async def test_stream_deltas_and_nonoutput_items_do_not_query_receipt_history(h):
    tid = await h.thread()
    service = install(h, subscription=True)
    value = envelope(tid)
    await invoke(service, h, 'submit', value, after=False)
    turn = h.store.claim(tid)['turn_id']
    statements = []
    h.store.db.set_trace_callback(statements.append)
    try:
        for _ in range(1000):
            await service.observe_event(tid, 'item/agentMessage/delta', {'turnId': turn, 'delta': 'chunk'})
        await service.observe_event(tid, 'item/started', {'turnId': turn, 'item': {'type': 'agentMessage'}})
        await service.observe_event(tid, 'item/completed', {'turnId': turn, 'item': {'type': 'userMessage'}})
        assert statements == []
        await service.observe_event(tid, 'item/completed', {'turnId': turn, 'item': {'type': 'agentMessage'}})
    finally:
        h.store.db.set_trace_callback(None)
    status = service._status(tid, 'message', value['semanticSha256'])
    assert status['deliveryState'] == 'pending'  # output is not an SDK context acknowledgement
    assert status['processing']['state'] == 'output_observed'
    query = next(sql for sql in statements if sql.startswith('SELECT message_id FROM external_inputs'))
    plan = ' '.join(row[3] for row in h.store.db.execute('EXPLAIN QUERY PLAN ' + query))
    assert 'SEARCH external_inputs USING' in plan and 'SCAN external_inputs' not in plan
    assert 'external_notes_turn' in plan


async def finish_external(h, service, value, *, receipt=True, completed=None):
    tid = value['threadId']
    await invoke(service, h, 'submit', value, after=False)
    record = h.store.claim(tid)
    if receipt:
        service._persisted(tid, value['message']['id'], record['turn_id'])
    h.store.finish(tid, record['turn_id'], 'completed')
    if completed is not None:
        h.store.db.execute('UPDATE turns SET completed=? WHERE id=?', (completed, record['turn_id']))
    return record['turn_id']


async def test_retention_keeps_permanent_receipt_conflict_and_retry_identity(h):
    tid = await h.thread()
    service = install(h, subscription=True)
    value = envelope(tid, version=2, view={'schema': 'large.v1', 'data': {'redundant': 'x' * 8192}})
    # A durable retry receipt must survive even once the input has finished.
    await invoke(service, h, 'submit', value, after=False)
    retry = {**binding(tid), 'messageId': 'message', 'semanticSha256': value['semanticSha256'],
             'expectedAttemptId': None, 'retryId': 'retry-old'}
    released = await invoke(service, h, 'retry', retry, after=False)
    turn = h.store.claim(tid)['turn_id']
    service._persisted(tid, 'message', turn)
    h.store.finish(tid, turn, 'completed')
    now = time.time()
    h.store.db.execute('UPDATE turns SET completed=? WHERE id=?', (now - 31 * 86400, turn))
    before = service._status(tid, 'message', value['semanticSha256'])
    assert service.compact_history(now=now) == 1
    row = service._row(tid, 'message')
    assert row['compacted'] == 1 and len(row['envelope']) < 512
    assert 'redundant' not in row['envelope'] and 'text' not in json.loads(row['envelope'])['message']
    assert service._status(tid, 'message', value['semanticSha256']) == before
    assert (await invoke(service, h, 'submit', value))['statuses'][0] == before
    assert await invoke(service, h, 'retry', retry, after=False) == released
    changed = json.loads(json.dumps(value))
    changed['message']['text'] = 'different meaning'
    changed['semanticSha256'] = semantic_digest(changed)
    assert (await invoke(service, h, 'submit', changed))['statuses'][0]['deliveryState'] == 'conflict'
    with pytest.raises(RpcError, match='never replayed'):
        await invoke(service, h, 'retry', {**retry, 'retryId': 'new-retry'}, after=False)
    assert service.compact_history(now=now) == 0
    assert h.store.db.execute('SELECT count(*) FROM inputs').fetchone()[0] == 1
    assert h.store.claim(tid) is None


async def test_retention_is_bounded_preserves_recent_and_queued_and_can_acknowledge_tombstone(h):
    tid = await h.thread()
    service = install(h, subscription=True)
    now = time.time()
    old = envelope(tid, 'old')
    turn = await finish_external(h, service, old, receipt=False, completed=now - 31 * 86400)
    await finish_external(h, service, envelope(tid, 'recent'), completed=now - 29 * 86400)
    await invoke(service, h, 'submit', envelope(tid, 'queued'), after=False)
    assert service.compact_history(now=now) == 1
    assert service._row(tid, 'recent')['compacted'] == 0
    assert service._row(tid, 'queued')['compacted'] == 0
    service._persisted(tid, 'old', turn)
    assert service._status(tid, 'old', old['semanticSha256'])['deliveryState'] == 'context_persisted'
    # A fresh service migrates/reopens tombstones without reconstructing or replaying them.
    reopened = ExternalInputService(h.server)
    assert reopened._status(tid, 'old', old['semanticSha256']) == service._status(tid, 'old', old['semanticSha256'])


async def test_retention_batch_limit_and_service_migration(h):
    tid = await h.thread()
    service = install(h, subscription=True)
    now = time.time()
    for mid in ('old-one', 'old-two'):
        await finish_external(h, service, envelope(tid, mid), completed=now - 31 * 86400)
    service.MAINTENANCE_BATCH = 1
    assert service.compact_history(now=now) == 1
    assert service.compact_history(now=now) == 1
    h.store.db.execute('DROP INDEX external_inputs_uncompacted')
    h.store.db.execute('ALTER TABLE external_inputs DROP COLUMN compacted')
    migrated = ExternalInputService(h.server)
    assert migrated._status(tid, 'old-one', envelope(tid, 'old-one')['semanticSha256'])['receipt'] is not None
    assert h.store.db.execute('SELECT count(*) FROM external_inputs WHERE compacted=1').fetchone()[0] == 2


async def test_tombstone_survives_writer_restart_with_delayed_ack_and_permanent_retry(tmp_path):
    settings = Settings(tmp_path, tmp_path / 'state', backend='subscription',
                        external_owner_id='owner', external_runtime_generation=7)
    peer = SimpleNamespace(subscriptions=set())
    store = Store(settings.state_dir)
    try:
        service = ExternalInputService(SimpleNamespace(store=store, settings=settings))
        tid = store.new_thread('test', str(tmp_path))
        value = envelope(tid)
        await service._submit(peer, tid, value)
        retry = {**binding(tid), 'messageId': 'message', 'semanticSha256': value['semanticSha256'],
                 'expectedAttemptId': None, 'retryId': 'before-restart'}
        released, _ = service._retry(tid, retry)
        turn = store.claim(tid)['turn_id']
        store.finish(tid, turn, 'completed')
        store.db.execute('UPDATE turns SET completed=? WHERE id=?', (time.time() - 31 * 86400, turn))
        assert service.compact_history() == 1
        assert service._row(tid, 'message')['receipt'] is None
    finally:
        store.close()
    reopened = Store(settings.state_dir)
    try:
        restored = ExternalInputService(SimpleNamespace(store=reopened, settings=settings))
        restored._persisted(tid, 'message', turn)
        status = restored._status(tid, 'message', value['semanticSha256'])
        assert status['deliveryState'] == 'context_persisted'
        assert status['receipt']['semanticSha256'] == value['semanticSha256']
        assert restored._retry(tid, retry) == (released, None)
        duplicate, callback = await restored._submit(peer, tid, value)
        assert duplicate['statuses'] == [status] and callback is None
        changed = json.loads(json.dumps(value))
        changed['message']['text'] = 'different content after restart'
        changed['semanticSha256'] = semantic_digest(changed)
        conflict, callback = await restored._submit(peer, tid, changed)
        assert conflict['statuses'][0]['deliveryState'] == 'conflict' and callback is None
        assert reopened.claim(tid) is None
        assert reopened.db.execute('SELECT count(*) FROM turns').fetchone()[0] == 1
    finally:
        reopened.close()
