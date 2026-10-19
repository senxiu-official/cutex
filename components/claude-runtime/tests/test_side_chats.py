"""Side chats (feature `sideChat`, claude-r21) and live forks (`forkWhileBusy`).

The fake SDK's native process keeps its "transcript" in `.fixture-sessions`;
FakeNative serves it as transcript entries and forks it like the official
SDK's `fork_session` (a copy up to a message). No credentials or network.
"""
import asyncio
import json
from pathlib import Path

import pytest

from cute_claude.errors import RpcError
from cute_claude.history import History
from cute_claude.side_chats import SIDE_BOUNDARY_PROMPT, SIDE_DEVELOPER_INSTRUCTIONS
from cute_claude.store import dump, identifier
from test_persistent import ph, until  # noqa: F401  (fixture)


class FakeNative:
    def __init__(self, config_dir):
        self.config_dir = Path(config_dir)
        self.calls = []

    def path(self, sid):
        return self.config_dir / '.fixture-sessions' / f'{sid}.json'

    def transcript(self, sid):
        return self.path(sid) if self.path(sid).exists() else None

    def entries(self, sid, *, live=False):
        prompts = json.loads(self.path(sid).read_text())['prompts']
        entries, parent = [], None
        for index, prompt in enumerate(prompts):
            user = {'uuid': f'{sid}:u{index}', 'parentUuid': parent, 'type': 'user',
                    'message': {'role': 'user', 'content': prompt}}
            answer = {'uuid': f'{sid}:a{index}', 'parentUuid': user['uuid'], 'type': 'assistant',
                      'message': {'role': 'assistant', 'content': [{'type': 'text', 'text': prompt}]}}
            entries += [user, answer]
            parent = answer['uuid']
        return entries

    async def call(self, kind, sid, **arguments):
        self.calls.append((kind, sid, arguments))
        if kind == 'delete':
            self.path(sid).unlink()
            return {}
        entries = self.entries(sid)
        cut = next(i for i, e in enumerate(entries) if e['uuid'] == arguments['upToMessageId'])
        kept = sum(1 for e in entries[:cut + 1] if e['type'] == 'user')
        new = identifier()
        prompts = json.loads(self.path(sid).read_text())['prompts'][:kept]
        self.path(new).write_text(json.dumps({'sid': new, 'prompts': prompts}))
        return {'sessionId': new}


@pytest.fixture
def native(ph):
    fake = FakeNative(ph.engine.driver.config_dir)
    ph.server.r8.sessions.native = fake
    return fake


async def read(ph, tid, chat, after=None, wait=0):
    params = {'threadId': tid, 'chatId': chat}
    if after is not None:
        params.update(afterRevision=after, waitMs=wait)
    return (await ph.call('claude/side/read', params))['chat']


async def settle(ph, tid, chat_id):
    async with asyncio.timeout(5):
        while True:
            chat = await read(ph, tid, chat_id)
            if chat['status'] == 'ready':
                return chat
            await read(ph, tid, chat_id, chat['revision'], 200)


def answers(chat):
    return [e['params']['item']['text'] for e in chat['events']
            if e['method'] == 'item/completed' and e['params']['item']['type'] == 'agentMessage']


async def test_side_sees_parent_context_and_never_writes_into_it(ph, native):
    tid = await ph.thread()
    first = await ph.turn(tid, 'ALPHA'); await ph.drain()
    await ph.turn(tid, 'BETA'); await ph.drain()
    parent_sid = ph.engine.session(tid)['session_id']
    parent_file = native.path(parent_sid).read_text()
    parent_events = len(ph.store.events(tid, 0, 1000))
    parent_client = ph.sdk.clients[0]
    seen = len(ph.out)

    chat = (await ph.call('claude/side/open', {'threadId': tid, 'lastTurnId': first, 'anchorTurnId': first}))['chat']
    side = chat['threadId']
    assert chat['status'] == 'ready' and chat['forkedThroughTurnId'] == first and chat['events'] == []
    # Official fork_session up to the first turn's last message; the parent is only read.
    assert native.calls == [('fork', parent_sid, {'upToMessageId': f'{parent_sid}:a0', 'title': 'Side'})]
    side_sid = ph.engine.session(side)['session_id']
    assert side_sid != parent_sid and ph.engine.session(side)['attempted'] == 1
    assert History(ph.store).full_read(side) == []  # Nothing projected: Side shows its own turns only.

    sent = await ph.call('claude/side/send', {'threadId': tid, 'chatId': chat['id'], 'text': 'hello', 'clientMessageId': 'm1'})
    again = await ph.call('claude/side/send', {'threadId': tid, 'chatId': chat['id'], 'text': 'hello', 'clientMessageId': 'm1'})
    assert again['turnId'] == sent['turnId']
    chat = await settle(ph, tid, chat['id'])
    client = ph.sdk.clients[-1]
    assert client is not parent_client and client.options.resume == side_sid
    # The parent's system prompt (Claude Code keeps a resumed session's own; same cache prefix).
    assert client.options.system_prompt == parent_client.options.system_prompt
    blocks = [b['text'] for b in client.messages[0]['message']['content']]
    assert blocks == [SIDE_DEVELOPER_INSTRUCTIONS, SIDE_BOUNDARY_PROMPT, 'hello']
    shown = [e for e in chat['events'] if e['method'] == 'thread/presentation/appended'
             or (e['params'].get('item') or {}).get('type') == 'userMessage']
    assert 'hello' in dump(shown) and 'Side conversation boundary' not in dump(shown)  # Never shown.
    await ph.call('claude/side/send', {'threadId': tid, 'chatId': chat['id'], 'text': 'HISTORY', 'clientMessageId': 'm2'})
    chat = await settle(ph, tid, chat['id'])
    history = answers(chat)[-1]
    # The parent's first turn is context; its later turn is not; the boundary went once.
    assert history.startswith('[FIXTURE] ALPHA|') and 'BETA' not in history and history.endswith('hello|HISTORY')
    assert not any(e['method'] == 'thread/started' for e in chat['events'])
    error = await ph.call('claude/side/send', {'threadId': tid, 'chatId': chat['id'], 'text': 'other', 'clientMessageId': 'm1'}, error=True)
    assert error['data']['code'] == 'side_message_id_conflict'

    # The parent saw nothing: transcript, events, subscribers, listing.
    assert native.path(parent_sid).read_text() == parent_file
    assert len(ph.store.events(tid, 0, 1000)) == parent_events
    assert not any(side in dump(m) for m in ph.out[seen:] if 'method' in m)
    listed = (await ph.call('thread/list'))['data']
    assert [t['id'] for t in listed] == [tid]
    assert [c['id'] for c in (await ph.call('claude/side/list', {'threadId': tid}))['chats']] == [chat['id']]

    assert await ph.call('claude/side/close', {'threadId': tid, 'chatId': chat['id']}) == {'closed': True}
    assert native.calls[-1][:2] == ('delete', side_sid) and not native.path(side_sid).exists()
    with pytest.raises(RpcError, match='thread not found'):
        ph.store.thread(side)
    assert (await ph.call('claude/side/list', {'threadId': tid}))['chats'] == []
    error = await ph.call('claude/side/read', {'threadId': tid, 'chatId': chat['id']}, error=True)
    assert error['data']['code'] == 'side_chat_not_found'
    # The parent's runtime kept running and still answers.
    assert not ph.engine.driver.owners[tid].task.done()
    await ph.turn(tid, 'GAMMA'); await ph.drain()
    assert len(History(ph.store).full_read(tid)) == 3


async def test_side_opens_on_a_busy_parent_and_keeps_one_chat_per_parent(ph, native):
    tid = await ph.thread()
    done = await ph.turn(tid, 'ALPHA'); await ph.drain()
    running = await ph.turn(tid, 'HANG')
    await until(lambda: 'HANG' in native.path(ph.engine.session(tid)['session_id']).read_text())
    chat = (await ph.call('claude/side/open', {'threadId': tid}))['chat']
    # The running turn is left out; the parent is neither interrupted nor closed.
    assert chat['forkedThroughTurnId'] == done
    parent_sid = ph.engine.session(tid)['session_id']
    assert native.calls[0][2]['upToMessageId'] == f'{parent_sid}:a0'
    assert ph.store.turn(running)['status'] == 'inProgress'
    assert ph.engine.driver.owners[tid].active.turn_id == running
    same = (await ph.call('claude/side/open', {'threadId': tid}))['chat']
    assert same['id'] == chat['id']
    error = await ph.call('claude/side/open', {'threadId': tid, 'anchorTurnId': done}, error=True)
    assert error['data'] == {'code': 'side_chat_already_open', 'chatId': chat['id'], 'anchorTurnId': None}
    error = await ph.call('claude/side/open', {'threadId': chat['threadId']}, error=True)
    assert error['data']['code'] == 'side_runtime_unsupported'
    for cutoff, code in [(running, 'turn_in_progress'), ('missing', 'turn_not_found')]:
        await ph.call('claude/side/close', {'threadId': tid, 'chatId': chat['id']})
        error = await ph.call('claude/side/open', {'threadId': tid, 'lastTurnId': cutoff}, error=True)
        assert error['data']['code'] == code
        chat = (await ph.call('claude/side/open', {'threadId': tid}))['chat']
    # Closing the parent's runtime closes its Side chats first.
    await ph.call('claude/runtime/close', {'threadId': tid})
    assert ph.server.side_chats.chats == {}
    assert ph.store.db.execute('SELECT count(*) FROM threads').fetchone()[0] == 1


async def test_side_approvals_stay_in_the_side_chat(ph, native):
    tid = await ph.thread()
    await ph.turn(tid, 'ALPHA'); await ph.drain()
    ph.peer.interactive = True
    await ph.call('claude/approvals/set', {'threadId': tid, 'mode': 'default'})
    chat = (await ph.call('claude/side/open', {'threadId': tid}))['chat']
    side = chat['threadId']
    assert ph.server.approvals.mode(side) == 'default'  # Permissions are the parent's.
    seen = len(ph.out)
    task = asyncio.create_task(ph.server.approvals.request_command(side, 'turn', 'item', 'ls'))
    await until(lambda: ph.server.side_chats.chats[chat['id']].pending)
    snapshot = await read(ph, tid, chat['id'])
    request = snapshot['pendingRequests'][0]
    assert request['method'] == 'item/commandExecution/requestApproval' and request['params']['command'] == 'ls'
    assert not any('id' in m and m.get('method') for m in ph.out[seen:])  # Never sent to the owner's clients.
    error = await ph.call('claude/side/respond', {'threadId': tid, 'chatId': chat['id'], 'requestId': 'nope', 'result': {}}, error=True)
    assert error['data']['code'] == 'side_request_not_found'
    await ph.call('claude/side/respond', {'threadId': tid, 'chatId': chat['id'], 'requestId': request['id'],
                                         'result': {'decision': 'accept'}})
    assert await asyncio.wait_for(task, 2) == 'accept'
    assert (await read(ph, tid, chat['id']))['pendingRequests'] == []
    await ph.call('claude/side/close', {'threadId': tid, 'chatId': chat['id']})


async def test_leftover_side_threads_are_deleted_and_shutdown_closes_chats(ph, native):
    tid = await ph.thread()
    await ph.turn(tid, 'ALPHA'); await ph.drain()
    chat = (await ph.call('claude/side/open', {'threadId': tid}))['chat']
    ph.server.side_chats.chats.clear()  # As after an adapter restart.
    ph.server.side_chats.swept = False
    await ph.call('claude/side/list', {'threadId': tid})
    with pytest.raises(RpcError, match='thread not found'):
        ph.store.thread(chat['threadId'])
    await ph.call('claude/side/open', {'threadId': tid})
    await ph.call('claude/runtime/close', {'threadId': tid})
    assert ph.server.side_chats.chats == {}


async def test_side_is_refused_without_the_sdk_backend(h):
    tid = await h.thread()
    error = await h.call('claude/side/open', {'threadId': tid}, error=True)
    assert error['code'] == -32601 and error['data']['code'] == 'side_runtime_unsupported'
    caps = await h.call('claude/capabilities')
    assert caps['features']['sideChat'] is False and caps['features']['forkWhileBusy'] is False


def test_features_declare_side_chat_and_busy_fork():
    features = json.loads((Path(__file__).parents[1] / 'src/cute_claude/features.json').read_text())
    assert features['release'] == 'claude-r21'
    assert features['features']['sideChat'] is True and features['features']['forkWhileBusy'] is True
