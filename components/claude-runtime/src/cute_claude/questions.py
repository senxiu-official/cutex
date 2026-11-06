"""Translate native AskUserQuestion to the existing app-server question UI."""
from __future__ import annotations

import json

from .errors import require

MAX_TEXT_BYTES = 65536
MAX_ANSWER_BYTES = 262144
# Sixteen previews plus repeated legacy text: bound the whole request too.
MAX_QUESTION_BYTES = 1048576


def _bounded(value, limit=MAX_QUESTION_BYTES) -> bool:
    """Bound JSON transport bytes, including legacy/extension duplication."""
    try:
        return len(json.dumps(value, ensure_ascii=False).encode('utf-8')) <= limit
    except (TypeError, ValueError, UnicodeError):
        return False


def _text_bytes(value) -> int:
    try:
        return len(value.encode('utf-8')) if isinstance(value, str) else MAX_TEXT_BYTES + 1
    except UnicodeError:
        return MAX_TEXT_BYTES + 1


def questions_wire(arguments: dict) -> list[dict]:
    require(isinstance(arguments, dict), 'Invalid question arguments')
    questions = arguments.get('questions')
    require(isinstance(questions, list) and 1 <= len(questions) <= 4, 'Expected 1–4 questions')
    result = []
    for index, question in enumerate(questions):
        require(isinstance(question, dict), 'Invalid question')
        title, header = question.get('question'), question.get('header')
        require(isinstance(title, str) and bool(title.strip()) and isinstance(header, str)
                and bool(header.strip()), 'Invalid question text')
        require('multiSelect' not in question or type(question['multiSelect']) is bool,
                'multiSelect must be a boolean')
        options = question.get('options')
        require(isinstance(options, list) and 2 <= len(options) <= 4, 'Expected 2–4 options')
        require(all(isinstance(o, dict) and isinstance(o.get('label'), str) and o['label'].strip()
                    and isinstance(o.get('description'), str) for o in options), 'Invalid options')
        require(all(o['label'] == o['label'].strip() for o in options), 'Option labels must not have surrounding whitespace')
        require(len({o['label'].strip() for o in options}) == len(options), 'Option labels must be distinct')
        require(all(o.get('preview') is None or isinstance(o['preview'], str) for o in options),
                'Option preview must be a string or null')
        require(all(_text_bytes(value) <= MAX_TEXT_BYTES for value in [title, header] +
                    [o[k] for o in options for k in ('label', 'description', 'preview')
                     if k in o and o[k] is not None]),
                'Question text or option exceeds 65536 UTF-8 bytes')
        # The native SDK accepts preview:null; the wire extension only accepts
        # text when present. Project it away without changing SDK arguments.
        extension = {'version': 1, 'question': title, 'multiSelect': question.get('multiSelect', False),
                     'options': [{k: o[k] for k in ('label', 'description', 'preview')
                                  if k in o and o[k] is not None}
                                 for o in options]}
        # Older clients only support single-select. Keep their multi-select
        # fallback in free text, without discarding any option.
        if question.get('multiSelect'):
            title += '\n' + '\n'.join(f"- {o['label']}: {o['description']}" for o in options)
            title += '\nEnter one or more labels separated by commas, or your own answer.'
            choices = None
        else:
            choices = [{'label': o['label'], 'description': o['description']} for o in options]
        result.append({'id': f'q{index}', 'header': header, 'question': title,
                       'isOther': True, 'isSecret': False, 'options': choices,
                       'cutexQuestionOptions': extension})
    require(len({q['question'].strip() for q in questions}) == len(questions), 'Question text must be distinct')
    require(_bounded(result), 'Questions exceed 1048576 transport bytes')
    return result


def _validated_answers(result, questions: list[dict]) -> list[list[str]] | None:
    """Validate selections, legacy notes and intentional unanswered questions."""
    answers = result.get('answers') if isinstance(result, dict) and set(result) == {'answers'} else None
    if not isinstance(answers, dict):
        return None
    if not answers:
        return []  # Legacy TUI auto-resolution: no questions were answered.
    if set(answers) != {f'q{index}' for index in range(len(questions))}:
        return None
    output = []
    total_bytes = 0
    for index, question in enumerate(questions):
        answer = answers.get(f'q{index}')
        values = answer.get('answers') if isinstance(answer, dict) and set(answer) == {'answers'} else None
        multi = question.get('multiSelect', False)
        if not isinstance(values, list) or len(values) > (len(question['options']) + 2 if multi else 2):
            return None
        if not all(isinstance(v, str) and v.strip() for v in values):
            return None
        # Bound submitted bytes before trimming, then use normalized values for
        # duplicate detection and the SDK mapping. Labels are already clean.
        sizes = [_text_bytes(v) for v in values]
        total_bytes += sum(sizes)
        if any(size > MAX_TEXT_BYTES for size in sizes) or total_bytes > MAX_ANSWER_BYTES:
            return None
        normalized = [value.strip() for value in values]
        if len(set(normalized)) != len(normalized):
            return None
        labels = {o['label'] for o in question['options']}
        selections = custom = notes = 0
        for raw, value in zip(values, normalized):
            if value in labels:
                selections += 1  # Offered labels win over the reserved prefix.
            elif raw.lstrip().startswith('user_note: '):
                if not value[len('user_note: '):].strip():
                    return None
                notes += 1
            else:
                custom += 1
                selections += 1
        if notes > 1 or custom > 1 or selections > (len(labels) + 1 if multi else 1):
            return None
        output.append(normalized)
    return output


def valid_no_answer(result, questions: list[dict]) -> bool:
    """An intentional legacy no-answer, never a malformed/missing result."""
    values = _validated_answers(result, questions)
    return values is not None and (not values or any(not answer for answer in values))


def read_answers(result, questions: list[dict]):
    values = _validated_answers(result, questions)
    if values is None or not values or any(not answer for answer in values):
        return None
    return {question['question']: ', '.join(answer) for question, answer in zip(questions, values)}
