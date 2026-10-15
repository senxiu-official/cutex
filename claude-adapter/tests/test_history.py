import json

import pytest

from cute_claude import history


def seed(h, tid, count=3, width=3):
    with h.store.transaction():
        for turn in range(count):
            turn_id = f'{tid}-turn-{turn}'
            h.store.db.execute('INSERT INTO turns(id,thread_id,state,started,completed) VALUES(?,?,?,?,?)',
                               (turn_id, tid, 'completed', 1.0, 2.0))
            for item in range(width):
                h.store.put_item(turn_id, {'id': f'{turn_id}-item-{item}', 'type': 'agentMessage', 'text': f'{turn}:{item}'})


@pytest.mark.parametrize('direction', ['asc', 'desc'])
async def test_keyset_pages_complete_stable_and_scoped(h, direction):
    tid = await h.thread(); seed(h, tid)
    other = await h.thread(); seed(h, other)
    for kind, total in [('turns', 3), ('items', 9)]:
        method = f'thread/{kind}/list'
        params = {'threadId':tid, 'limit':2, 'sortDirection':direction, 'itemsView':'notLoaded'}
        first = await h.call(method, params)
        seen = []
        page = first
        while True:
            seen.extend(page['data'])
            if not page['nextCursor']: break
            page = await h.call(method, dict(params, cursor=page['nextCursor']))
        ids = [v['item']['id'] if kind == 'items' else v['id'] for v in seen]
        assert len(set(ids)) == total
        assert ids == sorted(ids, reverse=direction == 'desc')
        reverse = await h.call(method, dict(params, sortDirection='desc' if direction=='asc' else 'asc', cursor=first['backwardsCursor']))
        assert reverse['data'][0] == first['data'][0]
        bad = await h.call(method, dict(params, threadId=other, cursor=first['nextCursor']), error=True)
        assert bad['code'] == -32602
    turn = f'{tid}-turn-1'
    page = await h.call('thread/items/list', {'threadId':tid,'turnId':turn})
    assert len(page['data']) == 3 and all(x['turnId']==turn for x in page['data'])
    error = await h.call('thread/items/list', {'threadId':other,'turnId':turn}, error=True)
    assert error['code'] == -32004
    assert not h.provider.requests


async def test_append_does_not_shift_descending_cursor(h):
    tid = await h.thread(); seed(h, tid)
    first = await h.call('thread/items/list', {'threadId':tid,'limit':2,'sortDirection':'desc'})
    h.store.put_item(f'{tid}-turn-2', {'id':'new-item','type':'agentMessage','text':'new'})
    second = await h.call('thread/items/list', {'threadId':tid,'limit':2,'sortDirection':'desc','cursor':first['nextCursor']})
    assert [x['item']['text'] for x in second['data']] == ['2:0','1:2']


async def test_history_larger_than_64m_is_complete_in_bounded_pages(h):
    tid = await h.thread(); seed(h, tid, count=1, width=0)
    payload = '测' * 100000
    count = 230  # 69MB UTF-8: exceeds the old full-response ceiling.
    with h.store.transaction():
        for n in range(count):
            item = {'id':f'large-{n}', 'type':'agentMessage', 'text':payload}
            h.store.put_item(f'{tid}-turn-0', item)
            h.store.event(tid, 'item/completed', {'threadId':tid,'turnId':f'{tid}-turn-0','item':item})
        h.store.event(tid, 'turn/completed', {'threadId':tid,'turn':h.store.turn(f'{tid}-turn-0', include_items=False)})
    resumed = await h.call('thread/resume', {'threadId':tid, 'historyMode':'paginated', 'excludeTurns':True})
    assert resumed['thread']['historyMode'] == 'paginated' and resumed['thread']['turns'] == []
    assert len(json.dumps(resumed).encode()) < 10000
    cursor, seen, maximum = None, [], 0
    while True:
        page = await h.call('thread/items/list', {'threadId':tid, 'limit':100, 'cursor':cursor})
        maximum = max(maximum, history.size(page))
        for entry in page['data']:
            assert entry['item']['text'] == payload
            seen.append(entry['item']['id'])
        cursor = page['nextCursor']
        h.out.clear()  # The test recorder is not a display history store.
        if cursor is None: break
    assert seen == [f'large-{n}' for n in range(count)]
    assert maximum < history.PAGE_BYTES + 4096
    cursor, timeline_ids = None, []
    # Timeline boundary reads must not hydrate the 69MB turn.
    original = h.store.turn
    def metadata_only(turn_id, *, include_items=True):
        assert not include_items
        return original(turn_id, include_items=False)
    h.store.turn = metadata_only
    try:
        while True:
            page = await h.call('thread/timeline/list', {'threadId':tid,'cursor':cursor})
            assert history.size(page) < history.PAGE_BYTES + 4096
            timeline_ids[0:0] = [e['item']['id'] for e in page['data'] if e['type']=='item']
            cursor = page['nextCursor']; h.out.clear()
            if cursor is None: break
    finally:
        h.store.turn = original
    assert timeline_ids == seen
    assert not h.provider.requests
    # Full reads (Management bootstrap) stay bounded: a turn too large to load
    # is listed with itemsView=notLoaded instead of failing the whole read.
    read = await h.call('thread/read', {'threadId':tid, 'includeTurns':True})
    [turn] = read['thread']['turns']
    assert turn['itemsView'] == 'notLoaded' and turn['items'] == []
    assert len(json.dumps(read).encode()) < 10000


async def test_full_read_keeps_newest_turns_within_budget(h, monkeypatch):
    tid = await h.thread(); seed(h, tid, count=5, width=2)
    with h.store.transaction():
        for n in range(5):
            h.store.db.execute('UPDATE turns SET started=? WHERE id=?', (float(n), f'{tid}-turn-{n}'))
    one_turn = history.History(h.store).turn_bytes(f'{tid}-turn-0')
    monkeypatch.setattr(history, 'FULL_READ_BYTES', one_turn * 2)
    read = await h.call('thread/read', {'threadId':tid, 'includeTurns':True})
    assert [t['id'] for t in read['thread']['turns']] == [f'{tid}-turn-3', f'{tid}-turn-4']
    assert all(t['itemsView'] == 'full' and len(t['items']) == 2 for t in read['thread']['turns'])


@pytest.mark.parametrize('params', [{'cursor':'garbage'}, {'limit':0}, {'limit':True}, {'sortDirection':'up'}])
async def test_invalid_page_parameters(h, params):
    tid = await h.thread()
    error = await h.call('thread/items/list', dict(params, threadId=tid), error=True)
    assert error['code'] == -32602


async def test_single_oversized_item_has_actionable_error_without_truncation(h, monkeypatch):
    tid = await h.thread(); seed(h, tid, 1, 0)
    h.store.put_item(f'{tid}-turn-0', {'id':'big','type':'agentMessage','text':'x'*1000})
    monkeypatch.setattr(history, 'ITEM_BYTES', 100)
    error = await h.call('thread/items/list', {'threadId':tid}, error=True)
    assert error['code'] == -32001 and 'single history item' in error['message']
    assert len(h.store.items(f'{tid}-turn-0')[0]['text']) == 1000


def test_cursor_survives_store_reopen(tmp_path):
    from cute_claude.store import Store
    path = tmp_path/'restart-state'
    store = Store(path)
    tid = store.new_thread('mock', str(tmp_path), '')
    store.db.execute('INSERT INTO turns(id,thread_id,state,started,completed) VALUES(?,?,?,?,?)', ('turn', tid, 'completed', 1, 2))
    for n in range(5):
        store.put_item('turn', {'id':str(n), 'type':'agentMessage', 'text':str(n)})
    pager = history.History(store)
    first = pager.page(tid, {'limit':2}, items=True)
    params = {'limit':2,'cursor':first['nextCursor']}
    expected = pager.page(tid, params, items=True)
    store.close()
    reopened = Store(path)
    try:
        assert history.History(reopened).page(tid, params, items=True) == expected
    finally:
        reopened.close()
