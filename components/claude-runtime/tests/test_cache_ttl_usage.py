"""Real usage policy evidence, without a Claude process, account or network."""
from types import SimpleNamespace

import pytest

from cute_claude.engine import Engine
from cute_claude.subscription import usage_counts


def test_usage_counts_preserves_ttl_breakdown_without_counting_tokens_twice():
    raw = {'input_tokens': 10, 'cache_creation_input_tokens': 30,
           'cache_creation': {'ephemeral_5m_input_tokens': 20, 'ephemeral_1h_input_tokens': 10}}
    usage = usage_counts(raw)
    assert usage['cache_creation_ephemeral_5m_input_tokens'] == 20
    assert usage['cache_creation_ephemeral_1h_input_tokens'] == 10
    assert Engine._totals([{'usage': usage}])['inputTokens'] == 40
    assert raw['cache_creation'] == {'ephemeral_5m_input_tokens': 20, 'ephemeral_1h_input_tokens': 10}
    for value in (-1, True, '10', None):
        assert 'cache_creation_ephemeral_5m_input_tokens' not in usage_counts({'cache_creation': {'ephemeral_5m_input_tokens': value}})


@pytest.mark.asyncio
async def test_ttl_is_latest_positive_parent_evidence_and_survives_zero_writes():
    engine = Engine.__new__(Engine)
    engine.store = SimpleNamespace(db=SimpleNamespace(execute=lambda *_: []))
    events = []

    async def emit(thread, method, params):
        events.append((thread, method, params))

    engine.emit = emit

    async def observe(usage, *, kind='sdk-turn', thread='thread'):
        await engine._emit_usage(thread, 'turn', [{'kind': kind, 'usage': usage}])
        return events[-1][2]['tokenUsage']

    assert 'cacheTtlSeconds' not in await observe({'cache_creation_input_tokens': 10})
    assert (await observe({'cache_creation_ephemeral_1h_input_tokens': 10}))['cacheTtlSeconds'] == 3600
    assert (await observe({'cache_creation_ephemeral_5m_input_tokens': 10}))['cacheTtlSeconds'] == 300
    assert (await observe({'cache_creation_ephemeral_5m_input_tokens': 0}))['cacheTtlSeconds'] == 300
    # A subagent's cache policy does not reclassify its parent.
    assert (await observe({'cache_creation_ephemeral_1h_input_tokens': 10}, kind='background-task'))['cacheTtlSeconds'] == 300
    assert 'cacheTtlSeconds' not in await observe({}, thread='other-thread')
    # Actual parent usage can switch back to an hour; both policies choose the shorter one.
    assert (await observe({'cache_creation_ephemeral_1h_input_tokens': 10}))['cacheTtlSeconds'] == 3600
    assert (await observe({'cache_creation_ephemeral_5m_input_tokens': 10, 'cache_creation_ephemeral_1h_input_tokens': 5}))['cacheTtlSeconds'] == 300
