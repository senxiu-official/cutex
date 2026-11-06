"""Question extension, legacy transport and first-valid-answer arbitration (offline)."""
import asyncio
from copy import deepcopy
import json
from pathlib import Path
from types import SimpleNamespace

import pytest

from cute_claude.errors import RpcError
from cute_claude.questions import MAX_TEXT_BYTES, MAX_ANSWER_BYTES, questions_wire, read_answers, valid_no_answer
from cute_claude.store import identifier
from test_persistent import ph
from test_questions_plan import QUESTIONS, begin


def arguments(multi=False):
    args = deepcopy(QUESTIONS)
    args['questions'][0]['multiSelect'] = multi
    return args


def reply(*values):
    return {'answers': {'q0': {'answers': list(values)}}}


@pytest.mark.parametrize('case', json.loads(
    (Path(__file__).parent / 'fixtures' / 'question_answer_contract.json').read_text(encoding='utf-8')),
    ids=lambda case: case['name'])
def test_shared_question_answer_contract(case):
    questions = [{'question': 'Which platform?', 'header': 'Platform', 'multiSelect': case['multiSelect'],
                  'options': [{'label': 'Windows', 'description': 'PC'},
                              {'label': 'Linux', 'description': 'Server'}]}]
    assert questions_wire({'questions': questions})[0]['isOther'] is True
    result = {'answers': case['answers']}
    no_answer = valid_no_answer(result, questions)
    assert (read_answers(result, questions) is not None or no_answer) == case['accepted']
    assert no_answer == case['noAnswer']


@pytest.mark.parametrize('multi', [False, True])
def test_extension_and_legacy_fields_are_independent_and_verbatim(multi):
    args = arguments(multi)
    preview = '  <script>alert("hi")</script>\n```html\n<b>text</b>\n```  '
    args['questions'][0]['options'][0]['preview'] = preview
    original = deepcopy(args)
    [wire] = questions_wire(args)
    extension = wire.pop('cutexQuestionOptions')
    assert args == original
    assert extension == {'version': 1, 'question': 'Which color?', 'multiSelect': multi,
                         'options': args['questions'][0]['options']}
    assert extension['options'][0]['preview'] == preview
    title = ('Which color?\n- Pink: Theme pink\n- Blue: Theme blue\n'
             'Enter one or more labels separated by commas, or your own answer.') if multi else 'Which color?'
    assert wire == {'id': 'q0', 'header': 'Color', 'question': title, 'isOther': True, 'isSecret': False,
                    'options': None if multi else QUESTIONS['questions'][0]['options']}


def test_omitted_multi_defaults_to_false_and_synthetic_ids_ignore_native_id():
    args = arguments()
    del args['questions'][0]['multiSelect']
    args['questions'][0]['id'] = 'native'
    [wire] = questions_wire(args)
    assert wire['id'] == 'q0'
    assert wire['cutexQuestionOptions']['multiSelect'] is False


@pytest.mark.parametrize('multi', [False, True])
def test_native_null_preview_is_absent_in_extension_without_mutating_arguments(multi):
    args = arguments(multi)
    args['questions'][0]['options'][0]['preview'] = None
    args['questions'][0]['options'][1]['preview'] = ''
    original = deepcopy(args)
    [wire] = questions_wire(args)
    assert args == original
    assert wire['cutexQuestionOptions']['options'] == [
        {'label': 'Pink', 'description': 'Theme pink'},
        {'label': 'Blue', 'description': 'Theme blue', 'preview': ''}]
    del wire['cutexQuestionOptions']
    [legacy] = questions_wire(arguments(multi))
    del legacy['cutexQuestionOptions']
    assert wire == legacy


@pytest.mark.parametrize('value', [None, 0, 1, 'false', [], {}])
def test_multi_select_requires_boolean(value):
    args = arguments()
    args['questions'][0]['multiSelect'] = value
    with pytest.raises(RpcError):
        questions_wire(args)


@pytest.mark.parametrize('field,value', [('question', None), ('question', ''), ('question', '  '),
                                         ('header', None), ('header', ''), ('header', '  '),
                                         ('options', None), ('options', []), ('options', [{}]),
                                         ('options', [None, None])])
def test_invalid_question_fields(field, value):
    args = arguments()
    args['questions'][0][field] = value
    with pytest.raises(RpcError):
        questions_wire(args)


@pytest.mark.parametrize('field,value', [('label', None), ('label', ''), ('label', ' '), ('label', ' Pink'),
                                         ('label', 'Pink '),
                                         ('description', None), ('preview', {}),
                                         ('preview', 42)])
def test_invalid_option_fields(field, value):
    args = arguments()
    args['questions'][0]['options'][0][field] = value
    with pytest.raises(RpcError):
        questions_wire(args)


def test_missing_and_duplicate_questions_and_options():
    bad = [None, {}, {'questions': None}, {'questions': []}, {'questions': [None]},
           {'questions': QUESTIONS['questions'] * 2}, {'questions': QUESTIONS['questions'] * 5}]
    for field in ('question', 'header', 'options'):
        args = arguments()
        del args['questions'][0][field]
        bad.append(args)
    args = arguments()
    args['questions'][0]['options'][1]['label'] = 'Pink'
    bad.append(args)
    args = arguments()
    args['questions'].append({**deepcopy(args['questions'][0]), 'question': ' Which color? '})
    bad.append(args)
    args = arguments()
    args['questions'][0]['options'] *= 3
    bad.append(args)
    for field in ('label', 'description'):
        args = arguments()
        del args['questions'][0]['options'][0][field]
        bad.append(args)
    for args in bad:
        with pytest.raises(RpcError):
            questions_wire(args)


@pytest.mark.parametrize('response', [None, {}, {'answers': {}}, {'answers': []},
    {'answers': {'q0': {'answers': ['Pink'], 'extra': True}}},
    {'answers': {'q0': {'answers': ['Pink']}}, 'extra': True},
    {'answers': {'q1': {'answers': ['Pink']}}},
    {'answers': {'q0': {'answers': ['Pink']}, 'extra': {'answers': ['Blue']}}},
    {'answers': {'q0': None}}, {'answers': {'q0': {}}},
    {'answers': {'q0': {'answers': 'Pink'}}},
    reply(), reply(''), reply(' '), reply(None), reply(7), reply('Pink', 'Pink'), reply('Pink', 'Blue')])
def test_strict_single_answers(response):
    assert read_answers(response, arguments()['questions']) is None


def test_multi_answers_and_other_keep_native_string_mapping():
    questions = arguments(True)['questions']
    for values in [('Pink',), ('Pink', 'Blue'), ('a custom color',), ('Pink', 'Blue', '  custom, color  '),
                   ('Pink, Blue',), ('Blue, Pink',)]:
        assert read_answers(reply(*values), questions) == {'Which color?': ', '.join(v.strip() for v in values)}
    for values in [('Pink', 'Pink'), ('Pink', ' Pink '), ('Pink', 'Blue', 'other', 'extra'), ('other', 'another'), (' ',)]:
        assert read_answers(reply(*values), questions) is None
    assert read_answers(reply('  custom color  '), arguments()['questions']) == {'Which color?': 'custom color'}


@pytest.mark.parametrize('multi,values', [
    (False, ['Blue', 'user_note: explain']),
    (False, ['user_note: explain']),
    (False, ['custom', 'user_note: explain']),
    (True, ['Pink', 'Blue', 'custom', 'user_note: explain']),
    (True, ['Blue, Pink', 'user_note: explain']),
    (True, ['user_note: explain', 'Pink', 'Blue']),
])
def test_legacy_user_note_is_one_extra_value_preserved_in_native_mapping(multi, values):
    questions = arguments(multi)['questions']
    assert read_answers(reply(*values), questions) == {'Which color?': ', '.join(values)}
    assert not valid_no_answer(reply(*values), questions)


@pytest.mark.parametrize('values', [
    ['Blue', 'Pink', 'user_note: explain'],
    ['Blue', 'user_note: first', 'user_note: second'],
    ['user_note: first', 'user_note: second'],
    ['Blue', 'user_note: '], ['user_note: \t\x1c'],
    ['Blue', 'user_note:nospace'], ['user_note: '],
    ['user_note: explain', ' user_note: explain '],
    ['Blue', 'user_note: ' + 'x' * MAX_TEXT_BYTES],
])
def test_invalid_legacy_notes_remain_invalid_not_no_answer(values):
    questions = arguments()['questions']
    assert read_answers(reply(*values), questions) is None
    assert not valid_no_answer(reply(*values), questions)


@pytest.mark.parametrize('values,valid', [
    (['Blue', 'user_note:nospace'], True),
    (['user_note:nospace'], True),
    (['Pink', 'Blue', 'user_note:nospace', 'user_note: explanation'], True),
    (['Blue', 'user_note:nospace', 'another custom'], False),
])
def test_multi_nospace_note_prefix_is_one_other_not_an_extra_note(values, valid):
    questions = arguments(True)['questions']
    assert read_answers(reply(*values), questions) == ({'Which color?': ', '.join(values)} if valid else None)
    assert not valid_no_answer(reply(*values), questions)


def test_offered_label_wins_over_note_prefix():
    args = arguments()
    args['questions'][0]['options'][0]['label'] = 'user_note: option'
    questions_wire(args)
    questions = args['questions']
    assert read_answers(reply('user_note: option', 'user_note: explain'), questions) == {
        'Which color?': 'user_note: option, user_note: explain'}
    assert read_answers(reply('user_note: option', 'Blue'), questions) is None
    assert not valid_no_answer(reply('user_note: option', 'Blue'), questions)


def test_legacy_no_answer_requires_valid_complete_shape_and_valid_other_answers():
    args = arguments()
    args['questions'].append({**deepcopy(args['questions'][0]), 'question': 'Second?'})
    questions = args['questions']
    for response in ({'answers': {}}, {'answers': {'q0': {'answers': []}, 'q1': {'answers': ['Blue']}}},
                     {'answers': {'q0': {'answers': ['user_note: explain']}, 'q1': {'answers': []}}}):
        assert read_answers(response, questions) is None
        assert valid_no_answer(response, questions)
    for response in (None, {}, {'answers': {'q0': {'answers': []}}},
                     {'answers': {'unknown': {'answers': []}, 'q1': {'answers': []}}},
                     {'answers': {'q0': {'answers': []}, 'q1': {'answers': ['Blue', 'Pink']}}},
                     {'answers': {'q0': {'answers': []}, 'q1': {'answers': ['user_note: ']}}},
                     {'answers': {}, 'extra': True}):
        assert read_answers(response, questions) is None
        assert not valid_no_answer(response, questions)


def test_multiple_questions_require_exact_ids_and_preserve_each_native_key():
    args = arguments()
    args['questions'].append({**deepcopy(args['questions'][0]), 'question': 'Which second color?', 'multiSelect': True})
    assert [q['id'] for q in questions_wire(args)] == ['q0', 'q1']
    response = reply('Pink')
    assert read_answers(response, args['questions']) is None
    response['answers']['q1'] = {'answers': ['Pink', 'Blue']}
    assert read_answers(response, args['questions']) == {'Which color?': 'Pink', 'Which second color?': 'Pink, Blue'}


def test_question_text_and_preview_utf8_limits():
    args = arguments(True)
    args['questions'][0]['options'][0]['preview'] = 'x' * MAX_TEXT_BYTES
    assert questions_wire(args)[0]['cutexQuestionOptions']['options'][0]['preview'] == 'x' * MAX_TEXT_BYTES
    args['questions'][0]['options'][0]['preview'] += 'x'
    with pytest.raises(RpcError):
        questions_wire(args)
    args['questions'][0]['options'][0]['preview'] = '界' * 22000
    with pytest.raises(RpcError):
        questions_wire(args)
    args = arguments(True)
    args['questions'][0]['options'][0]['description'] = 'x' * (MAX_TEXT_BYTES + 1)
    with pytest.raises(RpcError):
        questions_wire(args)


def test_total_question_transport_is_bounded_even_when_each_preview_fits():
    args = arguments(True)
    template = args['questions'][0]
    template['options'] = [{'label': str(i), 'description': '', 'preview': 'x' * MAX_TEXT_BYTES}
                           for i in range(4)]
    args['questions'] = [{**deepcopy(template), 'question': f'Question {i}'} for i in range(4)]
    with pytest.raises(RpcError, match='transport bytes'):
        questions_wire(args)


def test_four_options_plus_one_other_is_the_multi_answer_maximum():
    args = arguments(True)
    args['questions'][0]['options'] += [{'label': 'Red', 'description': ''}, {'label': 'Green', 'description': ''}]
    questions_wire(args)
    assert read_answers(reply('Pink', 'Blue', 'Red', 'Green', 'custom'), args['questions']) == {
        'Which color?': 'Pink, Blue, Red, Green, custom'}
    assert read_answers(reply('Pink', 'Blue', 'Red', 'Green', 'custom', 'extra'), args['questions']) is None


def test_answer_utf8_limits_per_value_and_total():
    assert read_answers(reply('x' * MAX_TEXT_BYTES), arguments()['questions']) is not None
    assert read_answers(reply('x' * (MAX_TEXT_BYTES + 1)), arguments()['questions']) is None
    assert read_answers(reply('界' * 22000), arguments()['questions']) is None
    args = arguments()
    args['questions'] = [{**deepcopy(args['questions'][0]), 'question': f'Question {i}'} for i in range(4)]
    response = {'answers': {f'q{i}': {'answers': ['x' * MAX_TEXT_BYTES]} for i in range(4)}}
    assert sum(len(v['answers'][0]) for v in response['answers'].values()) == MAX_ANSWER_BYTES
    assert read_answers(response, args['questions']) is not None
    # All individual values fit, but adding a selected label exceeds the total.
    args['questions'][0]['multiSelect'] = True
    response['answers']['q0']['answers'].append('Pink')
    assert read_answers(response, args['questions']) is None


@pytest.mark.parametrize('same_peer', [False, True])
@pytest.mark.parametrize('native_preview', [None, '<b>literal</b>'])
async def test_invalid_first_answer_does_not_resolve_and_valid_second_wins(ph, same_peer, native_preview):
    tid, turn, bridge = await begin(ph)
    other_out = []

    async def send(message):
        other_out.append(deepcopy(message))

    other = ph.server.connect(send)
    other.ready = other.interactive = True
    other.subscriptions.add(tid)
    args = arguments(True)
    args['questions'][0]['options'][0]['preview'] = native_preview
    original = deepcopy(args)
    call = identifier()
    await bridge.pre_tool({'session_id': bridge.sid, 'tool_name': 'AskUserQuestion', 'tool_input': args}, call, {})
    invoke = ph.sdk.clients[0].options.can_use_tool
    task = asyncio.create_task(invoke('AskUserQuestion', args, SimpleNamespace(tool_use_id=call)))
    try:
        async with asyncio.timeout(2):
            while not ph.server.approvals.pending:
                await asyncio.sleep(.001)
        req = next(x for x in ph.out if x.get('method') == 'item/tool/requestUserInput')
        assert other_out[-1] == req
        wire_option = req['params']['questions'][0]['cutexQuestionOptions']['options'][0]
        if native_preview is None:
            assert 'preview' not in wire_option
        else:
            assert wire_option['preview'] == native_preview
        pending = ph.server.approvals.pending[req['id']]
        for malformed in ({'result': None}, {}, {'result': []}, {'result': 'invalid'}):
            await ph.server.handle(ph.peer, {'id': req['id'], **malformed})
            await asyncio.sleep(0)
            assert ph.server.approvals.pending[req['id']] is pending
            assert not task.done() and not pending.future.done()
            assert not any(x.get('method') == 'serverRequest/resolved' for x in ph.out + other_out)
            assert ph.peer in pending.peers and other in pending.peers
        for invalid in (reply('Pink', 'Pink'), reply('Blue', 'user_note: '),
                        reply('user_note: first', 'user_note: second'),
                        {'answers': {'unknown': {'answers': []}}}):
            await ph.server.handle(ph.peer, {'id': req['id'], 'result': invalid})
            await asyncio.sleep(0)
            assert not task.done() and not pending.future.done()
            assert not any(x.get('method') == 'serverRequest/resolved' for x in ph.out + other_out)
            assert ph.peer in pending.peers and other in pending.peers
        await ph.server.handle(ph.peer if same_peer else other, {'id': req['id'], 'result': reply('Pink', 'Blue', 'custom')})
        result = await asyncio.wait_for(task, 2)
        assert result.behavior == 'allow'
        assert result.updated_input['questions'] == original['questions']
        assert result.updated_input['answers'] == {'Which color?': 'Pink, Blue, custom'}
        assert args == original
        assert not ph.server.approvals.pending
        assert len([x for x in ph.out if x.get('method') == 'serverRequest/resolved']) == 1
    finally:
        if not task.done():
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)


@pytest.mark.parametrize('values,multi', [(['Blue', 'user_note: explain'], False),
                                        (['user_note: explain'], False),
                                        (['Pink', 'Blue', 'custom', 'user_note: explain'], True),
                                        ([], False), (None, False)])
async def test_legacy_response_reaches_sdk_and_resolves(ph, values, multi):
    tid, turn, bridge = await begin(ph)
    args, call = arguments(multi), identifier()
    await bridge.pre_tool({'session_id': bridge.sid, 'tool_name': 'AskUserQuestion', 'tool_input': args}, call, {})
    task = asyncio.create_task(ph.sdk.clients[0].options.can_use_tool(
        'AskUserQuestion', args, SimpleNamespace(tool_use_id=call)))
    try:
        async with asyncio.timeout(2):
            while not ph.server.approvals.pending:
                await asyncio.sleep(.001)
        [rid] = ph.server.approvals.pending
        await ph.server.handle(ph.peer, {'id': rid, 'result': reply(*values) if values is not None else {'answers': {}}})
        result = await asyncio.wait_for(task, 2)
        assert result.behavior == ('allow' if values else 'deny')
        if values:
            assert result.updated_input['answers'] == {'Which color?': ', '.join(values)}
        assert not ph.server.approvals.pending
        assert any(x.get('method') == 'serverRequest/resolved' for x in ph.out)
    finally:
        if not task.done():
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)


@pytest.mark.parametrize('message', [{'error': {'code': -1, 'message': 'cancel'}},
                                   {'result': None, 'error': {'code': -1, 'message': 'cancel'}},
                                   {'result': {'answers': {}}}, {'result': reply()}])
async def test_explicit_cancel_or_error_still_denies_and_cleans_up(ph, message):
    tid, turn, _ = await begin(ph)
    task = asyncio.create_task(ph.server.approvals.ask(tid, turn, arguments(), identifier()))
    try:
        async with asyncio.timeout(2):
            while not ph.server.approvals.pending:
                await asyncio.sleep(.001)
        [rid] = ph.server.approvals.pending
        await ph.server.handle(ph.peer, {'id': rid, **message})
        assert await asyncio.wait_for(task, 2) is None
        assert not ph.server.approvals.pending
        assert any(x.get('method') == 'serverRequest/resolved' for x in ph.out)
        assert next(i for i in ph.store.items(turn) if i.get('tool') == 'AskUserQuestion')['success'] is False
    finally:
        if not task.done():
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)
