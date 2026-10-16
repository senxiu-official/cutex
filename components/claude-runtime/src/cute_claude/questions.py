"""Translate native AskUserQuestion to the existing app-server question UI."""
from __future__ import annotations

from .errors import require


def questions_wire(arguments: dict) -> list[dict]:
    questions = arguments.get('questions')
    require(isinstance(questions, list) and 1 <= len(questions) <= 4, 'Expected 1–4 questions')
    result = []
    for index, question in enumerate(questions):
        require(isinstance(question, dict), 'Invalid question')
        title, header = question.get('question'), question.get('header')
        require(isinstance(title, str) and bool(title.strip()) and isinstance(header, str), 'Invalid question text')
        options = question.get('options')
        require(isinstance(options, list) and 2 <= len(options) <= 4, 'Expected 2–4 options')
        require(all(isinstance(o, dict) and isinstance(o.get('label'), str) and o['label'].strip()
                    and isinstance(o.get('description'), str) for o in options), 'Invalid options')
        # The current native Codex question UI is single-select. Keep multi-select
        # honest using its free-text field, without discarding any option.
        if question.get('multiSelect'):
            title += '\n' + '\n'.join(f"- {o['label']}: {o['description']}" for o in options)
            title += '\nEnter one or more labels separated by commas, or your own answer.'
            choices = None
        else:
            choices = [{'label': o['label'], 'description': o['description']} for o in options]
        result.append({'id': f'q{index}', 'header': header, 'question': title,
                       'isOther': True, 'isSecret': False, 'options': choices})
    require(len({q['question'] for q in questions}) == len(questions), 'Question text must be distinct')
    return result


def read_answers(result, questions: list[dict]):
    answers = result.get('answers') if isinstance(result, dict) else None
    if not isinstance(answers, dict):
        return None
    output = {}
    for index, question in enumerate(questions):
        answer = answers.get(f'q{index}')
        values = answer.get('answers') if isinstance(answer, dict) else None
        if not isinstance(values, list) or not values or not all(isinstance(v, str) and v.strip() for v in values):
            return None
        output[question['question']] = ', '.join(values)
    return output
