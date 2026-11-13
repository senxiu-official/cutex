"""Approval notice receipt timing, hard cap and legacy compatibility; no executables."""
import json
import pytest
from test_persistent import ph
from test_safety_grants import blocked_once, OWNER, events
from test_busy_delivery import soon, hook_for
from test_r8_services import install, invoke
from cute_claude import safety_grants

@pytest.mark.parametrize(('minutes','expires_minutes'), [(10,40),(100,120)])
async def test_window_starts_at_first_read_and_cannot_cross_hard_cap(ph,monkeypatch,minutes,expires_minutes):
    tid,turn,b,args,block=await blocked_once(ph)
    clock=[2_000_000.0];monkeypatch.setattr(safety_grants.time,'time',lambda:clock[0])
    notice='alarm-approval-window'
    granted=await ph.call('thread/safetyCheck/grant',{'threadId':tid,'blockId':block['blockId'],'decidedBy':OWNER,'noticeInputId':notice})
    assert granted['block']['noticeReadAt'] is None
    row=ph.server.safety_grants.row(tid,block['blockId'])
    assert row['expires']==clock[0]+7200
    clock[0]+=minutes*60
    await ph.server.safety_grants.notice_read('different-thread',notice)
    await ph.server.safety_grants.notice_read(tid,'wrong-notice')
    assert ph.server.safety_grants.row(tid,block['blockId'])['notice_read'] is None
    await ph.server.safety_grants.notice_read(tid,notice)
    row=ph.server.safety_grants.row(tid,block['blockId'])
    assert row['notice_read']==clock[0] and row['expires']==2_000_000+expires_minutes*60
    clock[0]+=60
    await ph.server.safety_grants.notice_read(tid,notice)
    assert ph.server.safety_grants.row(tid,block['blockId'])['expires']==row['expires'],'repeat receipt must not extend window'
    assert events(ph,'thread/safetyCheck/updated')[-1]['block']['noticeReadAt']
    reopened=safety_grants.SafetyGrants(ph.server)
    persisted=reopened.row(tid,block['blockId'])
    assert persisted['notice_read']==row['notice_read'] and persisted['expires']==row['expires']

async def test_never_read_expires_at_two_hours_and_a_late_receipt_cannot_revive(ph,monkeypatch):
    tid,turn,b,args,block=await blocked_once(ph)
    clock=[2_000_000.0];monkeypatch.setattr(safety_grants.time,'time',lambda:clock[0])
    await ph.call('thread/safetyCheck/grant',{'threadId':tid,'blockId':block['blockId'],'decidedBy':OWNER,'noticeInputId':'alarm-approval-unread'})
    clock[0]+=7200
    await ph.server.safety_grants.notice_read(tid,'alarm-approval-unread')
    row=ph.server.safety_grants.row(tid,block['blockId'])
    assert row['notice_read'] is None and row['expires']==clock[0]
    assert ph.server.safety_grants.wire(row)['state']=='expired'
    assert ph.server.safety_grants.consume(tid,'Bash',args,'retry') is None

async def test_legacy_controller_keeps_approval_time_window(ph,monkeypatch):
    tid,turn,b,args,block=await blocked_once(ph)
    monkeypatch.setattr(safety_grants.time,'time',lambda:2_000_000.0)
    await ph.call('thread/safetyCheck/grant',{'threadId':tid,'blockId':block['blockId'],'decidedBy':OWNER})
    row=ph.server.safety_grants.row(tid,block['blockId'])
    assert row['expires']==2_000_000+1800 and row['hard_expires'] is None

async def test_registered_and_offered_are_not_read_but_native_receipt_starts_window(ph):
    tid,turn,bridge,args,block=await blocked_once(ph)
    notice='alarm-approval-persisted'
    await ph.call('thread/safetyCheck/grant',{'threadId':tid,'blockId':block['blockId'],'decidedBy':OWNER,'noticeInputId':notice})
    service=install(ph,subscription=True)
    envelope=soon(tid,notice)
    await invoke(service,ph,'submit',envelope,after=False)
    assert ph.server.safety_grants.row(tid,block['blockId'])['notice_read'] is None
    await ph.server.busy_delivery.items_published(tid)
    if tid in ph.server.busy_delivery.stage_tasks:
        await ph.server.busy_delivery.stage_tasks[tid]
    registered=[e for e in events(ph,'claude/input/stageChanged') if e.get('messageId')==notice]
    assert any(e['stage']=='registered' and e['clientId']==notice for e in registered)
    hook,data=await hook_for(ph,bridge)
    await hook(data,'tool-1',{})
    assert ph.server.safety_grants.row(tid,block['blockId'])['notice_read'] is None
    await ph.server.busy_delivery.acknowledge(tid,turn)
    assert service._status(tid,notice,envelope['semanticSha256'])['receipt']
    assert ph.server.safety_grants.row(tid,block['blockId'])['notice_read'] is not None

async def test_native_receipt_and_window_are_atomic_without_followup_event(ph):
    tid,turn,bridge,args,block=await blocked_once(ph)
    mid='alarm-approval-recovered'
    await ph.call('thread/safetyCheck/grant',{'threadId':tid,'blockId':block['blockId'],'decidedBy':OWNER,'noticeInputId':mid})
    service=install(ph,subscription=True); value=soon(tid,mid)
    await invoke(service,ph,'submit',value,after=False)
    service._persisted(tid,mid,turn,native_proof=True)
    assert ph.server.safety_grants.row(tid,block['blockId'])['notice_read'] is not None
    assert service._row(tid,mid)['receipt']
