import asyncio
from types import SimpleNamespace
import pytest
from test_persistent import ph

async def test_status_reads_current_usage_without_new_turn_or_child(ph):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId':tid})
    owner = ph.engine.driver.owners[tid]
    client = owner.client
    counts = []
    async def context():
        assert asyncio.current_task() is owner.task
        counts.append('context')
        return {'totalTokens':30000,'maxTokens':200000}
    async def control(request):
        assert asyncio.current_task() is owner.task
        assert request == {'subtype':'get_usage'}
        counts.append('usage')
        return {'rate_limits_available':True,'rate_limits':{
            'five_hour':{'utilization':25,'resets_at':'2099-01-01T00:00:00Z'},
            'seven_day':{'utilization':None,'resets_at':None},
            'model_scoped':[{'display_name':'Fable','utilization':45,'resets_at':None}]}}
    client.get_context_usage = context
    client._query = SimpleNamespace(_send_control_request=control)
    result = await ph.call('account/rateLimits/read')
    assert result['rateLimitsByLimitId']['claude-five_hour']['primary']['usedPercent'] == 25
    assert result['rateLimitsByLimitId']['claude-seven_day']['primary'] is None
    usage = [m['params']['tokenUsage'] for m in ph.out if m.get('method') == 'thread/tokenUsage/updated'][-1]
    assert usage['modelContextWindow'] == 200000
    assert usage['last']['totalTokens'] == 30000
    assert usage['total']['totalTokens'] == 0
    assert client.query_count == 0 and len(ph.sdk.clients) == 1
    await ph.call('account/rateLimits/read')
    assert counts == ['usage','context']

async def test_metrics_failure_does_not_pause_or_stop_runtime(ph):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId':tid})
    client = ph.engine.driver.owners[tid].client
    async def failed(*args): raise RuntimeError('unavailable')
    client.get_context_usage = failed
    client._query = SimpleNamespace(_send_control_request=failed)
    await ph.call('account/rateLimits/read')
    await ph.turn(tid,'still works')
    await ph.drain()
    assert ph.engine.driver.owners[tid].state == 'waiting'
    assert client.query_count == 1

async def test_resume_refreshes_context_without_billing_totals_fabrication(ph):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId':tid})
    async def context(): return {'totalTokens':50,'maxTokens':1000000}
    ph.engine.driver.owners[tid].client.get_context_usage = context
    await ph.call('thread/resume',{'threadId':tid})
    assert ph.engine.metrics.context[tid] == {'used':50,'maximum':1000000}
