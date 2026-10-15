from types import SimpleNamespace
import pytest
from cute_claude.rate_limits import RateLimits
from cute_claude.store import Store


def test_observations_replace_missing_fields_and_survive_restart(tmp_path):
    store = Store(tmp_path)
    limits = RateLimits(store)
    assert limits.response()['providerRateLimits']['rows'] == []
    limits.observe(SimpleNamespace(rate_limit_info=SimpleNamespace(
        rate_limit_type='five_hour', status='allowed_warning', utilization=0.815, resets_at=2000)))
    row = limits.response()['providerRateLimits']['rows'][0]
    assert row['usedPercent'] == 81.5
    assert row['status'] == 'allowed_warning'
    limits.observe({'rate_limit_info':{'rateLimitType':'seven_day','status':'rejected','utilization':1}})
    limits.observe({'rate_limit_info':{'rate_limit_type':'five_hour','status':'allowed'}})
    store.close()
    reopened = Store(tmp_path)
    result = RateLimits(reopened).response()
    assert [(r['id'],r['usedPercent'],r['resetsAt']) for r in result['providerRateLimits']['rows']] == [
        ('five_hour',None,None),('seven_day',100.0,None)]
    assert result['rateLimitsByLimitId']['claude-five_hour']['primary'] is None
    assert result['rateLimitsByLimitId']['claude-seven_day']['primary']['usedPercent'] == 100
    assert result['ordinaryUsageAllowed'] is None
    reopened.close()


@pytest.mark.parametrize('value',[True,-0.1,1.1,float('nan'),float('inf'),'0.5',None])
def test_invalid_utilization_is_not_a_percentage(tmp_path,value):
    store=Store(tmp_path)
    row=RateLimits(store).observe({'rate_limit_info':{'status':'allowed','utilization':value}})
    assert row['usedPercent'] is None
    store.close()

from test_persistent import ph


async def test_sdk_event_reaches_account_read_without_starting_a_turn(ph):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId':tid})
    owner = ph.engine.driver.owners[tid]
    await owner._event({'type':'rate_limit_event','rate_limit_info':{
        'rate_limit_type':'five_hour','status':'allowed_warning','utilization':0.6}})
    await owner._event({'type':'rate_limit_event','rate_limit_info':{
        'rate_limit_type':'seven_day','status':'allowed'}})
    result = await ph.call('account/rateLimits/read', {})
    assert [(r['id'],r['usedPercent']) for r in result['providerRateLimits']['rows']] == [
        ('five_hour',60.0),('seven_day',None)]
    assert ph.sdk.clients[0].query_count == 0


def test_expired_observation_keeps_audit_but_clears_numeric_surface(tmp_path, monkeypatch):
    monkeypatch.setattr('cute_claude.rate_limits.time.time',lambda:1000)
    store=Store(tmp_path)
    limits=RateLimits(store)
    limits.observe({'rate_limit_info':{'rate_limit_type':'five_hour','status':'allowed_warning','utilization':.8,'resets_at':1200}})
    assert limits.response()['rateLimits']['primary']['usedPercent'] == 80
    monkeypatch.setattr('cute_claude.rate_limits.time.time',lambda:1201)
    result=limits.response()
    assert result['rateLimits']['primary'] is None
    assert result['providerRateLimits']['rows'][0]['usedPercent'] == 80
    assert result['providerRateLimits']['rows'][0]['status'] == 'allowed_warning'
    store.close()


def test_full_snapshot_clears_absent_windows_and_preserves_unknown(tmp_path):
    store = Store(tmp_path)
    limits = RateLimits(store)
    limits.observe({'rate_limit_info':{'status':'allowed','rate_limit_type':'seven_day','utilization':0.5}})
    assert limits.observe_usage({'rate_limits_available':True,'rate_limits':{
        'five_hour':{'utilization':1,'resets_at':'2099-01-01T00:00:00Z'}}})
    rows = limits.response()['providerRateLimits']['rows']
    assert [(r['id'],r['usedPercent']) for r in rows] == [('five_hour',1.0)]
    assert limits.observe_usage({'rate_limits_available':False,'rate_limits':None})
    assert limits.response()['providerRateLimits']['rows'] == []
    store.close()
