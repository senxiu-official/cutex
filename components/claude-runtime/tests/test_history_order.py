"""Visible-item read model: canonical SQL mutations, compatibility and work bounds."""
import json
import sqlite3

import pytest

from cute_claude import history_order
from cute_claude.history import History
from cute_claude.history_reader import Reader
from cute_claude.store import Store, dump


@pytest.fixture
def store(tmp_path):
    value = Store(tmp_path / 'state')
    yield value
    value.close()


def turn(store, tid, name, *, reverted=0):
    store.db.execute('INSERT INTO turns(id,thread_id,state,started,completed,reverted) VALUES(?,?,\'completed\',1,2,?)',
                     (name, tid, reverted))
    return name


def projection_matches(db):
    actual = [tuple(r) for r in db.execute('SELECT thread_id,seq FROM history_item_order ORDER BY thread_id,seq')]
    expected = [tuple(r) for r in db.execute('''SELECT t.thread_id,i.seq FROM turns t JOIN items i ON i.turn_id=t.id
        WHERE t.reverted=0 ORDER BY t.thread_id,i.seq''')]
    assert actual == expected


def remove_model(db, *, marker=True):
    for suffix in history_order.TRIGGERS:
        db.execute('DROP TRIGGER history_order_' + suffix)
    db.execute('DROP TABLE history_item_order')
    if marker:
        db.execute('DELETE FROM meta WHERE key=?', (history_order.MARKER,))


def test_sql_mutations_maintain_visibility_and_do_not_copy_payloads(store, tmp_path):
    a, b = [store.new_thread('mock', str(tmp_path)) for _ in range(2)]
    ta, tb, hidden = turn(store, a, 'a'), turn(store, b, 'b'), turn(store, a, 'hidden', reverted=1)
    # The real busy-delivery move/external deletion paths write canonical SQL.
    # Plain triggers must also work for a prior component with FK enforcement off.
    older = sqlite3.connect(store.path, isolation_level=None)
    try:
        assert older.execute('PRAGMA foreign_keys').fetchone()[0] == 0
        for n, t in enumerate((ta, tb, ta, hidden)):
            older.execute('INSERT INTO items(turn_id,id,payload) VALUES(?,?,?)', (t, str(n), dump({'id':str(n)})))
        projection_matches(store.db)
        before = older.total_changes
        older.execute('UPDATE items SET payload=? WHERE id=\'0\'', (dump({'id':'0','text':'changed'}),))
        assert older.total_changes == before + 1  # No read-model rewrite for payload updates.
        for destination in (hidden, tb, ta):
            older.execute('UPDATE items SET turn_id=? WHERE id=\'0\'', (destination,))
            projection_matches(store.db)
        older.execute('UPDATE items SET seq=100 WHERE id=\'0\'')
        projection_matches(store.db)
        older.execute('UPDATE turns SET thread_id=? WHERE id=?', (b, ta))
        projection_matches(store.db)
        for reverted in (1, 1, 0):
            older.execute('UPDATE turns SET reverted=? WHERE id=?', (reverted, ta))
            projection_matches(store.db)
        older.execute('DELETE FROM items WHERE id=\'0\'')
        projection_matches(store.db)
        # With foreign keys off, canonical orphan items are invisible as before.
        older.execute('DELETE FROM turns WHERE id=?', (ta,))
        projection_matches(store.db)
    finally:
        older.close()
    assert [r[1] for r in store.db.execute('PRAGMA table_info(history_item_order)')] == ['thread_id', 'seq']


def test_fk_cascade_seq_move_delete_and_transaction_rollback(store, tmp_path):
    tid = store.new_thread('mock', str(tmp_path))
    turn(store, tid, 't')
    store.put_item('t', {'id':'i'})
    store.db.execute('UPDATE items SET seq=99 WHERE id=\'i\'')
    projection_matches(store.db)
    with pytest.raises(RuntimeError):
        with store.transaction():
            store.db.execute('UPDATE turns SET reverted=1 WHERE id=\'t\'')
            assert store.db.execute('SELECT count(*) FROM history_item_order').fetchone()[0] == 0
            raise RuntimeError('abort visibility change')
    projection_matches(store.db)
    store.db.execute('DELETE FROM items')
    projection_matches(store.db)
    assert store.db.execute('PRAGMA foreign_key_check').fetchall() == []


@pytest.mark.parametrize('marker', [True, False])
def test_offline_missing_projection_or_marker_is_read_only_legacy_then_migrates(store, tmp_path, marker):
    tid = store.new_thread('mock', str(tmp_path))
    turn(store, tid, 't')
    for n in range(5):
        store.put_item('t', {'id':str(n)})
    projected = History(store)
    first = projected.page(tid, {'limit':2}, items=True)
    expected = projected.page(tid, {'cursor':first['nextCursor']}, items=True)
    if marker:
        # Table with no completion marker must not be trusted by an offline reader.
        store.db.execute('DELETE FROM meta WHERE key=?', (history_order.MARKER,))
        store.db.execute('DELETE FROM history_item_order')
    else:
        # Likewise, marker alone is insufficient.
        remove_model(store.db, marker=False)
    reader = Reader(store.directory)
    try:
        assert not reader.history.item_order
        assert reader.history.page(tid, {'cursor':first['nextCursor']}, items=True) == expected
        assert reader.db.total_changes == 0
        assert reader.db.execute('PRAGMA query_only').fetchone()[0] == 1
        before = store.db.execute('SELECT count(*) FROM sqlite_master').fetchone()[0]
        reader.history.page(tid, {}, items=True)
        assert store.db.execute('SELECT count(*) FROM sqlite_master').fetchone()[0] == before
    finally:
        reader.db.close()
    store._migrate_schema()
    projection_matches(store.db)
    assert History(store).page(tid, {'cursor':first['nextCursor']}, items=True) == expected
    changes = store.db.total_changes
    with store.transaction():
        history_order.ensure_schema(store.db)
    for _ in range(3):
        assert History(store).item_order
    assert store.db.total_changes == changes  # No backfill on construction/reopen migration.


def test_backfill_marker_and_triggers_publish_atomically(store, tmp_path):
    tid = store.new_thread('mock', str(tmp_path))
    turn(store, tid, 't')
    store.put_item('t', {'id':'i'})
    remove_model(store.db)
    denied = []
    def deny_trigger(action, name, *_):
        if action == sqlite3.SQLITE_CREATE_TRIGGER and name.startswith('history_order_'):
            denied.append(name)
            return sqlite3.SQLITE_DENY
        return sqlite3.SQLITE_OK
    store.db.set_authorizer(deny_trigger)
    try:
        with pytest.raises(sqlite3.DatabaseError):
            store._migrate_schema()
    finally:
        store.db.set_authorizer(None)
    assert denied == ['history_order_item_inserted']  # Failure after the backfill.
    assert not history_order.available(store.db)
    assert store.db.execute("SELECT 1 FROM sqlite_master WHERE name='history_item_order'").fetchone() is None
    store._migrate_schema()
    assert history_order.available(store.db)
    projection_matches(store.db)


def test_pre_revert_database_offline_then_writer_upgrade(store, tmp_path):
    tid = store.new_thread('mock', str(tmp_path))
    turn(store, tid, 'legacy')
    store.put_item('legacy', {'id':'old-item'})
    expected = History(store).page(tid, {}, items=True)
    remove_model(store.db)
    store.db.execute('ALTER TABLE turns DROP COLUMN reverted')
    store.db.execute("UPDATE meta SET value='1' WHERE key='schema'")
    reader = Reader(store.directory)
    try:
        assert not reader.history.item_order
        assert reader.history.page(tid, {}, items=True) == expected
        assert reader.db.total_changes == 0
    finally:
        reader.db.close()
    store._migrate_schema()
    assert History(store).page(tid, {}, items=True) == expected
    assert store.db.execute("SELECT value FROM meta WHERE key='schema'").fetchone()[0] == '2'


def steps(db, call):
    count = 0
    def tick():
        nonlocal count
        count += 1
    db.set_progress_handler(tick, 1)
    try:
        result = call()
    finally:
        db.set_progress_handler(None, 0)
    return result, count


def test_fixed_visible_page_work_is_flat_with_history_and_reverted_prefix(tmp_path):
    results = []
    for scale in (1, 10):
        store = Store(tmp_path / str(scale))
        try:
            tid = store.new_thread('mock', str(tmp_path))
            # Same audit calibration: 327 turns/7,974 items at1x; x10 at10x.
            # Remove the model so the one-time migration is measured separately.
            remove_model(store.db)
            with store.transaction():
                store.db.executemany("INSERT INTO turns(id,thread_id,state,started,completed) VALUES(?,?,'completed',?,?)",
                                     [(f't{i}',tid,i,i+1) for i in range(327*scale)])
                store.db.executemany('INSERT INTO items(turn_id,id,payload) VALUES(?,?,?)',
                                     [(f't{i//25}',f'i{i}',dump({'id':f'i{i}','type':'agentMessage','text':'synthetic'}))
                                      for i in range(7974*scale)])
            legacy = History(store)
            assert not legacy.item_order
            before, baseline = steps(store.db, lambda: legacy.page(tid, {'limit':100}, items=True))
            _, migration = steps(store.db, store._migrate_schema)
            current = History(store)
            after, candidate = steps(store.db, lambda: current.page(tid, {'limit':100}, items=True))
            assert after == before
            _, per_turn = steps(store.db, lambda: current.page(tid, {'turnId':'t0','limit':10}, items=True))
            store.db.execute('UPDATE turns SET reverted=1 WHERE started<?', (200*scale,))
            for direction in ('asc', 'desc'):
                params = {'limit':100, 'sortDirection':direction}
                expected = legacy.page(tid, params, items=True)
                actual, dense = steps(store.db, lambda: current.page(tid, params, items=True))
                assert actual == expected
                backwards = dict(params, sortDirection='desc' if direction=='asc' else 'asc', cursor=actual['backwardsCursor'])
                assert current.page(tid, backwards, items=True) == legacy.page(tid, backwards, items=True)
                if direction == 'asc':
                    dense_asc = dense
            plan = [tuple(r) for r in store.db.execute('''EXPLAIN QUERY PLAN SELECT i.seq,i.turn_id,i.payload
                FROM history_item_order p CROSS JOIN items i ON i.seq=p.seq
                WHERE p.thread_id=? AND p.seq>? ORDER BY p.seq LIMIT 101''', (tid, 0))]
            assert not any('TEMP B-TREE' in row[3] for row in plan)
            _, deletion = steps(store.db, lambda: store.db.execute('DELETE FROM items WHERE id=?', (f'i{7974*scale-1}',)))
            projection_matches(store.db)
            results.append(dict(scale=scale,baseline_page_steps=baseline,page_steps=candidate,
                                reverted_page_steps=dense_asc,per_turn_steps=per_turn,migration_steps=migration,
                                point_delete_steps=deletion,plan=plan))
        finally:
            store.close()
    print('HISTORY_ORDER_GATE '+json.dumps(results))
    a, b = results
    assert b['baseline_page_steps'] > a['baseline_page_steps'] * 3
    for key in ('page_steps', 'reverted_page_steps', 'per_turn_steps', 'point_delete_steps'):
        assert b[key] == a[key], (key, results)
    assert b['migration_steps'] > a['migration_steps'] * 5
