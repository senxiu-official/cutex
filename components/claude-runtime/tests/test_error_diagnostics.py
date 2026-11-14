import json

import pytest

from cute_claude.subscription import SdkResponseError
from test_persistent import ph


@pytest.mark.parametrize('code', ['ECONNRESET', 'ENOTFOUND', 'ETIMEDOUT'])
def test_network_error_is_not_a_confirmed_http_500(code):
    e = SdkResponseError({'error': 'server_error', 'content': [
        {'text': f'API Error: Connection failed ({code}). private prompt, sk-ant-private'}]}, None)
    assert e.codex_error_info == {'httpConnectionFailed': {'httpStatusCode': None}}
    assert str(e).startswith(f'Official Claude connection failed ({code}).')
    assert json.loads(e.additional_details) == {'category': code, 'httpStatusCode': None, 'requestId': None}
    assert 'private' not in str(e) + e.additional_details


@pytest.mark.parametrize('status,info', [(500, 'internalServerError'), (503, 'internalServerError'),
                                      (529, 'serverOverloaded'), (401, 'unauthorized'),
                                      (400, 'badRequest'), (429, 'usageLimitExceeded')])
def test_verified_status_and_safe_structured_request_id(status, info):
    rid = 'req_011CW1234567890abcdef'
    e = SdkResponseError({'error': 'server_error', 'request_id': rid,
                         'content': [{'text': f'API Error: {status} private body'}]}, None)
    assert e.codex_error_info == info
    assert json.loads(e.additional_details) == {
        'category': 'overloaded' if status == 529 else 'http_error',
        'httpStatusCode': status, 'requestId': rid}
    assert f'HTTP status: {status}' in str(e)
    assert 'private' not in str(e) + e.additional_details


@pytest.mark.parametrize('text', ['prompt mentions HTTP 500 and ECONNRESET',
                                  '500 tokens remaining', '529 tokens remaining',
                                  'private body 529 secret', 'The server was overloaded',
                                  'API Error: 5000 is a private request value',
                                  'API Error: Connection failed (ECONNRESET_SECRET)', 'unknown'])
def test_incidental_or_unknown_text_does_not_assert_server_500(text):
    e = SdkResponseError({'error': 'server_error', 'request_id': 'sk-ant-private',
                         'content': [{'text': text}]}, None)
    assert e.codex_error_info == 'other'
    assert json.loads(e.additional_details) == {'category': 'provider_error', 'httpStatusCode': None, 'requestId': None}
    assert 'sk-ant-' not in str(e) + e.additional_details


def test_conflicting_diagnostics_remain_unknown_and_never_claim_stream_disconnect():
    e = SdkResponseError({'error': 'server_error', 'content': [
        {'text': 'API Error: 500 broken'}, {'text': 'API Error: Connection dropped (ECONNRESET).'}]}, None)
    assert e.codex_error_info == 'other'


def test_cli_json_error_request_id_is_preserved_without_the_body():
    rid = 'req_011CW1234567890abcdef'
    text = 'API Error: 500 ' + json.dumps({'request_id': rid, 'error': {'message': 'secret prompt'}})
    e = SdkResponseError({'error': 'server_error', 'content': [{'text': text}]}, None)
    assert json.loads(e.additional_details)['requestId'] == rid
    assert 'secret prompt' not in e.additional_details + str(e)


@pytest.mark.parametrize('text', ['Repeated 529 Overloaded errors', 'API Error: Repeated 529 Overloaded errors.'])
def test_repeated_overloaded_errors_are_classified(text):
    e = SdkResponseError({'error': 'server_error', 'content': [{'text': text}]}, None)
    assert e.codex_error_info == 'serverOverloaded'
    assert json.loads(e.additional_details)['httpStatusCode'] == 529


async def test_connection_error_details_survive_durable_completion_with_bounded_silent_retry(ph):
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    client = ph.sdk.clients[0]

    async def query(prompt):
        client.query_count += 1
        sid = ph.engine.session(tid)['session_id']
        await client.events.put({'type': 'assistant', 'error': 'server_error', 'content': [
            {'text': 'API Error: Connection dropped (ECONNRESET). private-provider-prose'}]})
        await client.events.put({'type': 'result', 'session_id': sid, 'subtype': 'error_during_execution',
                                 'is_error': True, 'num_turns': 1, 'usage': {}})

    client.query = query
    turn = await ph.turn(tid, 'original input')
    await ph.drain()
    error = ph.store.turn(turn)['error']
    assert error['codexErrorInfo'] == {'httpConnectionFailed': {'httpStatusCode': None}}
    assert json.loads(error['additionalDetails'])['category'] == 'ECONNRESET'
    assert client.query_count == 3
    final = [e['params']['turn'] for e in ph.out if e.get('method') == 'turn/completed'][-1]
    assert final['error'] == error
    assert 'private-provider-prose' not in json.dumps(error)
