"""Full access: Claude Code safety checks are declined, recorded as safety
blocks, and the owner may grant exactly one retry (safety_grants.py)."""
import time
from dataclasses import replace
from types import SimpleNamespace

from test_persistent import ph
from test_questions_plan import begin
from test_subscription import SubHarness
from cute_claude.native_tools import BYPASS_SAFETY_HINT
from cute_claude.safety_grants import GRANT_SECONDS, grant_key
from cute_claude.subscription import OfficialSdkDriver

REASON = 'Dangerous rm operation on possibly-empty variable path: $S/$v'
OWNER = {'kind': 'owner'}


async def setup(ph):
    ph.engine.settings = replace(ph.settings, allow_exec=True)
    tid, turn, b = await begin(ph)
    assert ph.server.approvals.effective_mode(tid) == 'bypassPermissions'
    return tid, turn, b


async def attempt(b, args, call_id, name='Bash'):
    await b.pre_tool({'session_id': b.sid, 'tool_name': name, 'tool_input': args}, call_id, {})
    return await b.native.approve(name, args, SimpleNamespace(tool_use_id=call_id, title=REASON))


def events(ph, method):
    return [m['params'] for m in ph.out if m.get('method') == method]


async def blocked_once(ph):
    tid, turn, b = await setup(ph)
    args = {'command': 'rm -rf $S/$v', 'description': 'clean up'}
    decision = await attempt(b, args, 'call-1')
    assert decision['allow'] is False and BYPASS_SAFETY_HINT in decision['message']
    [blocked] = events(ph, 'thread/safetyCheck/blocked')
    return tid, turn, b, args, blocked['block']


async def test_block_is_recorded_and_notified_without_asking_anyone(ph):
    tid, turn, b, args, block = await blocked_once(ph)
    assert block['threadId'] == tid and block['turnId'] == turn and block['itemId'] == 'call-1'
    assert block['toolName'] == 'Bash' and block['toolInput'] == args
    assert block['command'] == 'rm -rf $S/$v' and block['reason'] == REASON
    assert block['state'] == 'blocked' and block['blockId'].startswith('sb-') and block['grantSeconds'] == GRANT_SECONDS
    assert block['source'] == {'kind': 'user', 'id': 'test', 'name': None}
    # Layer 1: the model is told to stop and explain rather than work around it.
    assert 'do not retry it' in BYPASS_SAFETY_HINT and 'explain to the owner' in BYPASS_SAFETY_HINT
    assert '${NAME:?}' in BYPASS_SAFETY_HINT and 'sh -c' in BYPASS_SAFETY_HINT
    assert not ph.server.approvals.pending
    assert not any(m.get('method') == 'item/commandExecution/requestApproval' for m in ph.out)
    listed = await ph.call('thread/safetyCheck/list', {'threadId': tid, 'state': 'blocked'})
    assert [x['blockId'] for x in listed['data']] == [block['blockId']]
    # Durable: a reconnecting controller can read it from the event log.
    logged = await ph.call('claude/events/read', {'threadId': tid})
    assert any(e['method'] == 'thread/safetyCheck/blocked' for e in logged['data'])


async def test_grant_allows_exactly_one_identical_retry(ph):
    tid, turn, b, args, block = await blocked_once(ph)
    granted = await ph.call('thread/safetyCheck/grant', {'threadId': tid, 'blockId': block['blockId'], 'decidedBy': OWNER})
    assert granted['block']['state'] == 'granted' and granted['block']['decidedBy'] == OWNER
    assert granted['block']['expiresAt'] is not None
    assert events(ph, 'thread/safetyCheck/updated')[-1]['block']['state'] == 'granted'
    # The retry may word its description differently: Bash binds to the command.
    retry = await attempt(b, {'command': 'rm -rf $S/$v', 'description': 'retry after approval'}, 'call-2')
    assert retry == {'allow': True, 'session': False, 'interrupt': False}
    used = events(ph, 'thread/safetyCheck/updated')[-1]['block']
    assert used['state'] == 'used' and used['usedAt'] is not None
    # Never reusable: the same input again is declined and recorded anew.
    again = await attempt(b, dict(args), 'call-3')
    assert again['allow'] is False and BYPASS_SAFETY_HINT in again['message']
    assert len(events(ph, 'thread/safetyCheck/blocked')) == 2
    refused = await ph.call('thread/safetyCheck/grant', {'threadId': tid, 'blockId': block['blockId'], 'decidedBy': OWNER}, error=True)
    assert refused['data']['code'] == 'safety_block_closed'


async def test_grant_does_not_cover_a_different_input_or_thread(ph):
    tid, turn, b, args, block = await blocked_once(ph)
    await ph.call('thread/safetyCheck/grant', {'threadId': tid, 'blockId': block['blockId'], 'decidedBy': OWNER})
    other = await attempt(b, {'command': 'rm -rf $S/$w'}, 'call-2')
    assert other['allow'] is False
    assert grant_key('Bash', {'command': 'x'}) != grant_key('Write', {'command': 'x'})
    # The grant is still there for the exact command.
    assert (await attempt(b, {'command': 'rm -rf $S/$v'}, 'call-3'))['allow'] is True


async def test_grant_expires_after_thirty_minutes(ph):
    tid, turn, b, args, block = await blocked_once(ph)
    await ph.call('thread/safetyCheck/grant', {'threadId': tid, 'blockId': block['blockId'], 'decidedBy': OWNER})
    ph.store.db.execute('UPDATE safety_blocks SET expires=? WHERE id=?', (time.time() - 1, block['blockId']))
    assert (await attempt(b, dict(args), 'call-2'))['allow'] is False
    listed = await ph.call('thread/safetyCheck/list', {'threadId': tid})
    assert {x['blockId']: x['state'] for x in listed['data']}[block['blockId']] == 'expired'


async def test_dismiss_clears_without_granting(ph):
    tid, turn, b, args, block = await blocked_once(ph)
    dismissed = await ph.call('thread/safetyCheck/dismiss', {'threadId': tid, 'blockId': block['blockId'], 'decidedBy': OWNER})
    assert dismissed['block']['state'] == 'dismissed'
    assert (await attempt(b, dict(args), 'call-2'))['allow'] is False
    refused = await ph.call('thread/safetyCheck/grant', {'threadId': tid, 'blockId': block['blockId'], 'decidedBy': OWNER}, error=True)
    assert refused['data']['code'] == 'safety_block_closed'


async def test_only_the_owner_through_an_interactive_controller_can_decide(ph):
    tid, turn, b, args, block = await blocked_once(ph)
    params = {'threadId': tid, 'blockId': block['blockId']}
    for decided_by in [None, {'kind': 'agent', 'id': 'cutex.director'}, {'kind': 'service', 'id': 'bus'}, {'kind': 'user'}]:
        refused = await ph.call('thread/safetyCheck/grant', {**params, **({'decidedBy': decided_by} if decided_by else {})}, error=True)
        assert refused['data']['code'] == 'owner_required', decided_by
    ph.peer.interactive = False  # an observer / service connection
    refused = await ph.call('thread/safetyCheck/grant', {**params, 'decidedBy': OWNER}, error=True)
    assert refused['data']['code'] == 'owner_required'
    ph.peer.interactive = True
    missing = await ph.call('thread/safetyCheck/grant', {**params, 'blockId': 'sb-unknown', 'decidedBy': OWNER}, error=True)
    assert missing['data']['code'] == 'safety_block_not_found'
    assert (await ph.call('thread/safetyCheck/list', {'threadId': tid}))['data'][0]['state'] == 'blocked'
    assert (await attempt(b, dict(args), 'call-2'))['allow'] is False


async def test_grant_survives_an_adapter_restart_and_is_still_single_use(ph):
    tid, turn, b, args, block = await blocked_once(ph)
    await ph.call('thread/safetyCheck/grant', {'threadId': tid, 'blockId': block['blockId'], 'decidedBy': OWNER})
    settings = ph.engine.settings
    cli, config = ph.engine.driver.cli, ph.engine.driver.config_dir
    await ph.close()
    async def already_closed(): pass
    ph.close = already_closed
    newer = SubHarness(settings, OfficialSdkDriver(cli, config, interrupt_timeout=2))
    try:
        await newer.init()
        listed = await newer.call('thread/safetyCheck/list', {'threadId': tid})
        assert listed['data'][0]['state'] == 'granted'
        # The previous turn was cut by the restart; the grant is consumed by
        # the next safety check of the same call, exactly once.
        safety = newer.server.safety_grants
        used = safety.consume(tid, 'Bash', {'command': 'rm -rf $S/$v'}, 'call-r1')
        assert used is not None and used['state'] == 'used' and used['blockId'] == block['blockId']
        assert safety.consume(tid, 'Bash', {'command': 'rm -rf $S/$v'}, 'call-r2') is None
    finally:
        await newer.close()
