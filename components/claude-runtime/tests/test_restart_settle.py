"""A turn the previous adapter process left running (killed, crashed, host
restart) is settled as `interrupted` on the next start, and the first
subscribers of the new process receive the settling events."""
from __future__ import annotations

from cute_claude.config import Settings
from cute_claude.store import Store
from conftest import Harness


def running_turn(store: Store, tmp_path):
    tid = store.new_thread('fixture', str(tmp_path))
    receipt = store.enqueue(tid, 'input', {'source': {'kind': 'user', 'id': 'owner'}, 'eventType': 'user.message',
                                           'text': 'run a tool', 'metadata': {}}, 4)
    turn = receipt['turnId']
    assert store.claim(tid)
    tool = {'type': 'commandExecution', 'id': 'call-1', 'command': 'sleep 600', 'cwd': str(tmp_path),
            'processId': None, 'source': 'agent', 'status': 'inProgress', 'commandActions': [],
            'aggregatedOutput': None, 'exitCode': None, 'durationMs': None, 'pluginId': None, 'scriptPath': None}
    card = {'type': 'collabAgentToolCall', 'id': 'call-2', 'status': 'inProgress'}
    store.put_item(turn, tool)
    store.put_item(turn, card)
    return tid, turn


async def test_restart_settles_running_turn_as_interrupted_and_replays_on_resume(tmp_path):
    workspace = tmp_path / 'workspace'
    workspace.mkdir()
    settings = Settings(workspace, tmp_path / 'state')
    store = Store(settings.state_dir)
    tid, turn = running_turn(store, workspace)
    store.db.close()  # the process dies: no close(), no checkpoint
    store._lock.close()

    h = Harness(settings)  # next start: Store() runs recover()
    try:
        saved = h.store.turn(turn)
        assert saved['status'] == 'interrupted'
        assert [item['status'] for item in saved['items'] if 'status' in item] == ['failed', 'failed']
        assert h.store.input_status(tid, 'input')['state'] == 'uncertain'
        assert h.store.thread(tid)['halted']

        await h.init()
        await h.call('thread/resume', {'threadId': tid})
        replayed = [m for m in h.out if 'method' in m and m['method'] in {'item/completed', 'turn/completed'}]
        assert [m['method'] for m in replayed] == ['item/completed', 'item/completed', 'turn/completed']
        assert replayed[-1]['params']['turn']['id'] == turn
        assert replayed[-1]['params']['turn']['status'] == 'interrupted'
        assert {m['params']['item']['id'] for m in replayed[:2]} == {'call-1', 'call-2'}

        # thread/read after a reconnect returns the settled turn as well.
        read = await h.call('thread/read', {'threadId': tid, 'includeTurns': True})
        assert read['thread']['turns'][-1]['status'] == 'interrupted'

        # Once the thread's next turn ran, a later subscriber gets no replay.
        h.store.db.execute('UPDATE threads SET halted=0 WHERE id=?', (tid,))
        await h.turn(tid, 'next')
        await h.drain()
        h.out.clear()
        await h.call('thread/resume', {'threadId': tid})
        assert not [m for m in h.out if m.get('method') == 'turn/completed' and m['params']['turn']['id'] == turn]
    finally:
        await h.close()


def test_recover_settles_an_in_progress_turn_without_a_processing_input(tmp_path):
    store = Store(tmp_path / 'state')
    tid, turn = running_turn(store, tmp_path)
    store.db.execute("UPDATE inputs SET state='uncertain' WHERE turn_id=?", (turn,))
    store.recover()
    try:
        assert store.turn(turn)['status'] == 'interrupted'
        assert store.recovered_events[tid][-2][0] == 'turn/completed'
    finally:
        store.close()
