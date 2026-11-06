"""Native persistence evidence only; every transcript is a private fixture."""
import asyncio
import json
from types import SimpleNamespace
import uuid

import pytest
import pytest_asyncio

from cute_claude import context_receipts as module
from cute_claude.context_receipts import ContextReceipts


def uid():
    return str(uuid.uuid4())


@pytest_asyncio.fixture
async def proof(tmp_path):
    sid, tid, turn = uid(), uid(), uid()
    config = tmp_path / 'account'
    project = config / 'projects' / 'private-workspace'
    project.mkdir(parents=True)
    path = project / f'{sid}.jsonl'
    path.write_bytes(b'')
    bridge = SimpleNamespace(sid=sid, binding=(turn, uid()))
    owner = SimpleNamespace(session_id=sid, bridge=bridge, connection_id=uid(),
                            native_connection_generation=1)
    driver = SimpleNamespace(config_dir=config, owners={tid: owner})
    server = SimpleNamespace(engine=SimpleNamespace(driver=driver))
    acks = []

    async def ack(thread, consuming_turn, mids):
        acks.append((thread, consuming_turn, mids))

    observer = ContextReceipts(server, ack)
    fixture = SimpleNamespace(sid=sid, tid=tid, turn=turn, path=path, bridge=bridge,
                              owner=owner, server=server, acks=acks, observer=observer)
    yield fixture
    await observer.close()


def offer(p, mids=('first',), call='tool-1', context='EXACT returned context'):
    p.observer.offer(p.tid, p.bridge, call, mids, context, hook_name='PostToolUse:Read')


def parent(p, call='tool-1', **overrides):
    return {'type': 'user', 'uuid': uid(), 'sessionId': p.sid, 'isSidechain': False,
            'message': {'role': 'user', 'content': [
                {'type': 'tool_result', 'tool_use_id': call, 'content': 'not retained'}]},
            **overrides}


def attachment(p, preceding, call='tool-1', context='EXACT returned context', **overrides):
    return {'type': 'attachment', 'uuid': uid(), 'sessionId': p.sid, 'isSidechain': False,
            'parentUuid': preceding['uuid'], 'attachment': {
                'type': 'hook_additional_context', 'hookEvent': 'PostToolUse',
                'hookName': 'PostToolUse:Read',
                'toolUseID': call, 'content': [context]}, **overrides}


def append(p, *records):
    with p.path.open('ab') as stream:
        for record in records:
            stream.write(json.dumps(record).encode() + b'\n')


async def flush(p):
    await p.observer.flush(p.tid, p.turn)


async def test_exact_linked_context_acks_only_offered_mids_before_terminal(proof):
    p = proof
    offer(p, ('one', 'two', 'one'))
    user = parent(p)
    append(p, user)
    await flush(p)
    assert p.acks == [], 'tool result alone is not context proof'
    reader = p.observer.readers[p.tid]
    assert reader.parents[user['uuid']][1] == frozenset({'tool-1'})
    assert 'not retained' not in repr(reader.parents)
    append(p, attachment(p, user))
    await flush(p)
    assert p.acks == [(p.tid, p.turn, ['one', 'two'])]
    assert p.tid not in p.observer.readers
    await flush(p)
    assert len(p.acks) == 1


@pytest.mark.parametrize('bad', ['session', 'child', 'missing-sidechain', 'tool', 'context',
                                      'context-substring', 'uuid', 'parent', 'missing-parent',
                                      'kind', 'hook', 'user-message', 'content-shape',
                                      'hook-name', 'missing-hook-name'])
async def test_unrelated_or_untrusted_attachment_never_acks(proof, bad):
    p = proof
    offer(p)
    user = parent(p)
    record = attachment(p, user)
    if bad == 'session': record['sessionId'] = uid()
    elif bad == 'child': record['isSidechain'] = True
    elif bad == 'missing-sidechain': record.pop('isSidechain')
    elif bad == 'tool': record['attachment']['toolUseID'] = 'another-tool'
    elif bad == 'context': record['attachment']['content'] = ['wrong context']
    elif bad == 'context-substring': record['attachment']['content'] = ['prefix EXACT returned context suffix']
    elif bad == 'uuid': record['uuid'] = 'not-a-native-uuid'
    elif bad == 'parent': record['parentUuid'] = uid()
    elif bad == 'missing-parent': record.pop('parentUuid')
    elif bad == 'kind': record['attachment']['type'] = 'hook_success'
    elif bad == 'hook': record['attachment']['hookEvent'] = 'PreToolUse'
    elif bad == 'user-message': record['type'] = 'user'
    elif bad == 'content-shape': record['attachment']['content'] = 'EXACT returned context'
    elif bad == 'hook-name': record['attachment']['hookName'] = 'PostToolUse:Bash'
    elif bad == 'missing-hook-name': record['attachment'].pop('hookName')
    append(p, user, record)
    await flush(p)
    assert p.acks == []


@pytest.mark.parametrize('bad', ['session', 'child', 'agent', 'uuid', 'call', 'no-tool-result', 'after'])
async def test_parent_must_be_new_complete_mainchain_matching_tool_result(proof, bad):
    p = proof
    offer(p)
    user = parent(p)
    if bad == 'session': user['sessionId'] = uid()
    elif bad == 'child': user['isSidechain'] = True
    elif bad == 'agent': user['agentId'] = 'child'
    elif bad == 'uuid': user['uuid'] = 'bad'
    elif bad == 'call': user['message']['content'][0]['tool_use_id'] = 'another-tool'
    elif bad == 'no-tool-result': user['message']['content'][0]['type'] = 'text'
    record = attachment(p, user)
    append(p, *(record, user) if bad == 'after' else (user, record))
    await flush(p)
    assert p.acks == []


async def test_complete_newline_required_for_each_record(proof):
    p = proof
    offer(p)
    user = parent(p)
    p.path.write_bytes(json.dumps(user).encode())
    await flush(p)
    assert p.observer.readers[p.tid].parents == {}
    with p.path.open('ab') as stream:
        stream.write(b'\n' + json.dumps(attachment(p, user)).encode())
    await flush(p)
    assert p.acks == []
    with p.path.open('ab') as stream:
        stream.write(b'\n')
    await flush(p)
    assert len(p.acks) == 1


async def test_old_records_and_old_partial_line_are_not_evidence(proof):
    p = proof
    user = parent(p)
    append(p, user, attachment(p, user))
    partial = attachment(p, user)
    with p.path.open('ab') as stream:
        stream.write(json.dumps(partial).encode())
    offer(p)
    with p.path.open('ab') as stream:
        stream.write(b'\n')
    append(p, attachment(p, user))
    await flush(p)
    assert p.acks == []
    fresh = parent(p)
    append(p, fresh, attachment(p, fresh))
    await flush(p)
    assert len(p.acks) == 1


async def test_later_offer_cannot_use_already_written_record_or_parent(proof):
    p = proof
    offer(p, ('old',))
    user = parent(p)
    append(p, user, attachment(p, user))
    offer(p, ('new',))  # Same call/text intentionally; EOF separates the offers.
    reader = p.observer.readers[p.tid]
    task = reader.task
    await flush(p)
    assert p.acks == [(p.tid, p.turn, ['old'])]
    assert p.observer.readers[p.tid].task is task
    append(p, attachment(p, user))
    await flush(p)
    assert len(p.acks) == 1
    fresh = parent(p)
    append(p, fresh, attachment(p, fresh))
    await flush(p)
    assert p.acks[-1] == (p.tid, p.turn, ['new'])


@pytest.mark.parametrize('change', ['generation', 'connection', 'owner', 'driver', 'binding', 'sid', 'owner-session'])
async def test_changed_ownership_never_acks(proof, change):
    p = proof
    offer(p)
    user = parent(p)
    append(p, user, attachment(p, user))
    if change == 'generation': p.owner.native_connection_generation += 1
    elif change == 'connection': p.owner.connection_id = uid()
    elif change == 'owner': p.server.engine.driver.owners[p.tid] = SimpleNamespace(**vars(p.owner))
    elif change == 'driver': p.server.engine.driver = SimpleNamespace(**vars(p.server.engine.driver))
    elif change == 'binding': p.bridge.binding = (p.turn, uid())
    elif change == 'sid': p.bridge.sid = uid()
    elif change == 'owner-session': p.owner.session_id = uid()
    await flush(p)
    assert p.acks == [] and not p.observer.readers


async def test_truncation_stops_instead_of_reusing_offsets(proof):
    p = proof
    append(p, {'padding': 'x' * 2000})
    offer(p)
    p.path.write_bytes(b'')
    await flush(p)
    assert not p.observer.readers and not p.acks


async def test_replacement_file_stops_reader(proof):
    p = proof
    offer(p)
    replacement = p.path.with_suffix('.replacement')
    replacement.write_bytes(b'')
    replacement.replace(p.path)
    await flush(p)
    assert not p.observer.readers and not p.acks


async def test_missing_file_can_appear_after_offer(proof):
    p = proof
    p.path.unlink()
    offer(p)
    user = parent(p)
    append(p, user, attachment(p, user))
    await flush(p)
    assert len(p.acks) == 1


async def test_incremental_reads_and_line_limit(proof, monkeypatch):
    p = proof
    monkeypatch.setattr(module, 'MAX_CHUNK_BYTES', 64)
    offer(p)
    user = parent(p)
    append(p, user, attachment(p, user))
    for _ in range(30):
        await flush(p)
    assert len(p.acks) == 1
    offer(p, ('oversized',))
    monkeypatch.setattr(module, 'MAX_LINE_BYTES', 80)
    with p.path.open('ab') as stream:
        stream.write(b'x' * 81)
    await flush(p)
    await flush(p)
    assert not p.observer.readers and len(p.acks) == 1


async def test_watcher_runs_only_pending_and_close_cancels(proof):
    p = proof
    assert not p.observer.readers
    offer(p)
    reader = p.observer.readers[p.tid]
    user = parent(p)
    append(p, user, attachment(p, user))
    async with asyncio.timeout(2):
        while not reader.task.done():
            await asyncio.sleep(.01)
    assert len(p.acks) == 1 and not p.observer.readers
    offer(p, ('pending',))
    task = p.observer.readers[p.tid].task
    await p.observer.close(p.tid)
    assert task.done() and not p.observer.readers
    await p.observer.close()


async def test_wrong_turn_flush_does_not_consume_evidence(proof):
    p = proof
    offer(p)
    user = parent(p)
    append(p, user, attachment(p, user))
    await p.observer.flush(p.tid, 'other-turn')
    assert not p.acks
    await flush(p)
    assert len(p.acks) == 1


async def test_ambiguous_account_transcript_fails_closed(proof):
    p = proof
    other = p.path.parent.parent / 'another-project'
    other.mkdir()
    (other / p.path.name).write_bytes(b'')
    offer(p)
    assert not p.observer.readers and not p.acks


@pytest.mark.parametrize('failure', ['path-error', 'open-error', 'deleted', 'ack-error'])
async def test_flush_observational_failures_do_not_escape_and_close_joins_tasks(proof, monkeypatch, failure):
    p = proof
    offer(p)
    reader = p.observer.readers[p.tid]
    user = parent(p)
    append(p, user, attachment(p, user))
    if failure == 'path-error':
        def unavailable(_):
            raise OSError('private fixture unavailable')
        monkeypatch.setattr(reader.api, 'transcript', unavailable)
    elif failure == 'open-error':
        original = type(p.path).open
        def denied(path, *args, **kwargs):
            if path == p.path:
                raise PermissionError('private fixture denied')
            return original(path, *args, **kwargs)
        monkeypatch.setattr(type(p.path), 'open', denied)
    elif failure == 'deleted':
        p.path.unlink()
    else:
        async def failed_ack(*_):
            raise RuntimeError('private fixture callback failure')
        p.observer.acknowledge = failed_ack
    await flush(p)
    await p.observer.close(p.tid)
    assert reader.task.done() and not p.observer.readers
    assert not p.observer._tasks and not p.acks


async def test_truncate_then_regrow_cannot_reuse_parent_index(proof):
    p = proof
    offer(p)
    user = parent(p)
    append(p, user)
    await flush(p)
    offset = p.path.stat().st_size
    p.path.write_bytes(b'x' * offset + b'\n')
    append(p, attachment(p, user))
    await flush(p)
    assert not p.acks and not p.observer.readers


async def test_pending_expiry_and_parent_index_bounds(proof, monkeypatch):
    p = proof
    monkeypatch.setattr(module, 'MAX_PARENTS', 1)
    offer(p)
    first, second = parent(p), parent(p, call='another')
    append(p, first, second, attachment(p, first))
    await flush(p)
    reader = p.observer.readers[p.tid]
    assert len(reader.parents) == 1 and not p.acks
    reader.offers[0].expires = 0
    await flush(p)
    await p.observer.close()
    assert not p.observer.readers and not p.observer._tasks


async def test_close_one_thread_leaves_other_owned_watcher(proof):
    p = proof
    offer(p)
    other_tid = uid()
    p.server.engine.driver.owners[other_tid] = p.owner
    p.observer.offer(other_tid, p.bridge, 'another', ['other'], 'another context',
                     hook_name='PostToolUse:Read')
    other = p.observer.readers[other_tid]
    await p.observer.close(p.tid)
    assert p.tid not in p.observer.readers
    assert p.observer.readers[other_tid] is other and not other.task.done()
    await p.observer.close()
    assert other.task.done() and not p.observer._tasks


async def test_valid_large_batch_context_exceeds_old_one_mib_limit(proof):
    p = proof
    context = 'large valid batch ' + 'x' * (1024 * 1024 + 100)
    offer(p, ('large-one', 'large-two'), context=context)
    user = parent(p)
    append(p, user, attachment(p, user, context=context))
    for _ in range(8):
        await flush(p)
    assert p.acks == [(p.tid, p.turn, ['large-one', 'large-two'])]


async def test_separate_tool_offers_keep_exact_message_sets(proof):
    p = proof
    offer(p, ('one',), call='call-one', context='first full context')
    offer(p, ('two', 'three'), call='call-two', context='second full context')
    first = parent(p, call='call-one')
    append(p, first, attachment(p, first, call='call-one', context='first full context'))
    await flush(p)
    assert p.acks == [(p.tid, p.turn, ['one'])]
    second = parent(p, call='call-two')
    append(p, second, attachment(p, second, call='call-two', context='second full context'))
    await flush(p)
    assert p.acks[-1] == (p.tid, p.turn, ['two', 'three'])


@pytest.mark.parametrize('hook_name', [None, '', 'Read', 'PreToolUse:Read', 'PostToolUse:', 'PostToolUse:   '])
async def test_invalid_offer_hook_name_fails_closed_without_default(proof, hook_name):
    p = proof
    p.observer.offer(p.tid, p.bridge, 'tool-1', ['first'], 'EXACT returned context',
                     hook_name=hook_name)
    assert not p.observer.readers and not p.observer._tasks and not p.acks


async def test_hook_name_keyword_is_required(proof):
    p = proof
    with pytest.raises(TypeError, match='hook_name'):
        p.observer.offer(p.tid, p.bridge, 'tool-1', ['first'], 'EXACT returned context')
    assert not p.observer.readers and not p.acks


async def test_terminal_flush_drains_complete_proof_larger_than_one_chunk(proof):
    p = proof
    context = 'x' * 300605
    offer(p, ('large-final-proof',), context=context)
    user = parent(p)
    append(p, user, attachment(p, user, context=context))
    assert p.path.stat().st_size > module.MAX_CHUNK_BYTES
    # No watcher tick or second flush: terminal closes immediately afterwards.
    await flush(p)
    await p.observer.close(p.tid)
    assert p.acks == [(p.tid, p.turn, ['large-final-proof'])]


async def test_generation_change_between_flush_chunks_prevents_ack(proof, monkeypatch):
    p = proof
    context = 'x' * 300605
    offer(p, context=context)
    user = parent(p)
    append(p, user, attachment(p, user, context=context))
    first_chunk = asyncio.Event()
    original = p.observer._read

    async def read(reader, **kwargs):
        await original(reader, **kwargs)
        first_chunk.set()

    async def replace_generation():
        await first_chunk.wait()
        p.owner.native_connection_generation += 1

    monkeypatch.setattr(p.observer, '_read', read)
    changer = asyncio.create_task(replace_generation())
    await flush(p)
    await changer
    await p.observer.close(p.tid)
    assert not p.acks and not p.observer.readers


async def test_flush_does_not_chase_proof_appended_after_eof_snapshot(proof, monkeypatch):
    p = proof
    monkeypatch.setattr(module, 'MAX_CHUNK_BYTES', 64)
    offer(p)
    append(p, {'unrelated': 'x' * 300})
    captured_eof = p.path.stat().st_size
    original = p.observer._read
    appended = False

    async def read(reader, **kwargs):
        nonlocal appended
        await original(reader, **kwargs)
        if not appended:
            appended = True
            user = parent(p)
            append(p, user, attachment(p, user))

    monkeypatch.setattr(p.observer, '_read', read)
    await flush(p)
    assert p.observer.readers[p.tid].offset == captured_eof
    assert not p.acks
    await p.observer.close(p.tid)
    assert not p.acks


async def test_flush_byte_cap_leaves_unproven_tail_for_fallback(proof, monkeypatch):
    p = proof
    monkeypatch.setattr(module, 'MAX_CHUNK_BYTES', 64)
    monkeypatch.setattr(module, 'MAX_FLUSH_BYTES', 128)
    offer(p)
    user = parent(p)
    append(p, user, attachment(p, user))
    await flush(p)
    assert p.observer.readers[p.tid].offset == 128
    assert not p.acks
    await p.observer.close(p.tid)
