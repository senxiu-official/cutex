"""Atomic owner input prefix; every entry remains an original user message."""
from .errors import require
from .inputs import parse_turn_input
from .store import dump


def parse_owner_prefix(server, tid, params):
    if 'ownerInputs' not in params:
        return None
    require(server.settings.backend == 'subscription' and server.external_inputs.enabled,
            'ownerInputs requires bound subscription runtime', -32011)
    require('ownerCarries' not in params, 'ownerInputs and external ownerCarries cannot be combined')
    values = params['ownerInputs']
    client = params.get('clientUserMessageId')
    require(isinstance(client, str) and client.strip() and len(client.encode('utf-8')) <= 256,
            'ownerInputs requires carrier clientUserMessageId')
    require(isinstance(values, list) and len(values) < 100, 'ownerInputs accepts at most 99 prefix inputs')
    require(len(dump({'prefix': values, 'input': params.get('input')}).encode('utf-8')) <= 1024 * 1024,
            'owner input batch exceeds 1 MiB')
    seen = {client}
    result = []
    for value in values:
        require(isinstance(value, dict) and set(value) == {'clientUserMessageId', 'input'}, 'invalid ownerInputs entry')
        mid = value['clientUserMessageId']
        require(isinstance(mid, str) and mid.strip() and len(mid.encode('utf-8')) <= 256 and mid not in seen,
                'duplicate or invalid owner input identity')
        seen.add(mid)
        text, attachments, display = parse_turn_input(value['input'], images=True,
            skills=server.skills, cwd=server.store.thread(tid)['cwd'])
        result.append({'source': {'kind': 'user', 'id': 'local-controller'}, 'eventType': 'user.message',
            'clientId': mid, 'text': text, 'attachments': attachments, 'display': display, 'metadata': {}})
    return result
