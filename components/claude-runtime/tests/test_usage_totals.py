"""058 S2: committed usage totals equal the former full re-sum, with per-write work."""
import json
import random

from cute_claude import usage_totals
from cute_claude.engine import Engine
from cute_claude.store import Store, dump


def old_totals(db, tid):
    """The pre-S2 `_emit_usage` algorithm, verbatim: re-sum every turn of the thread."""
    calls = []
    for row in db.execute('SELECT usage FROM turns WHERE thread_id=?', (tid,)):
        calls.extend(json.loads(row[0]).get('modelRequests', []))
    def totals(calls):
        usage = [c['usage'] for c in calls if isinstance(c.get('usage'), dict)]
        plain = sum(u.get('input_tokens', 0) for u in usage)
        created = sum(u.get('cache_creation_input_tokens', 0) for u in usage)
        cached = sum(u.get('cache_read_input_tokens', 0) for u in usage)
        output = sum(u.get('output_tokens', 0) for u in usage)
        return {'totalTokens': plain + created + cached + output, 'inputTokens': plain + created + cached,
                'cachedInputTokens': cached, 'cacheWriteInputTokens': created, 'outputTokens': output,
                'reasoningOutputTokens': 0}
    return totals(calls), totals([c for c in calls if c.get('kind') == 'background-task'])


def new_totals(store, tid):
    committed = usage_totals.thread_totals(store, tid)
    return (Engine._sums(committed.sums[:4]), Engine._sums(committed.sums[4:])), committed.revision


def thread(store, tid):
    store.db.execute('INSERT INTO threads(id,model,cwd,created,updated) VALUES(?,?,?,0,0)', (tid, 'm', '/w'))


def turn(store, tid, turn_id, started=0.0):
    store.db.execute("INSERT INTO turns(id,thread_id,state,started) VALUES(?,?,'completed',?)", (turn_id, tid, started))


def counts(rng):
    return {'input_tokens': rng.randrange(50), 'cache_creation_input_tokens': rng.randrange(500),
            'cache_read_input_tokens': rng.randrange(5000), 'output_tokens': rng.randrange(300)}


def record(store, turn_id):
    return json.loads(store.db.execute('SELECT usage FROM turns WHERE id=?', (turn_id,)).fetchone()[0])


def test_random_histories_match_the_full_resum(tmp_path):
    """Steps, results, retries, corrections, late background tasks, fork copies, reverts,
    a previous component's raw writes, deletes and restarts all keep the old numbers."""
    rng = random.Random(58)
    store = Store(tmp_path / 'state')
    threads = ['t1', 't2']
    for tid in threads:
        thread(store, tid)
    turns = {tid: [] for tid in threads}
    for step in range(600):
        tid = rng.choice(threads)
        op = rng.choice(['new', 'request', 'result', 'correct', 'task', 'task', 'repeat', 'fork', 'revert',
                         'outside', 'delete', 'restart', 'insert-outside'])
        if op == 'new' or not turns[tid]:
            turn_id = f'{tid}-u{step}'
            turn(store, tid, turn_id, step)
            turns[tid].append(turn_id)
            usage_totals.write_turn_usage(store, turn_id, {'modelRequests': [
                {'kind': 'sdk-turn', 'outcome': 'requested', 'usage': None}]})
        turn_id = rng.choice(turns[tid])
        current = record(store, turn_id)
        calls = list(current.get('modelRequests') or [])
        if op == 'request':
            calls.append({'step': len(calls) + 1, 'outcome': 'requested', 'usage': None})
            usage_totals.write_turn_usage(store, turn_id, {**current, 'modelRequests': calls})
        elif op in {'result', 'correct'} and calls:
            calls[-1] = {**calls[-1], 'outcome': 'received', 'usage': counts(rng), 'requestId': f'req-{step}'}
            usage_totals.write_turn_usage(store, turn_id, {**current, 'modelRequests': calls})
        elif op == 'task':
            calls.append({'kind': 'background-task', 'taskId': f'task-{step}', 'outcome': 'received', 'usage': counts(rng)})
            usage_totals.write_turn_usage(store, turn_id, {**current, 'modelRequests': calls})
        elif op == 'repeat':
            usage_totals.write_turn_usage(store, turn_id, current)
        elif op == 'fork':
            # session_ops._copy_projection: copies carry provenance, never modelRequests.
            new_id = f'{tid}-f{step}'
            store.db.execute("INSERT INTO turns(id,thread_id,state,started,usage) VALUES(?,?,'completed',?,?)",
                             (new_id, tid, step, dump({'inheritedFrom': {'threadId': 'x', 'turnId': turn_id}, 'calls': []})))
            turns[tid].append(new_id)
        elif op == 'revert':
            store.db.execute('UPDATE turns SET reverted=1 WHERE id=?', (turn_id,))
        elif op == 'outside':
            # A previous component (no S2) after an explicit repin: raw usage write.
            calls.append({'kind': 'background-task', 'taskId': 'old', 'outcome': 'received', 'usage': counts(rng)})
            store.db.execute('UPDATE turns SET usage=? WHERE id=?', (dump({**current, 'modelRequests': calls}), turn_id))
        elif op == 'insert-outside':
            new_id = f'{tid}-o{step}'
            store.db.execute("INSERT INTO turns(id,thread_id,state,started,usage) VALUES(?,?,'completed',?,?)",
                             (new_id, tid, step, dump({'modelRequests': [{'outcome': 'received', 'usage': counts(rng)}]})))
            turns[tid].append(new_id)
        elif op == 'delete' and len(turns[tid]) > 1:
            store.db.execute('DELETE FROM items WHERE turn_id=?', (turn_id,))
            store.db.execute('DELETE FROM turns WHERE id=?', (turn_id,))
            turns[tid].remove(turn_id)
        elif op == 'restart':
            store.close()
            store = Store(tmp_path / 'state')
        for other in threads:
            assert new_totals(store, other)[0] == old_totals(store.db, other), (step, op)
    store.close()


def test_repeats_do_not_count_twice_and_revisions_follow_changes(tmp_path):
    store = Store(tmp_path / 'state')
    thread(store, 't')
    turn(store, 't', 'a')
    usage = {'input_tokens': 1, 'cache_creation_input_tokens': 2, 'cache_read_input_tokens': 3, 'output_tokens': 4}
    first = {'modelRequests': [{'outcome': 'received', 'usage': usage, 'requestId': 'r1'}]}
    usage_totals.write_turn_usage(store, 'a', first)
    (total, _), revision = new_totals(store, 't')
    assert total['totalTokens'] == 10
    for _ in range(3):  # The same SDK update again: same totals, same revision.
        usage_totals.write_turn_usage(store, 'a', first)
        assert new_totals(store, 't') == ((total, old_totals(store.db, 't')[1]), revision)
    # A correction of the same request replaces it.
    corrected = {'modelRequests': [{'outcome': 'received', 'usage': {**usage, 'output_tokens': 40}, 'requestId': 'r1'}]}
    usage_totals.write_turn_usage(store, 'a', corrected)
    (total, _), later = new_totals(store, 't')
    assert total['outputTokens'] == 40 and later == revision + 1
    store.close()


def test_new_component_builds_existing_threads_once(tmp_path):
    """A database written by the previous component has no totals: the first read builds them."""
    store = Store(tmp_path / 'state')
    for name in ('usage_totals_turn_updated', 'usage_totals_turn_inserted', 'usage_totals_turn_deleted'):
        store.db.execute(f'DROP TRIGGER {name}')
    for table in ('usage_turn_totals', 'usage_thread_totals', 'usage_totals_stale'):
        store.db.execute(f'DROP TABLE {table}')
    thread(store, 't')
    for i in range(20):
        turn(store, 't', f'u{i}')
        store.db.execute('UPDATE turns SET usage=? WHERE id=?', (dump({'modelRequests': [
            {'outcome': 'received', 'usage': {'output_tokens': i, 'input_tokens': 1}}]}), f'u{i}'))
    store.close()
    store = Store(tmp_path / 'state')  # The new component opens it: additive tables only.
    assert store.db.execute("SELECT value FROM meta WHERE key='schema'").fetchone()[0] == '2'
    assert new_totals(store, 't')[0] == old_totals(store.db, 't')
    store.close()


def test_emission_work_does_not_grow_with_history(tmp_path):
    """832 turns (EVA sample): after the first build a write + read never re-reads other turns' usage."""
    store = Store(tmp_path / 'state')
    thread(store, 't')
    for i in range(832):
        turn(store, 't', f'u{i}', i)
        store.db.execute('UPDATE turns SET usage=? WHERE id=?', (dump({'modelRequests': [
            {'kind': 'sdk-turn', 'outcome': 'received', 'usage': {'output_tokens': 7, 'cache_read_input_tokens': 300}}]}), f'u{i}'))
    usage_totals.thread_totals(store, 't')  # First build: once.
    statements = []
    store.db.set_trace_callback(statements.append)
    turn(store, 't', 'live', 900)
    calls = [{'kind': 'sdk-turn', 'outcome': 'requested', 'usage': None}]
    for step in range(50):
        calls[-1] = {**calls[-1], 'outcome': 'received', 'usage': {'output_tokens': step}}
        usage_totals.write_turn_usage(store, 'live', {'modelRequests': calls})
        usage_totals.thread_totals(store, 't')
    store.db.set_trace_callback(None)
    assert not [s for s in statements if 'FROM turns WHERE thread_id' in s]
    assert new_totals(store, 't')[0] == old_totals(store.db, 't')
    store.close()


async def test_engine_emits_committed_totals_and_revision(h):
    tid = await h.thread()
    for value in ('one', 'two', 'three'):
        await h.turn(tid, value)
        await h.drain()
    updates = [m['params']['tokenUsage'] for m in h.out if m.get('method') == 'thread/tokenUsage/updated']
    assert updates
    total, background = old_totals(h.store.db, tid)
    assert updates[-1]['total'] == total and updates[-1]['backgroundTotal'] == background
    revisions = [u['totalRevision'] for u in updates]
    assert revisions == sorted(revisions)
