"""Claude thinking blocks -> Codex v2 `reasoning` items (offline, fake SDK streams)."""
from dataclasses import replace

import pytest

from cute_claude.history_reader import Reader
from cute_claude.model_config import ModelConfig
from test_persistent import ph
from test_subscription import sh

REASONING_METHODS = {'item/reasoning/summaryPartAdded', 'item/reasoning/summaryTextDelta', 'item/reasoning/textDelta'}


def stream(sid, event):
    return {'type': 'stream_event', 'session_id': sid, 'event': event}


def start(sid, index, kind):
    return stream(sid, {'type': 'content_block_start', 'index': index, 'content_block': {'type': kind}})


def delta(sid, index, kind, **body):
    return stream(sid, {'type': 'content_block_delta', 'index': index, 'delta': {'type': kind, **body}})


def stop(sid, index):
    return stream(sid, {'type': 'content_block_stop', 'index': index})


def result(sid):
    return {'type': 'result', 'session_id': sid, 'subtype': 'success', 'is_error': False,
            'terminal_reason': 'completed', 'num_turns': 1, 'usage': {'input_tokens': 1, 'output_tokens': 1}}


async def run(h, script):
    tid = await h.thread()
    async def respond(**kw):
        for message in script(kw['session_id']):
            await kw['on_message'](message)
    h.provider.behavior = respond
    turn = await h.turn(tid, 'think')
    await h.drain()
    return tid, turn


def item_events(h, turn):
    """(method, item type, payload) of every item event of `turn`, in wire order."""
    out = []
    for m in h.out:
        p = m.get('params') or {}
        if p.get('turnId') != turn:
            continue
        if m.get('method') in {'item/started', 'item/completed'}:
            out.append((m['method'], p['item']['type'], p['item']))
        elif m.get('method') in REASONING_METHODS | {'item/agentMessage/delta'}:
            out.append((m['method'], None, p))
    return out


def reasoning_completions(h, turn):
    return [i for m, t, i in item_events(h, turn) if m == 'item/completed' and t == 'reasoning']


async def test_streamed_thinking_is_a_reasoning_item_with_summary_deltas(sh):
    def script(sid):
        return [start(sid, 0, 'thinking'),
                delta(sid, 0, 'thinking_delta', thinking='Plan: '),
                delta(sid, 0, 'thinking_delta', thinking='read the file.'),
                delta(sid, 0, 'signature_delta', signature='sig'),
                stop(sid, 0),
                {'type': 'assistant', 'session_id': sid, 'content': [
                    {'type': 'thinking', 'thinking': 'Plan: read the file.', 'signature': 'sig'}]},
                start(sid, 1, 'text'), delta(sid, 1, 'text_delta', text='Done.'), stop(sid, 1),
                {'type': 'assistant', 'session_id': sid, 'content': [{'type': 'text', 'text': 'Done.'}]},
                result(sid)]
    tid, turn = await run(sh, script)
    events = item_events(sh, turn)
    kinds = [(m, t) for m, t, _ in events if t != 'userMessage']
    assert kinds == [
        ('item/started', 'reasoning'),
        ('item/reasoning/summaryPartAdded', None),
        ('item/reasoning/summaryTextDelta', None),
        ('item/reasoning/summaryTextDelta', None),
        ('item/completed', 'reasoning'),
        ('item/started', 'agentMessage'),
        ('item/agentMessage/delta', None),
        ('item/completed', 'agentMessage'),
        # The terminal result names the last text the final answer, live too.
        ('item/completed', 'agentMessage')]
    texts = [i for m, t, i in events if m == 'item/completed' and t == 'agentMessage']
    assert [i['phase'] for i in texts] == ['commentary', 'final_answer'] and texts[0]['id'] == texts[1]['id']
    started = next(i for m, t, i in events if m == 'item/started' and t == 'reasoning')
    rid = started['id']
    assert started == {'type': 'reasoning', 'id': rid, 'summary': [], 'content': []}
    part = next(p for m, _, p in events if m == 'item/reasoning/summaryPartAdded')
    assert part == {'threadId': tid, 'turnId': turn, 'itemId': rid, 'summaryIndex': 0}
    deltas = [p for m, _, p in events if m == 'item/reasoning/summaryTextDelta']
    assert deltas[0] == {'threadId': tid, 'turnId': turn, 'itemId': rid, 'delta': 'Plan: ', 'summaryIndex': 0}
    [done] = reasoning_completions(sh, turn)
    assert done == {'type': 'reasoning', 'id': rid, 'summary': ['Plan: read the file.'], 'content': []}
    # Stored in wire order: reasoning before the answer.
    assert [i['type'] for i in sh.store.items(turn)][-2:] == ['reasoning', 'agentMessage']


async def test_each_thinking_block_is_its_own_item_in_order_across_model_calls(sh):
    def script(sid):
        out = []
        # Two API calls in one turn; stream indices restart at 0 for each.
        for n in (1, 2):
            out += [start(sid, 0, 'thinking'), delta(sid, 0, 'thinking_delta', thinking=f'thought {n}'), stop(sid, 0),
                    {'type': 'assistant', 'session_id': sid, 'content': [{'type': 'thinking', 'thinking': f'thought {n}', 'signature': 's'}]},
                    start(sid, 1, 'text'), delta(sid, 1, 'text_delta', text=f'step {n}'), stop(sid, 1),
                    {'type': 'assistant', 'session_id': sid, 'content': [{'type': 'text', 'text': f'step {n}'}]}]
        return out + [result(sid)]
    tid, turn = await run(sh, script)
    items = [i for i in sh.store.items(turn) if i['type'] != 'userMessage']
    assert [(i['type'], i.get('summary') or i.get('text')) for i in items] == [
        ('reasoning', ['thought 1']), ('agentMessage', 'step 1'), ('reasoning', ['thought 2']), ('agentMessage', 'step 2')]
    assert len(reasoning_completions(sh, turn)) == 2
    assert len({i['id'] for i in items}) == 4


async def test_omitted_thinking_completes_as_an_empty_reasoning_item(sh):
    # display "omitted": the block streams no text and arrives with thinking="".
    def script(sid):
        return [start(sid, 0, 'thinking'), delta(sid, 0, 'signature_delta', signature='sig'), stop(sid, 0),
                {'type': 'assistant', 'session_id': sid, 'content': [{'type': 'thinking', 'thinking': '', 'signature': 'sig'}]},
                {'type': 'assistant', 'session_id': sid, 'content': [{'type': 'text', 'text': 'ok'}]},
                result(sid)]
    tid, turn = await run(sh, script)
    [done] = reasoning_completions(sh, turn)
    assert done['summary'] == [] and done['content'] == []
    assert not [m for m in sh.out if m.get('method') in REASONING_METHODS]


async def test_redacted_thinking_is_an_empty_reasoning_item_closed_at_block_stop(sh):
    def script(sid):
        # The SDK drops redacted_thinking from AssistantMessage, so only the stream shows it.
        return [start(sid, 0, 'redacted_thinking'), stop(sid, 0),
                start(sid, 1, 'thinking'), delta(sid, 1, 'thinking_delta', thinking='visible'), stop(sid, 1),
                {'type': 'assistant', 'session_id': sid, 'content': [{'type': 'thinking', 'thinking': 'visible', 'signature': 's'}]},
                {'type': 'assistant', 'session_id': sid, 'content': [{'type': 'text', 'text': 'ok'}]},
                result(sid)]
    tid, turn = await run(sh, script)
    redacted, visible = reasoning_completions(sh, turn)
    assert (redacted['summary'], redacted['content']) == ([], [])
    assert visible['summary'] == ['visible']


async def test_thinking_without_stream_events_is_started_and_completed_from_the_final_block(sh):
    def script(sid):
        return [{'type': 'assistant', 'session_id': sid, 'content': [
                    {'type': 'thinking', 'thinking': 'whole thought', 'signature': 's'}, {'type': 'text', 'text': 'answer'}]},
                result(sid)]
    tid, turn = await run(sh, script)
    events = [(m, t) for m, t, _ in item_events(sh, turn) if t == 'reasoning']
    assert events == [('item/started', 'reasoning'), ('item/completed', 'reasoning')]
    assert reasoning_completions(sh, turn)[0]['summary'] == ['whole thought']


async def test_final_block_text_corrects_a_divergent_stream(sh):
    def script(sid):
        return [start(sid, 0, 'thinking'), delta(sid, 0, 'thinking_delta', thinking='partial'), stop(sid, 0),
                {'type': 'assistant', 'session_id': sid, 'content': [{'type': 'thinking', 'thinking': 'partial and more', 'signature': 's'}]},
                {'type': 'assistant', 'session_id': sid, 'content': [{'type': 'text', 'text': 'ok'}]},
                result(sid)]
    tid, turn = await run(sh, script)
    first, corrected = reasoning_completions(sh, turn)
    assert first['id'] == corrected['id'] and corrected['summary'] == ['partial and more']
    [stored] = [i for i in sh.store.items(turn) if i['type'] == 'reasoning']
    assert stored['summary'] == ['partial and more']


async def test_interrupted_turn_completes_an_open_reasoning_item(sh):
    def script(sid):
        return [start(sid, 0, 'thinking'), delta(sid, 0, 'thinking_delta', thinking='half')]
    tid, turn = await run(sh, script)  # No terminal result: the turn fails.
    [done] = reasoning_completions(sh, turn)
    assert done['summary'] == ['half']
    assert sh.store.turn(turn)['status'] == 'failed'


async def test_reasoning_items_survive_reload_live_and_offline(sh):
    def script(sid):
        return [start(sid, 0, 'thinking'), delta(sid, 0, 'thinking_delta', thinking='why'), stop(sid, 0),
                {'type': 'assistant', 'session_id': sid, 'content': [{'type': 'thinking', 'thinking': 'why', 'signature': 's'}]},
                {'type': 'assistant', 'session_id': sid, 'content': [{'type': 'text', 'text': 'because'}]},
                result(sid)]
    tid, turn = await run(sh, script)
    expected = [i['type'] for i in sh.store.items(turn)]
    assert expected[-2:] == ['reasoning', 'agentMessage']
    live_read = await sh.call('thread/read', {'threadId': tid, 'includeTurns': True})
    assert [i['type'] for i in live_read['thread']['turns'][-1]['items']] == expected
    page = await sh.call('thread/turns/list', {'threadId': tid, 'itemsView': 'full'})
    [reasoning] = [i for i in page['data'][0]['items'] if i['type'] == 'reasoning']
    assert reasoning['summary'] == ['why'] and reasoning['content'] == []
    reader = Reader(sh.settings.state_dir)
    try:
        offline = reader.respond({'id': 1, 'method': 'thread/items/list', 'params': {'threadId': tid}})['result']
        assert [x['item'] for x in offline['data'] if x['item']['type'] == 'reasoning'] == [reasoning]
    finally:
        reader.db.close()


# ---- thinking is requested from the official runtime --------------------------

async def test_thinking_defaults_to_adaptive_with_summaries(ph):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    assert ph.sdk.clients[0].options.thinking == {'type': 'adaptive', 'display': 'summarized'}


@pytest.mark.parametrize('thinking,display,expected', [
    ('disabled', 'summarized', {'type': 'disabled'}),
    ('adaptive', 'omitted', {'type': 'adaptive', 'display': 'omitted'})])
async def test_thinking_settings_reach_the_sdk(ph, thinking, display, expected):
    ph.engine.settings = replace(ph.settings, thinking=thinking, thinking_display=display)
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    assert ph.sdk.clients[0].options.thinking == expected


async def test_config_reasoning_summary_overrides_display(ph):
    assert (await ph.call('config/read'))['config']['model_reasoning_summary'] is None
    edit = lambda value: [{'keyPath': 'model_reasoning_summary', 'value': value, 'mergeStrategy': 'replace'}]
    await ph.call('config/batchWrite', {'edits': edit('none')})
    assert (await ph.call('config/read'))['config']['model_reasoning_summary'] == 'none'
    assert ModelConfig(ph.settings).thinking() == {'type': 'adaptive', 'display': 'omitted'}
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    assert ph.sdk.clients[0].options.thinking['display'] == 'omitted'
    await ph.call('config/batchWrite', {'edits': edit('detailed')})
    assert ModelConfig(replace(ph.settings, thinking_display='omitted')).thinking()['display'] == 'summarized'
    await ph.call('config/batchWrite', {'edits': edit(None)})
    assert ModelConfig(ph.settings).reasoning_summary() is None
    bad = await ph.call('config/batchWrite', {'edits': edit('verbose')}, error=True)
    assert bad['code'] == -32602


def test_settings_reject_unknown_thinking_values(tmp_path):
    from cute_claude.config import Settings
    base = Settings(tmp_path, tmp_path / 'state', model='opus', mock=False, backend='subscription')
    base.validate()
    for bad in ({'thinking': 'enabled'}, {'thinking_display': 'updates'}):
        with pytest.raises(ValueError):
            replace(base, **bad).validate()


async def test_interim_text_between_tool_calls_is_commentary_never_reasoning(sh):
    # Owner report 2026-10-21: a Claude turn's work group showed short user-facing lines as 推理 · 0s.
    # Interleaved thinking: each model call is thinking -> text -> tool_use. Only thinking blocks
    # become `reasoning`; the interim text is a commentary `agentMessage`, streamed or not.
    def script(sid):
        out = []
        for n in (1, 2):
            tool = {'type': 'tool_use', 'id': f'toolu_{n}', 'name': 'Bash', 'input': {'command': f'ls {n}'}}
            out += [start(sid, 0, 'thinking'), delta(sid, 0, 'thinking_delta', thinking=f'English thought {n}'), stop(sid, 0),
                    {'type': 'assistant', 'session_id': sid, 'content': [{'type': 'thinking', 'thinking': f'English thought {n}', 'signature': 's'}]},
                    start(sid, 1, 'text'), delta(sid, 1, 'text_delta', text=f'我先看看第 {n} 个文件。'), stop(sid, 1),
                    {'type': 'assistant', 'session_id': sid, 'content': [{'type': 'text', 'text': f'我先看看第 {n} 个文件。'}]},
                    {'type': 'assistant', 'session_id': sid, 'content': [tool]}]
        # A last call without stream events (thinking + text in one message), then the answer.
        out += [{'type': 'assistant', 'session_id': sid, 'content': [
                    {'type': 'thinking', 'thinking': 'English thought 3', 'signature': 's'},
                    {'type': 'text', 'text': '设定文件都很短。'}]},
                {'type': 'assistant', 'session_id': sid, 'content': [{'type': 'text', 'text': '完成。'}]},
                result(sid)]
        return out
    tid, turn = await run(sh, script)
    items = [i for i in sh.store.items(turn) if i['type'] != 'userMessage']
    assert [(i['type'], i.get('summary') or i.get('text'), i.get('phase')) for i in items] == [
        ('reasoning', ['English thought 1'], None), ('agentMessage', '我先看看第 1 个文件。', 'commentary'),
        ('reasoning', ['English thought 2'], None), ('agentMessage', '我先看看第 2 个文件。', 'commentary'),
        ('reasoning', ['English thought 3'], None), ('agentMessage', '设定文件都很短。', 'commentary'),
        ('agentMessage', '完成。', 'final_answer')]
    # On the wire too: no text ever reaches a reasoning item or a reasoning delta.
    texts = {'我先看看第 1 个文件。', '我先看看第 2 个文件。', '设定文件都很短。', '完成。'}
    for method, kind, payload in item_events(sh, turn):
        if kind == 'reasoning':
            assert not set(payload['summary']) & texts and payload['content'] == []
        elif method in REASONING_METHODS:
            assert payload.get('delta') not in texts
