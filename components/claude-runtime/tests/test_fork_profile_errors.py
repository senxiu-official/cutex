from types import SimpleNamespace
from cute_claude.subscription import SdkResponseError, history_signature_rejected, HISTORY_SIGNATURE_REJECTION


def test_copied_history_signature_error_is_clear_and_does_not_echo_provider_text():
    message = SimpleNamespace(error='invalid_request', content=[SimpleNamespace(text='Invalid thinking block signature secret-fixture-token')])
    error = SdkResponseError(message, None)
    assert str(error) == HISTORY_SIGNATURE_REJECTION
    assert error.codex_error_info == 'badRequest'
    assert 'secret-fixture-token' not in str(error)
    assert 'preserved' in str(error)
    assert 'fork' not in str(error)
    assert 'copied' not in str(error)


def test_history_error_classifier_is_narrow_and_bounded():
    assert history_signature_rejected(['The encrypted content could not be verified'])
    assert not history_signature_rejected(['rate limit reached'])
    assert not history_signature_rejected(['tool call signature mismatch'])
    assert not history_signature_rejected(['x' * 8192 + 'invalid thinking signature'])
