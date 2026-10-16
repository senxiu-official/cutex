"""Offline (read-only) history reader used by cutex while the agent is stopped."""
import io
import json
import subprocess
import sys
from pathlib import Path

from cute_claude.history_reader import Reader, serve
from cute_claude.store import Store


def ask(reader, method, params=None):
    return reader.respond({'id': 1, 'method': method, 'params': params or {}})


async def test_reader_matches_live_shapes_while_the_writer_is_open(h):
    tid = await h.thread()
    for text in ('first question', 'second question'):
        await h.turn(tid, text)
        await h.drain()
    reader = Reader(h.settings.state_dir)  # The live adapter still holds the writer lease.
    try:
        init = ask(reader, 'initialize')['result']
        assert Path(init['codexHome']) == h.settings.state_dir
        live = await h.call('thread/read', {'threadId': tid, 'includeTurns': True})
        offline = ask(reader, 'thread/read', {'threadId': tid, 'includeTurns': True})['result']
        assert offline['thread']['status'] == {'type': 'notLoaded'}
        assert live['thread']['status'] == {'type': 'idle'}
        for value in (live, offline):
            value['thread'].pop('status')
            value['thread'].pop('reasoningEffort')
        assert offline == live
        params = {'threadId': tid, 'limit': 1, 'sortDirection': 'desc', 'itemsView': 'full'}
        live_page = await h.call('thread/turns/list', params)
        offline_page = ask(reader, 'thread/turns/list', params)['result']
        assert offline_page == live_page and len(offline_page['data']) == 1 and offline_page['nextCursor']
        older = ask(reader, 'thread/turns/list', dict(params, cursor=offline_page['nextCursor']))['result']
        assert older == await h.call('thread/turns/list', dict(params, cursor=live_page['nextCursor']))
        items = ask(reader, 'thread/items/list', {'threadId': tid})['result']
        assert items == await h.call('thread/items/list', {'threadId': tid})
    finally:
        reader.db.close()


async def test_reader_never_takes_the_writer_lease_or_writes(tmp_path):
    state = tmp_path / 'state'
    store = Store(state)
    tid = store.new_thread('mock', str(tmp_path), '')
    store.close()
    reader = Reader(state)
    try:
        # A runtime can still start (take the lease) while the reader is warm.
        writer = Store(state)
        writer.close()
        assert ask(reader, 'thread/read', {'threadId': tid})['result']['thread']['id'] == tid
        missing = ask(reader, 'thread/read', {'threadId': 'other'})['error']
        assert missing['code'] == -32004
        assert reader.db.execute('PRAGMA query_only').fetchone()[0] == 1
        unsupported = ask(reader, 'turn/start', {'threadId': tid})['error']
        assert unsupported['code'] == -32601 and unsupported['data']['unsupported'] is True
    finally:
        reader.db.close()


def test_never_started_agent_has_empty_history(tmp_path):
    reader = Reader(tmp_path / 'never-started')
    assert ask(reader, 'thread/read', {'threadId': 't'})['error']['code'] == -32004
    assert ask(reader, 'thread/turns/list', {'threadId': 't'})['result'] == {
        'data': [], 'nextCursor': None, 'backwardsCursor': None}
    assert not (tmp_path / 'never-started').exists()


def test_stdio_lines_skip_notifications_and_report_bad_json(tmp_path):
    reader = Reader(tmp_path / 'none')
    stdin = io.BytesIO(b'{"method":"initialized"}\nnot json\n{"id":7,"method":"initialize","params":{}}\n')
    stdout = io.BytesIO()
    serve(reader, stdin, stdout)
    lines = [json.loads(line) for line in stdout.getvalue().splitlines()]
    assert [line['id'] for line in lines] == [None, 7]
    assert lines[0]['error']['code'] == -32700 and 'codexHome' in lines[1]['result']
    assert b'\r\n' not in stdout.getvalue()


def test_module_entry_point_serves_stdio(tmp_path):
    state = tmp_path / 'state'
    store = Store(state)
    tid = store.new_thread('mock', str(tmp_path), '')
    store.close()
    src = Path(__file__).resolve().parents[1] / 'src'
    requests = [{'id': 1, 'method': 'initialize', 'params': {}}, {'method': 'initialized'},
                {'id': 2, 'method': 'thread/read', 'params': {'threadId': tid, 'includeTurns': True}}]
    done = subprocess.run([sys.executable, '-m', 'cute_claude.history_reader', '--state-dir', str(state)],
                          input=''.join(json.dumps(r) + '\n' for r in requests).encode(), capture_output=True,
                          env={'PYTHONPATH': str(src), **{k: v for k, v in __import__('os').environ.items() if k != 'PYTHONPATH'}},
                          timeout=60, check=True)
    responses = [json.loads(line) for line in done.stdout.splitlines()]
    assert [r['id'] for r in responses] == [1, 2]
    assert responses[1]['result']['thread']['id'] == tid and responses[1]['result']['thread']['turns'] == []
