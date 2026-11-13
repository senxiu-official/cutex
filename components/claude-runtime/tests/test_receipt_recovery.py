import json
from types import SimpleNamespace
import uuid

import pytest

from cute_claude.receipt_recovery import recover
from cute_claude.store import Store, dump
from test_r8_services import envelope, install, invoke


def transcript(frame, sid, *, busy=False):
    text = 'EXTERNAL_INPUT_JSON\n' + dump(frame)
    content = [{'type':'tool_result', 'tool_use_id':'tool', 'content':text}] if busy else text
    rows = [
        {'type':'user','uuid':'prompt','parentUuid':None,'message':{'role':'user','content':text}},
        {'type':'api-request-blob','hash':'blob','message':{'role':'user','content':content}},
        {'type':'api-request','id':'failed-request','querySource':'sdk','keep':0,'tail':['blob']},
        {'type':'assistant','uuid':'quota','parentUuid':'prompt','requestId':'error', 'isApiErrorMessage':True,
         'message':{'model':'<synthetic>','usage':{'input_tokens':0},'content':[{'type':'text','text':'quota'}]}},
        {'type':'api-request','id':'continued-request','querySource':'sdk','base':'failed-request','keep':1,'tail':[]},
        {'type':'assistant','uuid':'output','parentUuid':'quota','requestId':'provider-request',
         'message':{'model':'claude-test','usage':{'input_tokens':1},'content':[{'type':'text','text':'real output'}]}},
    ]
    return [dict(row, sessionId=sid) for row in rows]


def write(path, rows):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(''.join(dump(row) + '\n' for row in rows), encoding='utf-8')


@pytest.mark.parametrize('busy', [False, True])
def test_exact_frame_in_inherited_primary_or_busy_request_has_real_output(tmp_path, busy):
    frame = {'source':{'kind':'service','id':'test'},'eventType':'notice','text':'hello'}
    path = tmp_path / 'native.jsonl'
    write(path, transcript(frame, 'session', busy=busy))
    assert recover(path, 'session', {'message':frame}, {'output':'carrying-turn'},
                   {'message':{'context_turn':'carrying-turn'} if busy else {'user_uuids':{'prompt'}}}) == {
        'message':('carrying-turn','continued-request','output')}


@pytest.mark.parametrize('case', ['synthetic','no-request','missing-second-request','missing-blob','wrong-text','wrong-session',
                                  'assistant-quote','sidechain','unmapped-output','reverted-branch','zero-tokens','error'])
def test_unproven_native_history_never_becomes_a_receipt(tmp_path, case):
    frame = {'source':{'kind':'service','id':'test'},'eventType':'notice','text':'hello'}
    rows = transcript(frame, 'session')
    mapped = {'output':'turn'}
    if case == 'synthetic': rows = rows[:4]
    elif case == 'no-request': rows = [row for row in rows if row['type'] != 'api-request']
    elif case == 'missing-second-request': del rows[4]
    elif case == 'missing-blob': rows = [row for row in rows if row['type'] != 'api-request-blob']
    elif case == 'wrong-text': frame = dict(frame, text='different')
    elif case == 'wrong-session': rows = [dict(row, sessionId='other') for row in rows]
    elif case == 'assistant-quote': rows[1]['message']['role'] = 'assistant'
    elif case == 'sidechain': rows[-1]['isSidechain'] = True
    elif case == 'unmapped-output': mapped = {}
    elif case == 'reverted-branch': rows.append(dict(rows[0], uuid='selected', parentUuid='prompt'))
    elif case == 'zero-tokens': rows[-1]['message']['usage']['input_tokens'] = 0
    elif case == 'error': rows[-1]['isApiErrorMessage'] = True
    path = tmp_path / 'native.jsonl'
    write(path, rows)
    assert recover(path, 'session', {'message':frame}, mapped, {'message':{'user_uuids':{'prompt'}}}) == {}


@pytest.mark.parametrize('mode', ['primary','busy','unoffered','duplicate','reverted','synthetic','unmapped'])
async def test_status_recovers_only_proven_receipts_without_replaying_input(h, tmp_path, mode):
    tid = await h.thread()
    service = install(h, subscription=True)
    value = envelope(tid)
    from cute_claude.external_input import semantic_digest
    if mode in {'busy','unoffered'}:
        value['message']['delivery'] = 'soon'
        value['semanticSha256'] = semantic_digest(value)
    await invoke(service, h, 'submit', value, after=False)
    original = h.store.claim(tid)
    input_uuid = str(uuid.uuid4())
    h.store.record_sdk_message(tid, original['turn_id'], input_uuid, 'user')
    h.store.finish(tid, original['turn_id'], 'failed')
    h.store.db.execute('UPDATE threads SET halted=0 WHERE id=?', (tid,))
    # A later real turn inherited the failed primary input in its native request.
    second = envelope(tid, mid='later')
    second['message']['text'] = 'continue'
    from cute_claude.external_input import semantic_digest
    second['semanticSha256'] = semantic_digest(second)
    await invoke(service, h, 'submit', second, after=False)
    carrying = h.store.claim(tid)
    h.store.finish(tid, carrying['turn_id'], 'completed')
    output_uuid = str(uuid.uuid4())
    if mode != 'unmapped':
        h.store.record_sdk_message(tid, carrying['turn_id'], output_uuid, 'assistant')
    if mode in {'busy','unoffered'}:
        h.store.db.execute('UPDATE busy_inputs SET context_turn_id=?,offered=? WHERE thread_id=? AND message_id=?',
                           (carrying['turn_id'], int(mode == 'busy'), tid, 'message'))
    if mode == 'reverted':
        h.store.db.execute('UPDATE turns SET reverted=1 WHERE id=?', (carrying['turn_id'],))
    if mode == 'duplicate':
        duplicate = envelope(tid, mid='duplicate')
        await invoke(service, h, 'submit', duplicate, after=False)
    sid = str(uuid.uuid4())
    h.engine.driver = SimpleNamespace(config_dir=tmp_path)
    h.engine.inspect_session = lambda selected: {'session_id':sid} if selected == tid else None
    payload = json.loads(original['payload'])
    frame = json.loads(Store.project_input(payload)[0]['text'].split('\n', 1)[1])
    path = tmp_path / 'projects' / 'test' / (sid + '.jsonl')
    rows = transcript(frame, sid, busy=mode in {'busy','unoffered'})
    if mode in {'busy','unoffered'}:
        rows[0]['message']['content'] = 'ongoing work'
    if mode == 'synthetic':
        rows[-1]['message']['model'] = '<synthetic>'
    rows[-1]['uuid'] = output_uuid
    rows[0]['uuid'] = input_uuid
    rows[3]['parentUuid'] = input_uuid
    write(path, rows)
    before = h.store.db.execute('SELECT count(*) FROM inputs').fetchone()[0]
    first = service._status(tid, 'message', value['semanticSha256'], recover=True)
    proven = mode in {'primary','busy'}
    assert first['deliveryState'] == ('context_persisted' if proven else 'pending')
    if proven:
        assert first['receipt']['turnId'] == carrying['turn_id']
        assert service._status(tid, 'message', value['semanticSha256'])['receipt'] == first['receipt']
    assert h.store.db.execute('SELECT count(*) FROM inputs').fetchone()[0] == before
    assert h.store.db.execute('SELECT count(*) FROM external_input_recovery').fetchone()[0] == int(proven)
