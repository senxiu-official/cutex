"""Native context receipts while a real adapter turn is held by the offline SDK.

Only Claude Code's process and transcript writer are fixtures. Admission,
authenticated hooks, account-path discovery, observer, SQLite and replay are real.
"""
import asyncio
from dataclasses import replace
import json
from pathlib import Path
import uuid

import pytest
import pytest_asyncio

from cute_claude import context_receipts as receipt_module
from cute_claude.busy_delivery import STEER_FRAME
from cute_claude.subscription import OfficialSdkDriver
from test_busy_delivery import soon
from test_perms_steer import post_hook, running_turn, steer
from test_persistent import ph, until  # noqa: F401
from test_r34_insert_stages import expected, live, publications, result, saved
from test_r8_services import binding, envelope, invoke
from test_subscription import SubHarness


STAGE = 'claude/input/stageChanged'
CONTEXT = 'claude/input/contextPersisted'


@pytest_asyncio.fixture
async def held(ph):
    # Exercise production owner/generation checks, without accessing an account
    # outside install_fake_sdk's private account directory.
    settings = replace(ph.settings, external_owner_id='owner', external_runtime_generation=7)
    ph.settings = ph.server.settings = ph.engine.settings = settings
    ph.server.external_inputs.settings = settings
    ph.server.external_inputs.runtime_generation = 7
    tid, turn, bridge = await running_turn(ph)
    await until(lambda: ph.sdk.clients[0].query_count == 1)
    path = Path(ph.engine.driver.config_dir) / 'projects' / 'fixture-safe-project' / (bridge.sid + '.jsonl')
    path.parent.mkdir(parents=True, exist_ok=True)
    # The file must exist before offered() captures EOF.
    path.touch()
    return ph, tid, turn, bridge, path


def write(path, *records):
    with path.open('ab') as stream:
        for record in records:
            stream.write((json.dumps(record, ensure_ascii=False) + '\n').encode('utf-8'))


def proof(sid, call, notes):
    parent = str(uuid.uuid4())
    return (
        {'type': 'user', 'sessionId': sid, 'isSidechain': False, 'uuid': parent,
         'message': {'role': 'user', 'content': [
             {'type': 'tool_result', 'tool_use_id': call, 'content': 'fixture'}]}},
        {'type': 'attachment', 'sessionId': sid, 'isSidechain': False,
         'uuid': str(uuid.uuid4()), 'parentUuid': parent,
         'attachment': {'type': 'hook_additional_context', 'hookEvent': 'PostToolUse',
                        'hookName': 'PostToolUse:Read', 'toolUseID': call, 'content': [notes]}},
    )


async def offer(held, client, call=None, text=None):
    ph, tid, turn, bridge, _ = held
    call = call or 'read-' + client
    await ph.call('turn/steer', steer(tid, client=client, text=text or 'insert ' + client,
                                     expectedTurnId=turn))
    notes = (await post_hook(ph, bridge, call))['hookSpecificOutput']['additionalContext']
    await publications(ph)
    assert live(ph, STAGE, client) == expected(tid, turn, client, ['registered', 'offered'])
    assert not live(ph, CONTEXT, client)
    return call, notes


async def status(ph, tid, client):
    return await ph.call('claude/input/status', {'threadId': tid, 'messageId': 'steer-' + client})


async def unconfirmed(held, *clients):
    ph, tid, turn, _, _ = held
    # Read all available bytes through the production observer. A positive
    # control follows every malformed-record check to prove the reader is alive.
    await ph.server.busy_delivery.context_receipts.flush(tid, turn)
    await publications(ph)
    assert ph.store.turn(turn)['status'] == 'inProgress'
    for client in clients:
        assert not (await status(ph, tid, client))['contextPersisted']
        assert not live(ph, CONTEXT, client)
        assert not saved(ph, tid, CONTEXT, client)
        assert all(p['stage'] != 'contextPersisted' for p in saved(ph, tid, STAGE, client))


async def confirmed(held, client):
    ph, tid, turn, _, _ = held
    await until(lambda: len(live(ph, CONTEXT, client)) == 1)
    await publications(ph)
    want = expected(tid, turn, client, ['registered', 'offered', 'contextPersisted'])
    assert live(ph, STAGE, client) == saved(ph, tid, STAGE, client) == want
    receipt = {key: value for key, value in want[-1].items() if key != 'stage'}
    assert live(ph, CONTEXT, client) == saved(ph, tid, CONTEXT, client) == [receipt]
    value = await status(ph, tid, client)
    assert value['contextPersisted'] is True and value['contextTurnId'] == turn
    assert ph.store.turn(turn)['status'] == 'inProgress', 'receipt must precede Result'
    assert ph.sdk.clients[0].query_count == 1, 'steers must not create SDK turns'


async def test_three_sequential_hook_claims_ack_independently_before_result(held):
    ph, tid, turn, bridge, path = held
    clients = ('one', 'two', 'three')
    records = []
    # All three are already offered, so a turn-wide ACK would wrongly confirm
    # the other two as soon as the first attachment becomes available.
    for client in clients:
        call, notes = await offer(held, client)
        records.append(proof(bridge.sid, call, notes))
    for index, (client, (predecessor, attachment)) in enumerate(zip(clients, records)):
        write(path, predecessor)
        await unconfirmed(held, *clients[index:])
        write(path, attachment)
        await confirmed(held, client)
        # Re-reading equivalent evidence cannot publish a second ACK or confirm
        # another offered message from a different authenticated hook claim.
        write(path, predecessor, attachment)
        await unconfirmed(held, *clients[index + 1:])
    assert len(live(ph, CONTEXT)) == 3
    await result(ph, tid)
    assert ph.store.turn(turn)['status'] == 'completed'
    for client in ('one', 'two', 'three'):
        assert len(live(ph, CONTEXT, client)) == 1
        assert live(ph, STAGE, client) == expected(tid, turn, client, ['registered', 'offered', 'contextPersisted'])


async def test_later_queued_input_cannot_use_an_earlier_attachment(held):
    ph, tid, turn, bridge, path = held
    call, notes = await offer(held, 'early')
    await ph.call('turn/steer', steer(tid, client='later', text='insert later', expectedTurnId=turn))
    write(path, *proof(bridge.sid, call, notes))
    await confirmed(held, 'early')
    await unconfirmed(held, 'later')
    assert saved(ph, tid, STAGE, 'later') == expected(tid, turn, 'later', ['registered'])
    later_notes = (await post_hook(ph, bridge, 'read-later'))['hookSpecificOutput']['additionalContext']
    write(path, *proof(bridge.sid, call, notes))
    await unconfirmed(held, 'later')
    write(path, *proof(bridge.sid, 'read-later', later_notes))
    await confirmed(held, 'later')


@pytest.mark.parametrize('mutation', [
    'session', 'sidechain', 'uuid', 'record-type', 'attachment-type', 'hook-event',
    'hook-name', 'call', 'substring', 'missing-parent', 'unrelated-parent',
    'parent-call', 'parent-session', 'parent-sidechain',
])
async def test_only_exact_main_chain_attachment_with_matching_tool_parent_confirms(held, mutation):
    _, _, _, bridge, path = held
    call, notes = await offer(held, 'strict')
    predecessor, attachment = proof(bridge.sid, call, notes)
    if mutation == 'session':
        attachment['sessionId'] = str(uuid.uuid4())
    elif mutation == 'sidechain':
        attachment['isSidechain'] = True
    elif mutation == 'uuid':
        attachment['uuid'] = 'not-a-native-uuid'
    elif mutation == 'record-type':
        attachment['type'] = 'system'
    elif mutation == 'attachment-type':
        attachment['attachment']['type'] = 'tool_result'
    elif mutation == 'hook-event':
        attachment['attachment']['hookEvent'] = 'PreToolUse'
    elif mutation == 'hook-name':
        attachment['attachment']['hookName'] = 'PostToolUse:Bash'
    elif mutation == 'call':
        attachment['attachment']['toolUseID'] = 'unrelated-tool'
    elif mutation == 'substring':
        attachment['attachment']['content'] = [notes + '\nextra bytes']
    elif mutation == 'missing-parent':
        attachment.pop('parentUuid')
    elif mutation == 'unrelated-parent':
        attachment['parentUuid'] = str(uuid.uuid4())
    elif mutation == 'parent-call':
        predecessor['message']['content'][0]['tool_use_id'] = 'unrelated-tool'
    elif mutation == 'parent-session':
        predecessor['sessionId'] = str(uuid.uuid4())
    elif mutation == 'parent-sidechain':
        predecessor['isSidechain'] = True
    write(path, predecessor, attachment)
    await unconfirmed(held, 'strict')
    write(path, *proof(bridge.sid, call, notes))
    await confirmed(held, 'strict')


async def test_complete_json_without_newline_waits_for_native_record_completion(held):
    _, _, _, bridge, path = held
    call, notes = await offer(held, 'partial', text='完整上下文 🌒')
    predecessor, attachment = proof(bridge.sid, call, notes)
    write(path, predecessor)
    raw = json.dumps(attachment, ensure_ascii=False).encode('utf-8')
    # Split within a multibyte codepoint, then finish valid JSON, then newline.
    split = raw.index('🌒'.encode('utf-8')) + 1
    with path.open('ab') as stream:
        stream.write(raw[:split])
    await unconfirmed(held, 'partial')
    with path.open('ab') as stream:
        stream.write(raw[split:])
    await unconfirmed(held, 'partial')
    with path.open('ab') as stream:
        stream.write(b'\n')
    await confirmed(held, 'partial')


async def test_preoffer_attachment_and_tool_result_alone_do_not_confirm(held):
    _, _, _, bridge, path = held
    call, text = 'read-old', 'same context before offer'
    write(path, *proof(bridge.sid, call, STEER_FRAME + text))
    actual_call, notes = await offer(held, 'old', call=call, text=text)
    assert notes == STEER_FRAME + text
    await unconfirmed(held, 'old')
    predecessor, attachment = proof(bridge.sid, actual_call, notes)
    write(path, predecessor)
    await unconfirmed(held, 'old')
    write(path, attachment)
    await confirmed(held, 'old')


async def test_new_attachment_cannot_reuse_a_tool_parent_from_before_offer(held):
    _, _, _, bridge, path = held
    call, text = 'read-old-parent', 'fresh offer'
    predecessor, attachment = proof(bridge.sid, call, STEER_FRAME + text)
    write(path, predecessor)
    _, notes = await offer(held, 'old-parent', call=call, text=text)
    write(path, attachment)
    await unconfirmed(held, 'old-parent')
    write(path, *proof(bridge.sid, call, notes))
    await confirmed(held, 'old-parent')


@pytest.mark.parametrize('sdk_kind', ['user', 'UserMessage'])
async def test_sdk_user_tool_result_is_not_native_attachment_proof(held, sdk_kind):
    ph, tid, _, bridge, path = held
    call, notes = await offer(held, 'sdk-message')
    predecessor, attachment = proof(bridge.sid, call, notes)
    await ph.sdk.clients[0].events.put({
        'type': sdk_kind, 'session_id': bridge.sid, 'uuid': predecessor['uuid'],
        'content': predecessor['message']['content'],
    })
    await until(lambda: ph.store.db.execute(
        'SELECT 1 FROM sdk_turn_messages WHERE thread_id=? AND native_uuid=?',
        (tid, predecessor['uuid'])).fetchone())
    await unconfirmed(held, 'sdk-message')
    write(path, predecessor)
    await unconfirmed(held, 'sdk-message')
    write(path, attachment)
    await confirmed(held, 'sdk-message')


async def test_attachment_must_match_full_hook_notes_including_passive_notes(held):
    ph, tid, _, bridge, path = held
    ph.store.enqueue_note(tid, 'passive-context', {
        'source': {'kind': 'agent', 'id': 'fixture-agent'},
        'eventType': 'agent_message', 'text': '独立附注 with exact whitespace\n  keep this',
    }, ph.settings.max_pending)
    call, notes = await offer(held, 'full-notes')
    owner_only = STEER_FRAME + 'insert full-notes'
    assert notes.startswith(owner_only) and '独立附注' in notes and notes != owner_only
    write(path, *proof(bridge.sid, call, owner_only))
    await unconfirmed(held, 'full-notes')
    write(path, *proof(bridge.sid, call, notes))
    await confirmed(held, 'full-notes')


@pytest.mark.parametrize('native_confirmed', [False, True])
async def test_interrupt_preserves_only_previously_confirmed_proof(held, native_confirmed):
    ph, tid, turn, bridge, path = held
    call, notes = await offer(held, 'interrupt')
    if native_confirmed:
        write(path, *proof(bridge.sid, call, notes))
        await confirmed(held, 'interrupt')
    else:
        await unconfirmed(held, 'interrupt')
    await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
    await ph.drain()
    # A late attachment cannot turn the interrupted, unconfirmed offer into ACK.
    write(path, *proof(bridge.sid, call, notes))
    await asyncio.sleep(0.2)
    await publications(ph)
    assert ph.store.turn(turn)['status'] == 'interrupted'
    assert (await status(ph, tid, 'interrupt'))['contextPersisted'] is native_confirmed
    assert len(live(ph, CONTEXT, 'interrupt')) == int(native_confirmed)
    stages = ['registered', 'offered'] + (['contextPersisted'] if native_confirmed else [])
    assert saved(ph, tid, STAGE, 'interrupt') == live(ph, STAGE, 'interrupt') == expected(tid, turn, 'interrupt', stages)


async def test_confirmed_receipts_survive_adapter_restart_and_cursor_replay(held):
    ph, tid, turn, bridge, path = held
    call, notes = await offer(held, 'durable')
    write(path, *proof(bridge.sid, call, notes))
    await confirmed(held, 'durable')
    await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
    await ph.drain()
    await publications(ph)
    before = ph.store.events(tid, 0, 1000)
    wanted = [e for e in before if e['method'] in {STAGE, CONTEXT} and e['params'].get('clientId') == 'durable']
    assert len(wanted) == 4
    cli, account = ph.engine.driver.cli, ph.engine.driver.config_dir
    await ph.close()

    async def already_closed():
        pass

    ph.close = already_closed  # shared fixture teardown must not close SQLite twice
    newer = SubHarness(ph.settings, OfficialSdkDriver(cli, account, interrupt_timeout=2))
    try:
        await newer.init()
        await newer.call('thread/resume', {'threadId': tid})
        value = await status(newer, tid, 'durable')
        assert value['contextPersisted'] is True and value['contextTurnId'] == turn
        replay = await newer.call('claude/events/read', {'threadId': tid, 'limit': 200})
        assert [e for e in replay['data'] if e['method'] in {STAGE, CONTEXT}
                and e['params'].get('clientId') == 'durable'] == wanted
        tail = await newer.call('claude/events/read', {
            'threadId': tid, 'afterSequence': wanted[0]['sequence'], 'limit': 200})
        assert [e for e in tail['data'] if e['method'] in {STAGE, CONTEXT}
                and e['params'].get('clientId') == 'durable'] == wanted[1:]
        # Reconstructing registration cannot regress the durable stage watermark.
        newer.server.busy_delivery.register(tid, 'steer-durable', value, turn_id=turn)
        await publications(newer)
        assert not live(newer, STAGE, 'durable') and not live(newer, CONTEXT, 'durable')
        assert [e for e in newer.store.events(tid, 0, 1000) if e['method'] in {STAGE, CONTEXT}
                and e['params'].get('clientId') == 'durable'] == wanted
        assert len(ph.sdk.clients) == 1, 'replay and registration must not start a native process'
    finally:
        await newer.close()


async def test_terminal_closes_admission_before_awaiting_native_flush(held, monkeypatch):
    ph, tid, turn, bridge, _ = held
    await ph.call('turn/steer', steer(tid, client='awaiting-hook', expectedTurnId=turn))
    data = {'session_id': bridge.sid, 'hook_event_name': 'PostToolUse', 'tool_name': 'Read',
            'tool_input': {'file_path': str(ph.settings.workspace / 'hello.txt')}, 'tool_response': 'ok'}
    call = 'read-arriving-after-result'
    await bridge.pre_tool(data, call, {})
    hook = ph.sdk.clients[0].options.hooks['PostToolUse'][0].hooks[0]
    observer = ph.server.busy_delivery.context_receipts
    entered, release = asyncio.Event(), asyncio.Event()
    flush = observer.flush

    async def blocked_flush(thread, active):
        if (thread, active) == (tid, turn):
            entered.set()
            await release.wait()
        await flush(thread, active)

    monkeypatch.setattr(observer, 'flush', blocked_flush)
    try:
        await ph.sdk.clients[0].events.put({
            'type': 'result', 'session_id': bridge.sid, 'subtype': 'success',
            'is_error': False, 'terminal_reason': 'completed', 'usage': {}, 'num_turns': 1})
        await asyncio.wait_for(entered.wait(), 5)
        assert ph.store.turn(turn)['status'] == 'inProgress'
        rejected = await ph.call('turn/steer', steer(
            tid, client='after-native-result', expectedTurnId=turn), error=True)
        assert rejected['code'] == -32009 and rejected['data']['reason'] == 'turnFinishing'
        assert not ph.store.input_known(tid, 'steer-after-native-result')
        assert not bridge.accepting
        assert ph.engine.driver.owners[tid].finishing_turn_id == turn
        assert 'additionalContext' not in json.dumps(await hook(data, call, {}))
        row = ph.store.db.execute('''SELECT context_turn_id,offered,persisted FROM busy_inputs
            WHERE thread_id=? AND message_id=?''', (tid, 'steer-awaiting-hook')).fetchone()
        assert tuple(row) == (None, 0, 0), 'late hook must leave the waiting input unclaimed'
        assert not live(ph, CONTEXT, 'awaiting-hook')
    finally:
        release.set()
    await ph.drain()
    await publications(ph)
    # The untouched input may run as an ordinary following turn. Its receipt
    # must never be attributed to the Result that arrived before the hook.
    assert ph.store.turn(turn)['status'] == 'completed'
    assert all(p['turnId'] != turn for p in live(ph, CONTEXT, 'awaiting-hook'))
    assert all(p['turnId'] != turn for p in saved(ph, tid, CONTEXT, 'awaiting-hook'))
    assert all(p['stage'] == 'registered' for p in saved(ph, tid, STAGE, 'awaiting-hook')
               if p['turnId'] == turn)
    assert not live(ph, CONTEXT, 'after-native-result')


async def test_external_inputs_same_turn_ack_only_the_attachment_message_ids(held):
    ph, tid, turn, bridge, path = held
    service = ph.server.external_inputs
    values = [soon(tid, mid) for mid in ('external-a', 'external-b')]
    offered = []
    for index, value in enumerate(values):
        submitted = await invoke(service, ph, 'submit', value, after=False)
        assert submitted['statuses'][0]['deliveryState'] == 'pending'
        call = 'read-external-' + str(index)
        notes = (await post_hook(ph, bridge, call))['hookSpecificOutput']['additionalContext']
        offered.append((call, notes))
    rows = ph.store.db.execute('''SELECT message_id,context_turn_id,offered,persisted
        FROM busy_inputs WHERE thread_id=? ORDER BY message_id''', (tid,)).fetchall()
    assert [tuple(row) for row in rows] == [
        ('external-a', turn, 1, 0), ('external-b', turn, 1, 0)]
    write(path, *proof(bridge.sid, *offered[0]))
    await until(lambda: any(p['messageId'] == 'external-a' for p in live(ph, CONTEXT)))
    await publications(ph)

    async def receipts():
        response = await invoke(service, ph, 'status', {
            **binding(tid), 'messages': [
                {'messageId': value['message']['id'], 'semanticSha256': value['semanticSha256']}
                for value in values]})
        first, second = response['statuses']
        assert first['deliveryState'] == 'context_persisted' and first['receipt']['turnId'] == turn
        assert second['deliveryState'] == 'pending' and second['receipt'] is None
        return first['receipt']

    receipt = await receipts()
    assert ph.store.turn(turn)['status'] == 'inProgress'
    second = await ph.call('claude/input/status', {'threadId': tid, 'messageId': 'external-b'})
    assert second['contextTurnId'] == turn and not second['contextPersisted']
    assert [p['messageId'] for p in live(ph, CONTEXT)] == ['external-a']
    assert [p['messageId'] for p in saved(ph, tid, CONTEXT)] == ['external-a']
    # No B proof is appended. An interruption must retain only A's receipt in
    # both the permanent external ledger and the busy-input persisted bit.
    await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
    await ph.drain()
    assert await receipts() == receipt
    assert [p['messageId'] for p in live(ph, CONTEXT)] == ['external-a']
    second = await ph.call('claude/input/status', {'threadId': tid, 'messageId': 'external-b'})
    assert not second['contextPersisted']


@pytest.mark.parametrize('terminal', ['interrupted', 'failed'])
async def test_non_success_terminal_flush_drains_complete_attachment_larger_than_chunk(held, monkeypatch, terminal):
    ph, tid, turn, bridge, path = held
    observer = ph.server.busy_delivery.context_receipts
    allow_poll = asyncio.Event()
    watch = observer._watch

    async def sleeping_poll(reader):
        # Hold only the background scheduling path: the terminal must consume
        # the complete pre-existing proof through its real production flush.
        await allow_poll.wait()
        await watch(reader)

    monkeypatch.setattr(observer, '_watch', sleeping_poll)
    clients = ['large-' + str(index) for index in range(receipt_module.MAX_CHUNK_BYTES // 60000 + 2)]
    try:
        for client in clients:
            await ph.call('turn/steer', steer(tid, client=client, text=client + ':' + 'x' * 60000,
                                             expectedTurnId=turn))
        call = 'read-large-batch'
        notes = (await post_hook(ph, bridge, call))['hookSpecificOutput']['additionalContext']
        assert len(notes.encode('utf-8')) > receipt_module.MAX_CHUNK_BYTES
        await publications(ph)
        assert not live(ph, CONTEXT)
        write(path, *proof(bridge.sid, call, notes))
        assert path.stat().st_size > receipt_module.MAX_CHUNK_BYTES
        assert observer.readers[tid].offset == 0, 'timer must not pre-consume the proof'
        if terminal == 'interrupted':
            await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
            await ph.drain()
            await publications(ph)
        else:
            await result(ph, tid, failed=True)
        assert ph.store.turn(turn)['status'] == terminal
        assert len(live(ph, CONTEXT)) == len(clients)
        for client in clients:
            value = await status(ph, tid, client)
            assert value['contextPersisted'] is True and value['contextTurnId'] == turn
            want = expected(tid, turn, client, ['registered', 'offered', 'contextPersisted'])
            assert live(ph, STAGE, client) == saved(ph, tid, STAGE, client) == want
            assert len(live(ph, CONTEXT, client)) == len(saved(ph, tid, CONTEXT, client)) == 1
        events = ph.store.events(tid, 0, 1000)
        completed = next(e['sequence'] for e in events if e['method'] == 'turn/completed'
                         and e['params']['turn']['id'] == turn)
        assert all(e['sequence'] < completed for e in events if e['method'] == CONTEXT)
        assert not observer.readers and not observer._tasks
        assert ph.sdk.clients[0].query_count == 1
    finally:
        allow_poll.set()


@pytest_asyncio.fixture
async def passive_owner_start(ph, monkeypatch):
    settings = replace(ph.settings, external_owner_id='owner', external_runtime_generation=7)
    ph.settings = ph.server.settings = ph.engine.settings = settings
    ph.server.external_inputs.settings = settings
    ph.server.external_inputs.runtime_generation = 7
    tid = await ph.thread()
    value = envelope(tid, 'passive-before-owner', passive=True)
    await invoke(ph.server.external_inputs, ph, 'submit', value, after=False)

    # The fixture process recognizes a literal HANG command. Keep the complete
    # SDK user message (including preceding note blocks) and adapt only that
    # fixture command, so adding a note cannot accidentally complete the turn.
    write_command = ph.sdk.ClaudeSDKClient.write

    async def hold_query(client, command):
        if command.get('type') == 'query':
            command = {**command, 'prompt': 'HANG'}
        await write_command(client, command)

    monkeypatch.setattr(ph.sdk.ClaudeSDKClient, 'write', hold_query)
    tid, turn, bridge = await running_turn(ph, tid)
    await until(lambda: ph.sdk.clients[0].query_count == 1)
    row = ph.store.db.execute('SELECT turn_id,state FROM notes WHERE thread_id=? AND message_id=?',
                              (tid, value['message']['id'])).fetchone()
    assert tuple(row) == (turn, 'injected')
    blocks = ph.sdk.clients[0].messages[0]['message']['content']
    note_frame = next(block['text'] for block in blocks
                      if block.get('text', '').startswith('EXTERNAL_INPUT_JSON\n'))
    assert json.loads(note_frame.split('\n', 1)[1])['text'] == value['message']['text']
    assert ph.store.pending_notes(tid) == 0
    path = Path(ph.engine.driver.config_dir) / 'projects' / 'fixture-safe-project' / (bridge.sid + '.jsonl')
    path.parent.mkdir(parents=True, exist_ok=True)
    path.touch()
    return (ph, tid, turn, bridge, path), value


async def passive_status(ph, tid, value):
    response = await invoke(ph.server.external_inputs, ph, 'status', {
        **binding(tid), 'messages': [{'messageId': value['message']['id'],
                                     'semanticSha256': value['semanticSha256']}]})
    return response['statuses'][0]


async def test_passive_start_note_waits_for_own_successful_terminal_receipt(passive_owner_start):
    held, value = passive_owner_start
    ph, tid, turn, bridge, path = held
    mid = value['message']['id']
    pending = await passive_status(ph, tid, value)
    assert pending['deliveryState'] == 'pending' and pending['receipt'] is None
    call, notes = await offer(held, 'unrelated-to-passive')
    assert notes == STEER_FRAME + 'insert unrelated-to-passive', 'start note is outside this hook proof'
    write(path, *proof(bridge.sid, call, notes))
    await confirmed(held, 'unrelated-to-passive')
    pending = await passive_status(ph, tid, value)
    assert pending['deliveryState'] == 'pending' and pending['receipt'] is None
    assert not [p for p in live(ph, CONTEXT) if p['messageId'] == mid]
    assert not [p for p in saved(ph, tid, CONTEXT) if p['messageId'] == mid]

    await result(ph, tid)
    assert ph.store.turn(turn)['status'] == 'completed'
    persisted = await passive_status(ph, tid, value)
    assert persisted['deliveryState'] == 'context_persisted'
    assert persisted['receipt']['turnId'] == turn
    own = [p for p in live(ph, CONTEXT) if p['messageId'] == mid]
    assert own == [{'threadId': tid, 'turnId': turn, 'messageId': mid, 'clientId': None}]
    assert [p for p in saved(ph, tid, CONTEXT) if p['messageId'] == mid] == own
    journal = ph.store.events(tid, 0, 1000)
    note_event = next(e for e in journal if e['method'] == CONTEXT and e['params']['messageId'] == mid)
    initial_mid = ph.store.db.execute('''SELECT message_id FROM inputs
        WHERE thread_id=? AND turn_id=? AND json_extract(payload,'$.clientId')='first' ''',
        (tid, turn)).fetchone()[0]
    initial_event = next(e for e in journal if e['method'] == CONTEXT and e['params']['messageId'] == initial_mid)
    assert note_event['sequence'] < initial_event['sequence'], 'passive note receives its own explicit ACK'


@pytest.mark.parametrize('terminal', ['interrupted', 'failed'])
async def test_unanswered_passive_start_note_gets_no_receipt_from_non_success_terminal(passive_owner_start, terminal):
    held, value = passive_owner_start
    ph, tid, turn, _, _ = held
    mid = value['message']['id']
    # No model output or native hook evidence: neither non-success terminal is
    # allowed to borrow the successful-Result path to acknowledge this note.
    assert not live(ph, CONTEXT)
    if terminal == 'interrupted':
        await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
        await ph.drain()
        await publications(ph)
    else:
        await result(ph, tid, failed=True)
    assert ph.store.turn(turn)['status'] == terminal
    pending = await passive_status(ph, tid, value)
    assert pending['deliveryState'] == 'pending' and pending['receipt'] is None
    assert not [p for p in live(ph, CONTEXT) if p['messageId'] == mid]
    assert ph.store.db.execute('''SELECT count(*) FROM events
        WHERE thread_id=? AND method=? AND json_extract(params,'$.messageId')=?''',
        (tid, CONTEXT, mid)).fetchone()[0] == 0
    row = ph.store.db.execute('SELECT turn_id,state FROM notes WHERE thread_id=? AND message_id=?',
                              (tid, mid)).fetchone()
    assert tuple(row) == (None, 'pending'), 'unanswered passive note stays available to a later turn'
