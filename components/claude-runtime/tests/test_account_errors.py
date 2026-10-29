"""Account errors pass through the SDK owner to durable turn completion."""
import pytest

from cute_claude.subscription_auth import ApiAccount
from test_persistent import ph


@pytest.mark.parametrize('api', [False, True])
@pytest.mark.parametrize('code,subscription,api_info', [
    ('authentication_failed', 'unauthorized', 'unauthorized'),
    ('billing_error', 'usageLimitExceeded', 'usageLimitExceeded'),
    ('rate_limit', 'usageLimitExceeded', 'rateLimitExceeded'),
    ('server_error', 'internalServerError', 'internalServerError'),
    ('invalid_request', 'badRequest', 'badRequest'),
    ('unknown', 'other', 'other'),
    ('unsafe key-in-error-code', 'other', 'other'),
])
async def test_sdk_error_mapping_and_redaction(ph, api, code, subscription, api_info):
    secret = 'fixture-key-never-display'
    if api:
        ph.engine.driver.api_account = ApiAccount('ca-fixture', 'ANTHROPIC_API_KEY',
            f'https://user:password@example.test/anthropic?secret={secret}', {}, secret, '/fixture/keys/ca-fixture.json')
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    client = ph.sdk.clients[0]
    async def query(prompt):
        client.query_count += 1
        sid = ph.engine.session(tid)['session_id']
        await client.events.put({'type': 'assistant', 'error': code,
                                 'content': [{'type': 'text', 'text': f'private body {secret}'}]})
        await client.events.put({'type': 'result', 'session_id': sid, 'subtype': 'error_during_execution',
                                 'is_error': True, 'num_turns': 1, 'usage': {}})
    client.query = query
    turn = await ph.turn(tid, 'one fresh instruction')
    await ph.drain()
    result = ph.store.turn(turn)
    assert result['status'] == 'failed'
    assert result['error']['codexErrorInfo'] == (api_info if api else subscription)
    message = result['error']['message']
    assert secret not in message and 'password' not in message and '?secret' not in message
    assert client.query_count == 1 and len(ph.sdk.clients) == 1  # no reconnect or input replay
    assert client.options.env['CLAUDE_CODE_MAX_RETRIES'] == ('3' if api else '0')
    if api:
        assert "Claude API account 'ca-fixture' (example.test)" in message
        assert 'Official Claude' not in message and ph.logins == []
    else:
        assert message.startswith(f'Official Claude returned an error (') and 'Official Claude' in message
    completed = [e['params']['turn'] for e in ph.out if e.get('method') == 'turn/completed']
    assert completed[-1]['error'] == result['error']


@pytest.mark.parametrize('detail', ['HTTP 529', 'overloaded_error'])
def test_overloaded_server_error_is_recognized_without_echoing_provider_body(detail):
    from cute_claude.subscription import SdkResponseError
    error = SdkResponseError({'error': 'server_error', 'content': [{'text': f'{detail} private body'}]}, None)
    assert error.codex_error_info == 'serverOverloaded'
    assert 'private body' not in str(error)


@pytest.mark.parametrize('cli_text', [
    'Failed to refresh OAuth token: another Claude Code process is refreshing it or exited mid-refresh. '
    'This is usually transient; retry in a minute, and if it persists close other Claude Code processes or sign in again',
    'Could not refresh your login because another Claude Code process is refreshing it (or exited mid-refresh) '
    '\u00b7 Try again in a minute; if it keeps happening, close other Claude Code windows or sign in again with /login',
])
def test_refresh_lock_contention_is_transient_not_a_login_failure(cli_text):
    # 2026-10-11 (Windows host): the first message of a new Claude quick chat on an account idle for
    # hours failed with this CLI text; the owner saw no reply and resent. It is not a login problem.
    from cute_claude.subscription import SdkResponseError
    error = SdkResponseError({'error': 'server_error', 'content': [{'text': cli_text}]}, None)
    assert error.transient_refresh is True
    assert error.codex_error_info == 'other'
    assert 'send the message again' in str(error) and 'not automatically retried' in str(error)


def test_other_refresh_failures_stay_unauthorized():
    from cute_claude.subscription import SdkResponseError
    error = SdkResponseError({'error': 'authentication_failed',
                              'content': [{'text': 'Failed to refresh OAuth token: invalid_grant'}]}, None)
    assert error.codex_error_info == 'unauthorized' and not error.transient_refresh
    assert 'OAuth refresh failed' in str(error)


@pytest.mark.parametrize('code', ['rate_limit', 'billing_error', 'server_error'])
def test_error_text_keeps_the_quota_prefix_and_does_not_promise_no_retry(code):
    # cutex quota detection matches the prefix; cutex may requeue a delegation at the reset, so
    # the text must not say the input is never retried.
    from cute_claude.subscription import SdkResponseError
    text = str(SdkResponseError({'error': code, 'content': [{'text': 'private body'}]}, None))
    assert text.startswith(f'Official Claude returned an error ({code}).')
    assert 'not automatically retried' not in text
    assert 'This runtime does not retry it itself; cutex may requeue delegations once the limit resets.' in text
