"""Claude Code keeps a resumed session's recorded system prompt (`snapshot`, default on).

The adapter turns the snapshot off for a resume whose `append` differs from the
recorded one (or whose record is unknown), so changed harness text, runtime
limits or instructions reach the model; an unchanged prompt keeps the record.
The real CLI behaviour is probed by tests/test_system_prompt_native.py.
"""
from __future__ import annotations

from cute_claude.sdk_runtime import PROMPT_RECORD, prompt_digest
from test_persistent import ph, until  # noqa: F401  (fixture)
from test_side_chats import native, settle  # noqa: F401  (fixture)


def record(h, tid):
    return h.store.thread_properties(tid).get(PROMPT_RECORD)


async def restart(h, tid):
    await h.call('claude/runtime/close', {'threadId': tid})
    await h.call('claude/runtime/start', {'threadId': tid})
    return h.sdk.clients[-1]


async def test_unchanged_prompt_keeps_the_snapshot_and_a_changed_one_turns_it_off(ph):
    tid = await ph.thread()
    await ph.turn(tid, 'one'); await ph.drain()
    first = ph.sdk.clients[0].options.system_prompt
    assert 'snapshot' not in first and ph.sdk.clients[0].options.resume is None
    assert record(ph, tid) == {'digest': prompt_digest(first['append'])}

    same = await restart(ph, tid)
    assert same.options.resume and same.options.system_prompt == first  # Record reused: same cache prefix.

    # A new harness / instructions text after the session's first request.
    ph.store.db.execute('UPDATE threads SET instructions=? WHERE id=?', ('NEW_ROLE_TEXT', tid))
    changed = await restart(ph, tid)
    assert changed.options.system_prompt['snapshot'] is False
    assert changed.options.system_prompt['append'].endswith('NEW_ROLE_TEXT')
    assert record(ph, tid) == {'digest': prompt_digest(first['append'])}  # The native record is still the old one.
    await ph.turn(tid, 'two'); await ph.drain()
    again = await restart(ph, tid)
    assert again.options.system_prompt['snapshot'] is False  # Stays off: snapshot false never re-records.

    # Back to the recorded text: the record is current again.
    ph.store.db.execute("UPDATE threads SET instructions='' WHERE id=?", (tid,))
    back = await restart(ph, tid)
    assert back.options.system_prompt == first


async def test_compaction_rerecords_the_launch_prompt(ph):
    tid = await ph.thread()
    await ph.turn(tid, 'one'); await ph.drain()
    ph.store.db.execute('UPDATE threads SET instructions=? WHERE id=?', ('NEW_ROLE_TEXT', tid))
    client = await restart(ph, tid)
    assert client.options.system_prompt['snapshot'] is False
    await client.events.put({'type': 'system', 'subtype': 'compact_boundary', 'data': {}})
    current = prompt_digest(client.options.system_prompt['append'])
    await until(lambda: record(ph, tid) == {'digest': current})
    after = await restart(ph, tid)
    assert 'snapshot' not in after.options.system_prompt


async def test_resume_without_a_known_record_renders_fresh(ph):
    # Sessions started before the record existed (claude-r21 and older).
    tid = await ph.thread()
    await ph.turn(tid, 'one'); await ph.drain()
    ph.store.set_thread_properties(tid, {})
    client = await restart(ph, tid)
    assert client.options.system_prompt['snapshot'] is False


async def test_auto_mode_change_after_the_first_request_reaches_the_prompt(ph):
    from cute_claude.config import AUTO_MODE_LIMIT
    tid = await ph.thread()
    await ph.turn(tid, 'one'); await ph.drain()
    await ph.server.approvals.configure(tid, 'auto')
    client = await restart(ph, tid)
    assert client.options.system_prompt['snapshot'] is False and AUTO_MODE_LIMIT in client.options.system_prompt['append']


async def test_side_chat_inherits_the_parent_record(ph, native):
    # Side shares the parent's system prompt (and prompt cache) in both states.
    tid = await ph.thread()
    await ph.turn(tid, 'one'); await ph.drain()
    for instructions in ('', 'NEW_ROLE_TEXT'):
        ph.store.db.execute('UPDATE threads SET instructions=? WHERE id=?', (instructions, tid))
        parent = (await restart(ph, tid)).options.system_prompt
        assert ('snapshot' in parent) == bool(instructions)
        chat = (await ph.call('claude/side/open', {'threadId': tid}))['chat']
        assert record(ph, chat['threadId']) == record(ph, tid)
        await ph.call('claude/side/send', {'threadId': tid, 'chatId': chat['id'], 'text': 'q', 'clientMessageId': 'm'})
        await settle(ph, tid, chat['id'])
        side = ph.sdk.clients[-1].options
        assert side.resume == ph.engine.session(chat['threadId'])['session_id'] and side.system_prompt == parent
        await ph.call('claude/side/close', {'threadId': tid, 'chatId': chat['id']})
