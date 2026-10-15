import json

from cute_claude.store import Store, identifier
from cute_claude.presentation import framed


def test_external_display_survives_restart_without_fabricated_tool_history(tmp_path):
    store = Store(tmp_path)
    store.bind_backend('subscription')
    tid = identifier()
    store.ensure_thread(tid, 'sonnet', str(tmp_path), '')
    text = '必要上下文 ' * 1000
    payload = {'source': {'kind':'agent','id':'cutex.sender'}, 'eventType':'agent_message',
               'text':text, 'metadata':{'sourceName':'Reviewer','cutexSessionId':'cutex.receiver'}}
    receipt = store.enqueue(tid, 'message-one', payload, 128)
    display = store.input_presentation(tid, 'message-one')
    assert 'Reviewer' in display['presentation']['title']
    assert len(display['presentation']['body']) <= 513
    p = display['presentation']
    assert display['semanticSha256'] == framed(b'codex:presentation:semantic:v1\0',
        ['cutex.receiver',tid,p['id'],'agent','cutex.sender',p['title'],p['body'],'plainText'])
    store.claim(tid)
    assert store.items(receipt['turnId']) == []  # no fake tool call or duplicate user row
    assert store.db.execute('SELECT count(*) FROM history').fetchone()[0] == 0
    store.event(tid, 'thread/presentation/appended', {'threadId':tid,'_inputPresentationRef':'message-one'})
    store.close()
    store = Store(tmp_path)
    assert store.input_presentation(tid, 'message-one') == display
    timeline = store.timeline(tid, None, 100)['data']
    assert [e['item'] for e in timeline if e['type'] == 'presentation'] == [display]
    assert store.enqueue(tid, 'message-one', payload, 128)['duplicate']
    assert store.db.execute('SELECT count(*) FROM presentations').fetchone()[0] == 1
    store.close()


def test_completed_events_reference_content_and_timeline_paginates(tmp_path):
    store = Store(tmp_path)
    store.bind_backend('mock')
    tid = identifier()
    store.ensure_thread(tid, 'mock', str(tmp_path), '')
    r = store.enqueue(tid, 'u', {'source':{'kind':'user','id':'u'},'eventType':'message','text':'hello'}, 128)
    store.claim(tid)
    for i in range(7):
        item = {'type':'agentMessage','id':str(i),'text':'x' * 10000}
        store.put_item(r['turnId'], item)
        store.event(tid, 'item/completed', {'threadId':tid,'turnId':r['turnId'],'item':item})
    assert store.db.execute('SELECT sum(length(params)) FROM events').fetchone()[0] < 2000
    cursor, pages = None, []
    while True:
        page = store.timeline(tid, cursor, 3)
        pages.append(page['data'])
        cursor = int(page['nextCursor']) if page['nextCursor'] else None
        if cursor is None:
            break
    entries = [entry for page in reversed(pages) for entry in page]
    assert [entry['item']['id'] for entry in entries] == list(map(str, range(7)))
    assert len(store.events(tid, 0, 10)[0]['params']['item']['text']) == 10000
    store.close()
    assert not (tmp_path/'state.sqlite3-wal').exists() or (tmp_path/'state.sqlite3-wal').stat().st_size == 0
