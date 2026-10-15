from __future__ import annotations

import copy
import json
from dataclasses import replace

import httpx
import pytest

from cute_claude.config import Settings
from cute_claude.provider import AnthropicMessages, ProviderError


def wire(events):
    return ''.join('event: '+e['type']+'\r\ndata: '+json.dumps(e)+'\r\n\r\n' for e in events).encode()


def text_events():
    return [
        {'type': 'message_start', 'message': {'usage': {'input_tokens': 10, 'output_tokens': 1, 'cache_read_input_tokens': 7}}},
        {'type': 'content_block_start', 'index': 0, 'content_block': {'type': 'text', 'text': ''}},
        {'type': 'ping'},
        {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'text_delta', 'text': 'Hello '}},
        {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'text_delta', 'text': '世界'}},
        {'type': 'content_block_stop', 'index': 0},
        {'type': 'message_delta', 'delta': {}, 'usage': {'output_tokens': 2}},
        {'type': 'message_delta', 'delta': {'stop_reason': 'end_turn'}, 'usage': {'output_tokens': 5}},
        {'type': 'message_stop'}]


async def run_provider(tmp_path, events, status=200):
    requests, deltas = [], []
    def handler(request):
        requests.append(request)
        return httpx.Response(status, content=wire(events), headers={'content-type': 'text/event-stream', 'request-id': 'test-request'})
    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as client:
        cfg = Settings(tmp_path, tmp_path / 'state', mock=False, model='configured-model')
        provider = AnthropicMessages(cfg, 'test-api-key-not-a-real-secret', client)
        async def on_delta(index, text): deltas.append((index, text))
        result = await provider.sample([{'role': 'user', 'content': [{'type': 'text', 'text': 'test'}]}], [], 'configured-model', on_delta)
        return result, requests, deltas


async def test_real_http_request_builder_and_sse_cumulative_usage(tmp_path):
    result, requests, deltas = await run_provider(tmp_path, text_events())
    assert result.content == [{'type': 'text', 'text': 'Hello 世界'}]
    assert result.usage['output_tokens'] == 5  # Not 1+2+5.
    assert result.usage['cache_read_input_tokens'] == 7
    assert ''.join(x[1] for x in deltas) == 'Hello 世界'
    assert str(requests[0].url) == 'https://api.anthropic.com/v1/messages'
    assert requests[0].headers['x-api-key'] == 'test-api-key-not-a-real-secret'
    assert 'authorization' not in requests[0].headers
    body = json.loads(requests[0].content)
    assert body['model'] == 'configured-model' and body['stream'] is True
    assert 'temperature' not in body and 'tools' not in body
    assert len(requests) == 1


async def test_tool_json_fragments_are_assembled(tmp_path):
    events = [
        {'type': 'message_start', 'message': {'usage': {}}},
        {'type': 'content_block_start', 'index': 0, 'content_block': {'type': 'tool_use', 'id': 'toolu_1', 'name': 'read_file', 'input': {}}},
        {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'input_json_delta', 'partial_json': '{"path":'}},
        {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'input_json_delta', 'partial_json': '"hello.txt"}'}},
        {'type': 'content_block_stop', 'index': 0},
        {'type': 'message_delta', 'delta': {'stop_reason': 'tool_use'}, 'usage': {'output_tokens': 3}},
        {'type': 'message_stop'}]
    result, _, _ = await run_provider(tmp_path, events)
    assert result.content[0]['input'] == {'path': 'hello.txt'}


async def test_thinking_signature_is_preserved_not_flattened(tmp_path):
    events = [
        {'type': 'message_start', 'message': {'usage': {}}},
        {'type': 'content_block_start', 'index': 0, 'content_block': {'type': 'thinking', 'thinking': '', 'signature': ''}},
        {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'thinking_delta', 'thinking': 'provider supplied reasoning'}},
        {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'signature_delta', 'signature': 'opaque-signature'}},
        {'type': 'content_block_stop', 'index': 0},
        {'type': 'message_delta', 'delta': {'stop_reason': 'end_turn'}, 'usage': {}},
        {'type': 'message_stop'}]
    result, _, deltas = await run_provider(tmp_path, events)
    assert result.content[0]['signature'] == 'opaque-signature'
    assert deltas == []


@pytest.mark.parametrize('events', [text_events()[:-1], [{'type': 'error', 'error': {'message': 'secret should not leak'}}], [{'type': 'message_start', 'message': {}}, {'type': 'content_block_start', 'index': 0, 'content_block': {'type': 'server_tool_use'}}]])
async def test_incomplete_or_unsupported_stream_is_not_success(tmp_path, events):
    with pytest.raises(ProviderError) as exc:
        await run_provider(tmp_path, events)
    assert 'secret should not leak' not in str(exc.value)


async def test_http_429_has_no_retry_or_body_leak(tmp_path):
    count = 0
    def handler(request):
        nonlocal count
        count += 1
        return httpx.Response(429, text='private response body: token-123')
    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as client:
        p = AnthropicMessages(Settings(tmp_path, tmp_path/'s'), 'key', client)
        async def delta(i, s): pass
        with pytest.raises(ProviderError) as exc:
            await p.sample([], [], 'model', delta)
        assert 'token-123' not in str(exc.value)
    assert count == 1


@pytest.mark.parametrize('base', ['http://example.com', 'https://user:pass@example.com', 'https://example.com?key=value'])
def test_api_base_credential_leaks_refused(tmp_path, base):
    with pytest.raises(ValueError):
        AnthropicMessages(Settings(tmp_path, tmp_path/'s', api_base=base), 'key')


async def test_sparse_content_indices_rejected(tmp_path):
    events = text_events()
    events[1]['index'] = 3
    with pytest.raises(ProviderError, match='sequence'):
        await run_provider(tmp_path, events)


async def test_invalid_tool_id_rejected_as_provider_error(tmp_path):
    events = [
        {'type': 'message_start', 'message': {'usage': {}}},
        {'type': 'content_block_start', 'index': 0, 'content_block': {'type': 'tool_use', 'id': ['invalid'], 'name': 'test', 'input': {}}},
        {'type': 'content_block_stop', 'index': 0},
        {'type': 'message_delta', 'delta': {'stop_reason': 'tool_use'}, 'usage': {}},
        {'type': 'message_stop'}]
    with pytest.raises(ProviderError, match='IDs'):
        await run_provider(tmp_path, events)
