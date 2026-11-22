"""Auto-online phase against a fake management API (no services, network or
credentials), plus staging into copies of the real kit tool.

Set CUTEX_R276_KIT_TOOL_SOURCE (and optionally CUTEX_R2761_KIT_TOOL_SOURCE)
to read-only kit-tool directories to run the staging cases too.
"""
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import types
import unittest
from unittest import mock

import stage_upgrade_online_kit as staging
import upgrade_online_kit as kit


def row(sid, name, outcome='confirmed', selected=True):
    return {'id': sid, 'name': name, 'sourceOperations': ['op'], 'selectedForOfflineByThisOperation': selected,
            'offlinedByThisOperation': outcome == 'confirmed', 'offlineOutcome': outcome}


class FakeApi:
    """Sessions, runtime-package runs and asks, like the management API."""

    def __init__(self, ids, outdated=(), queued=None, refuse=(), never_online=(), timeout=()):
        self.sessions = {sid: {'cutexSessionId': sid, 'displayName': sid.upper(),
                               'runtime': {'status': 'offline', 'appServerConnected': False, 'runtimeGeneration': 3}}
                         for sid in ids}
        self.outdated = set(outdated)
        self.queued = queued or {}
        self.refuse = set(refuse)
        self.never_online = set(never_online)
        self.timeout = set(timeout)
        self.calls = []
        self.online_requests = []
        self.runs = {}
        self.lock = threading.Lock()
        self.in_flight = 0
        self.max_in_flight = 0
        self.run_fails = False

    def __call__(self, path, method='GET', body=None, timeout=60):
        with self.lock:
            self.calls.append((method, path, body))
        if path == '/v2/runtime-packages':
            return {'agents': [{'cutexSessionId': s, 'online': self.online(s)} for s in sorted(self.outdated)],
                    'activeRun': None, 'selectedError': None}
        if path == '/v2/runtime-packages/runs' and method == 'POST':
            if self.run_fails:
                raise kit.HttpError(409, {'error': {'code': 'selected_unavailable', 'message': 'no deployment'}})
            items = [{'cutexSessionId': s, 'state': 'moved' if s != 'bad' else 'failed',
                      'code': None if s != 'bad' else 'reviewRefused', 'reason': None if s != 'bad' else 'refused',
                      'previousRuntimeVersion': '0.160.0', 'targetRuntimeVersion': '0.164.0'}
                     for s in body['agentIds'] if s in self.outdated]
            run = {'runId': 'rpu-%d' % len(self.runs), 'state': 'running', 'items': items}
            self.runs[run['runId']] = run
            for item in items:
                if item['state'] == 'moved':
                    self.outdated.discard(item['cutexSessionId'])
            return {'run': dict(run)}
        if path.startswith('/v2/runtime-packages/runs/'):
            run = self.runs[path.rsplit('/', 1)[1]]
            run['state'] = 'completed'
            return {'run': run}
        if path.startswith('/v2/asks?'):
            asks = [{'askId': '%s-%d' % (t, i), 'target': t} for t, n in self.queued.items() for i in range(n)]
            return {'asks': asks, 'hasMore': False}
        if path == '/v2/sessions?includeHidden=true':
            return {'sessions': list(self.sessions.values())}
        sid = path.split('/')[3]
        if method == 'GET':
            if sid not in self.sessions:
                raise kit.HttpError(404, {'error': {'code': 'session_not_found', 'message': 'gone'}})
            return json.loads(json.dumps(self.sessions[sid]))
        assert body['method'] == 'cutex/runtime/online' and path.endswith('/cutex/requests'), body
        assert body['params']['expectedRuntimeGeneration'] == 3 and body['params']['openVisibleTerminal'] is False
        with self.lock:
            self.online_requests.append((sid, body['requestId']))
            self.in_flight += 1
            self.max_in_flight = max(self.max_in_flight, self.in_flight)
        try:
            time.sleep(0.03)
            if sid in self.refuse:
                raise kit.HttpError(409, {'error': {'code': 'launch_refused', 'message': 'profile missing'}})
            if sid not in self.never_online:
                self.sessions[sid]['runtime'].update(status='online', appServerConnected=True)
            if sid in self.timeout:
                raise TimeoutError('timed out')
            return {'ok': True}
        finally:
            with self.lock:
                self.in_flight -= 1

    def online(self, sid):
        return self.sessions[sid]['runtime']['appServerConnected']


class PhaseTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix='uao-'))
        self.addCleanup(shutil.rmtree, self.root, True)
        self.op = self.root / 'operations' / '20261105T000000-aaaa'
        self.op.mkdir(parents=True)
        self.lines = []

    def manual(self, rows):
        (self.op / 'manual-online.json').write_text(json.dumps(rows), encoding='utf-8')

    def phase(self, api, plan=None, memory=lambda: None):
        plan = {'home': (plan or {}).get('home'), 'autoOnline': {'pollSeconds': 0.001, 'onlineTimeoutSeconds': 0.2, **((plan or {}).get('autoOnline') or {})}}
        return kit.Phase(self.op, plan, api, memory_probe=memory, out=self.lines.append, token='secret-token')

    def result(self):
        return json.loads((self.op / kit.RESULT).read_text(encoding='utf-8'))

    def by_id(self):
        return {a['id']: a for a in self.result()['agents']}

    def test_preserved_outdated_pin_is_not_moved_before_conditional_online(self):
        stopped = row('a', 'A');stopped['runtimeUpdatedAtAfterOffline'] = 'confirmed-stop'
        self.manual([stopped]);api = FakeApi(['a'], outdated=['a'])
        api.sessions['a']['updatedAt'] = 'confirmed-stop'
        self.phase(api, {'autoOnline': {'preserveExistingRuntimePins': True}}).run()
        self.assertEqual([sid for sid, _ in api.online_requests], ['a'])
        self.assertFalse(any(path.startswith('/v2/runtime-packages') for _, path, _ in api.calls))
        self.assertEqual(self.result()['runtimePackages']['runIds'], [])
        self.assertEqual(self.result()['runtimePackages']['policy'], 'preserve-existing-runtime-pins')

    def test_preserved_pin_still_respects_a_newer_owner_stop(self):
        stopped = row('a', 'A');stopped['runtimeUpdatedAtAfterOffline'] = 'confirmed-stop'
        self.manual([stopped]);api = FakeApi(['a'], outdated=['a'])
        api.sessions['a']['updatedAt'] = 'newer-owner-stop'
        self.phase(api, {'autoOnline': {'preserveExistingRuntimePins': True}}).run()
        self.assertEqual(self.by_id()['a']['code'], 'runtimeIntentChanged')
        self.assertFalse(api.online_requests)
        self.assertFalse(any(path.startswith('/v2/runtime-packages') for _, path, _ in api.calls))

    # r290 P9: r289 already-offline Claude can keep updatedAt unchanged.
    # Its real stop receipt replacement must still supersede the kit stop.
    def test_repeated_predecessor_stop_receipt_supersedes_unchanged_session_row(self):
        import hashlib
        path = self.root / 'runtime/runtime-stops' / (hashlib.sha256(b'a').hexdigest() + '-3.json')
        path.parent.mkdir(parents=True)
        raw = json.dumps({'session':'a','generation':3,'state':'offline'})
        path.write_text(raw)
        stopped = row('a','A');stopped['runtimeUpdatedAtAfterOffline']='unchanged'
        stopped['runtimeStopReceiptAfterOffline']=kit.runtime_stop_marker(self.root,'a',3)
        self.assertTrue(stopped['runtimeStopReceiptAfterOffline']['available'])
        self.manual([stopped]);api=FakeApi(['a']);api.sessions['a']['updatedAt']='unchanged'
        # The predecessor's RequestedStop::finish atomically replaces this
        # path with identical contents; no artificial sleep/clock is needed.
        replacement=path.with_suffix('.new');replacement.write_text(raw);os.replace(replacement,path)
        self.assertNotEqual(kit.runtime_stop_marker(self.root,'a',3),stopped['runtimeStopReceiptAfterOffline'])
        self.phase(api,{'home':str(self.root)}).run()
        self.assertEqual(self.by_id()['a']['code'],'runtimeIntentChanged')
        self.assertFalse(api.online_requests)

    def test_unchanged_confirmed_receipt_restores_but_unverifiable_receipt_does_not(self):
        import hashlib
        path=self.root/'runtime/runtime-stops'/(hashlib.sha256(b'a').hexdigest()+'-3.json')
        path.parent.mkdir(parents=True);path.write_text(json.dumps({'session':'a','generation':3,'state':'offline'}))
        stopped=row('a','A');stopped['runtimeUpdatedAtAfterOffline']='unchanged';stopped['runtimeStopReceiptAfterOffline']=kit.runtime_stop_marker(self.root,'a',3)
        unknown=row('b','B');unknown['runtimeStopReceiptAfterOffline']=kit.runtime_stop_marker(self.root,'b',3)
        self.manual([stopped,unknown]);api=FakeApi(['a','b']);api.sessions['a']['updatedAt']='unchanged';self.phase(api,{'home':str(self.root)}).run()
        self.assertEqual([sid for sid,_ in api.online_requests],['a'])
        self.assertEqual(self.by_id()['b']['code'],'runtimeIntentUnverifiable')

    def test_only_agents_this_operation_confirmed_offline_are_started(self):
        self.manual([row('a', 'A'), row('b', 'B'), row('inherited', 'I', outcome=None, selected=False),
                     row('planned', 'P', outcome='planned'), row('requested', 'R', outcome='requested')])
        api = FakeApi(['a', 'b', 'inherited', 'planned', 'requested', 'never-listed'])
        self.phase(api).run()
        self.assertEqual(sorted(sid for sid, _ in api.online_requests), ['a', 'b'])
        self.assertFalse(api.online('never-listed') or api.online('inherited') or api.online('planned'))
        result = self.result()
        self.assertEqual(result['state'], 'completed')
        self.assertEqual(result['counts']['online'], 2)
        self.assertEqual({e['id'] for e in result['excluded']}, {'inherited', 'planned', 'requested'})
        self.assertTrue(any(line.startswith('AUTO ONLINE completed: 2 back online, 0 failed, 3 skipped') for line in self.lines))

    # r290 P9: explicit maintenance online does not mean owner resume.
    def test_online_paused_is_restored_paused_and_original_offline_is_excluded(self):
        self.manual([row('a', 'A'), row('offline', 'Offline', outcome=None, selected=False)])
        api = FakeApi(['a', 'offline'])
        for session in api.sessions.values():
            session.update(agentPause={'by':'owner','requestedAt':'2026-11-17T00:00:00Z','phase':'paused'},
                           pauseReasons=[{'kind':'agent','phase':'paused'}])
        self.phase(api).run()
        agent = self.by_id()['a']
        self.assertEqual(agent['status'], 'online')
        self.assertEqual(agent['pauseBeforeOnline'], agent['pauseAfterOnline'])
        self.assertEqual(agent['pauseAfterOnline']['agentPause']['by'], 'owner')
        self.assertEqual([sid for sid, _ in api.online_requests], ['a'])

    # r290 P9: retain both observations, never repair by a synthetic resume.
    def test_online_pause_loss_is_reported_without_repair(self):
        self.manual([row('a', 'A')])
        api = FakeApi(['a'])
        api.sessions['a'].update(agentPause={'by':'owner','requestedAt':'2026-11-17T00:00:00Z'},
                                 pauseReasons=[{'kind':'agent'}])
        def request(path, method='GET', body=None, timeout=60):
            result = api(path, method, body, timeout)
            if method == 'POST' and body.get('method') == 'cutex/runtime/online':
                api.sessions['a'].update(agentPause=None, pauseReasons=[])
            return result
        self.phase(request).run()
        agent = self.by_id()['a']
        self.assertEqual(agent['code'], 'pauseChanged')
        self.assertEqual(len(api.online_requests), 1)
        self.assertFalse(agent['pauseAfterOnline']['agentPause'])

    # r290 P9: generation is unchanged by a second stop of an offline agent.
    def test_newer_owner_offline_after_our_confirmation_is_not_started(self):
        stopped = row('a', 'A')
        stopped['runtimeUpdatedAtAfterOffline'] = '2026-11-17T00:00:00Z'
        self.manual([stopped])
        api = FakeApi(['a'])
        api.sessions['a']['updatedAt'] = '2026-11-17T00:00:01Z'
        self.phase(api).run()
        self.assertEqual(self.by_id()['a']['code'], 'runtimeIntentChanged')
        self.assertEqual(api.online_requests, [])

    # r290 P9: the daemon must recheck after the GET/POST race, too.
    def test_online_carries_confirmed_stop_token_for_atomic_daemon_check(self):
        stopped = row('a', 'A')
        stopped['runtimeUpdatedAtAfterOffline'] = '2026-11-17T00:00:00Z'
        self.manual([stopped])
        api = FakeApi(['a'])
        api.sessions['a']['updatedAt'] = stopped['runtimeUpdatedAtAfterOffline']
        self.phase(api).run()
        posts = [body for method, path, body in api.calls if method == 'POST' and body.get('method') == 'cutex/runtime/online']
        self.assertEqual(len(posts), 1)
        self.assertEqual(posts[0]['params']['expectedRuntimeUpdatedAt'], stopped['runtimeUpdatedAtAfterOffline'])

    # r290 P9: newer owner offline after GET is caught by the daemon fence.
    def test_racing_owner_stop_refusal_is_skipped_once_without_replay(self):
        stopped = row('a', 'A')
        stopped['runtimeUpdatedAtAfterOffline'] = '2026-11-17T00:00:00Z'
        self.manual([stopped])
        api = FakeApi(['a'])
        api.sessions['a']['updatedAt'] = stopped['runtimeUpdatedAtAfterOffline']
        posts = []
        def request(path, method='GET', body=None, timeout=60):
            if method == 'POST':
                posts.append(body)
                raise kit.HttpError(409, {'error': {'code':'revision_conflict',
                    'message':'new owner stop', 'details':{'reason':'runtime_intent_changed'}}})
            return api(path, method, body, timeout)
        self.phase(request).run()
        self.assertEqual(self.by_id()['a']['status'], 'skipped')
        self.assertEqual(self.by_id()['a']['code'], 'runtimeIntentChanged')
        self.assertEqual(len(posts), 1)
        self.assertFalse(api.online('a'))

    def test_outdated_targets_move_through_one_runtime_run_and_failures_still_start(self):
        self.manual([row('a', 'A'), row('bad', 'Bad'), row('c', 'C')])
        api = FakeApi(['a', 'bad', 'c', 'other'], outdated=['a', 'bad', 'other'])
        self.phase(api).run()
        posts = [body for method, path, body in api.calls if path == '/v2/runtime-packages/runs' and method == 'POST']
        self.assertEqual(posts, [{'agentIds': ['a', 'bad']}])  # never 'other', never current 'c'
        agents = self.by_id()
        self.assertEqual(agents['a']['runtime']['state'], 'moved')
        self.assertEqual(agents['bad']['runtime']['code'], 'reviewRefused')
        self.assertEqual({a['status'] for a in agents.values()}, {'online'})
        self.assertIn('other', api.outdated)

    def test_runtime_run_error_keeps_pins_and_still_brings_agents_online(self):
        self.manual([row('a', 'A')])
        api = FakeApi(['a'], outdated=['a'])
        api.run_fails = True
        self.phase(api).run()
        self.assertIn('selected_unavailable', self.result()['runtimePackages']['error'])
        self.assertEqual(self.by_id()['a']['status'], 'online')

    def test_unfinished_runtime_run_blocks_only_its_agents(self):
        self.manual([row('a', 'A'), row('c', 'C')])
        api = FakeApi(['a', 'c'], outdated=['a'])
        phase = self.phase(api)
        with mock.patch.object(phase, 'follow', side_effect=RuntimeError('did not finish in time')):
            phase.run()
        agents = self.by_id()
        self.assertEqual(agents['a']['code'], 'runtimeRunUnfinished')
        self.assertEqual(agents['c']['status'], 'online')
        self.assertEqual([sid for sid, _ in api.online_requests], ['c'])

    def test_concurrency_is_limited(self):
        ids = ['a%02d' % i for i in range(12)]
        self.manual([row(s, s) for s in ids])
        api = FakeApi(ids)
        self.phase(api, {'autoOnline': {'concurrency': 3}}).run()
        self.assertEqual(len(api.online_requests), 12)
        self.assertLessEqual(api.max_in_flight, 3)
        self.assertGreater(api.max_in_flight, 1)

    def test_low_memory_stops_new_starts_and_records_skips(self):
        self.manual([row('a', 'A'), row('b', 'B')])
        api = FakeApi(['a', 'b'])
        gib = 1024 ** 3
        self.phase(api, {'autoOnline': {'memoryWaitSeconds': 0.01}}, memory=lambda: (4 * gib, 251 * gib)).run()
        self.assertEqual(api.online_requests, [])
        agents = self.by_id()
        self.assertEqual({a['code'] for a in agents.values()}, {'memoryLow'})
        self.assertIn('25702 MiB floor', agents['a']['reason'])  # 10 % of 251 GiB

    def test_failures_do_not_stop_others_and_carry_reasons(self):
        self.manual([row('a', 'A'), row('refused', 'R'), row('stuck', 'S'), row('gone', 'G')])
        api = FakeApi(['a', 'refused', 'stuck'], refuse=['refused'], never_online=['stuck'])
        self.phase(api).run()
        agents = self.by_id()
        self.assertEqual(agents['a']['status'], 'online')
        self.assertEqual((agents['refused']['status'], agents['refused']['code']), ('failed', 'launch_refused'))
        self.assertEqual((agents['stuck']['status'], agents['stuck']['code']), ('failed', 'notConfirmed'))
        self.assertEqual((agents['gone']['status'], agents['gone']['code']), ('skipped', 'notFound'))
        self.assertIn('Retry (safe, skips agents already brought back): owner auto-online', self.lines)
        self.assertNotIn('secret-token', (self.op / kit.RESULT).read_text(encoding='utf-8'))

    def test_transport_timeout_reads_back_and_never_replays(self):
        self.manual([row('a', 'A')])
        api = FakeApi(['a'], timeout=['a'])
        self.phase(api).run()
        self.assertEqual(len(api.online_requests), 1)
        self.assertEqual(self.by_id()['a']['status'], 'online')

    def test_retry_is_idempotent_and_never_restarts_agents_brought_back_earlier(self):
        self.manual([row('a', 'A'), row('refused', 'R')])
        api = FakeApi(['a', 'refused'], refuse=['refused'])
        self.phase(api).run()
        first = self.by_id()['refused']['requestIds']
        # The owner stops 'a' after the first run; the profile problem is fixed.
        api.sessions['a']['runtime'].update(status='offline', appServerConnected=False)
        api.refuse.clear()
        api.online_requests.clear()
        self.phase(api).run()
        self.assertEqual([sid for sid, _ in api.online_requests], ['refused'])
        agents = self.by_id()
        self.assertTrue(agents['a']['earlierRun'])
        self.assertEqual(agents['refused']['status'], 'online')
        self.assertEqual(agents['refused']['requestIds'][:len(first)], first)
        self.assertEqual(len(set(agents['refused']['requestIds'])), 2)
        self.assertEqual(self.result()['attempt'], 2)
        self.assertFalse(api.online('a'))

    def test_owner_skip_starts_nobody_and_retry_works_later(self):
        self.manual([row('a', 'A')])
        api = FakeApi(['a'], queued={'a': 2})
        self.phase(api).run(skip=True)
        self.assertEqual(api.online_requests, [])
        self.assertEqual(self.result()['state'], 'skipped-by-owner')
        self.assertEqual(self.by_id()['a']['code'], 'ownerSkipped')
        self.assertIn('  A (a): 2 queued', self.lines)
        self.phase(api).run()
        self.assertEqual(self.by_id()['a']['status'], 'online')

    def test_waiting_delegations_list_offline_targets_only_and_start_them_first(self):
        self.manual([row('a', 'A'), row('b', 'B'), row('refused', 'R')])
        api = FakeApi(['a', 'b', 'refused', 'stopped-by-owner'], refuse=['refused'],
                      queued={'b': 1, 'refused': 3, 'stopped-by-owner': 2})
        self.phase(api, {'autoOnline': {'concurrency': 1}}).run()
        self.assertEqual([sid for sid, _ in api.online_requests], ['refused', 'b', 'a'])
        waiting = self.result()['waitingDelegations']['agents']
        self.assertEqual([(w['id'], w['queued']) for w in waiting], [('refused', 3), ('stopped-by-owner', 2)])
        self.assertIn('WAITING DELEGATIONS 2 offline agent(s) with queued delegations', self.lines)

    def test_interrupt_is_recorded_and_does_not_propagate(self):
        self.manual([row('a', 'A')])
        api = FakeApi(['a'])
        phase = self.phase(api)
        with mock.patch.object(phase, 'start_all', side_effect=KeyboardInterrupt):
            result = phase.run()
        self.assertEqual(result['state'], 'interrupted')

    def deploy(self, phase='complete'):
        (self.op / 'operation.json').write_text(json.dumps({'phase': phase}), encoding='utf-8')
        d = types.SimpleNamespace(plan={'apiOrigin': 'http://127.0.0.1:9', 'autoOnline': {'pollSeconds': 0.001}},
                                  token='secret-token', root=self.root, latest=lambda: self.op)
        return d

    def test_after_activation_requires_complete_and_never_raises(self):
        self.manual([row('a', 'A')])
        out = io.StringIO()
        with mock.patch('sys.stdout', out):
            self.assertIsNone(kit.after_activation(self.deploy('selected'), self.op))
        self.assertIn('AUTO ONLINE not run: operation is selected', out.getvalue())
        api = FakeApi(['a'])
        with mock.patch.object(kit, 'http_for', return_value=api), mock.patch('sys.stdout', io.StringIO()):
            d = self.deploy()
            d.skip_auto_online = True
            self.assertEqual(kit.after_activation(d, self.op)['state'], 'skipped-by-owner')
            del d.skip_auto_online
            self.assertEqual(kit.after_activation(d, self.op)['state'], 'completed')
        with mock.patch.object(kit, 'Phase', side_effect=RuntimeError('boom secret-token')), \
             mock.patch('sys.stdout', io.StringIO()) as printed:
            self.assertIsNone(kit.after_activation(self.deploy(), self.op))
        self.assertIn('<management-token>', printed.getvalue())
        self.assertEqual(kit.back_online_ids(self.op), {'a'})
        self.assertIn('"state": "completed"', kit.status_line(self.op))

    def test_retry_refuses_rollback_or_incomplete_latest(self):
        self.manual([row('a', 'A')])
        (self.op / 'operation.json').write_text(json.dumps({'phase': 'complete', 'kind': 'rollback'}), encoding='utf-8')
        d = self.deploy('complete')
        (self.op / 'operation.json').write_text(json.dumps({'phase': 'rolled-back', 'kind': 'rollback'}), encoding='utf-8')
        with self.assertRaisesRegex(RuntimeError, 'completed upgrade'):
            kit.retry(d)

    def test_rehearsal_proof(self):
        self.manual([row('a', 'A')])
        self.phase(FakeApi(['a'])).run()
        self.assertEqual(kit.prove_deploy_auto_online(self.op, 'a')['status'], 'online')
        with self.assertRaises(AssertionError):
            kit.prove_deploy_auto_online(self.op, 'missing')


class StagingTests(unittest.TestCase):
    def copy(self, variable):
        source = os.environ.get(variable)
        if not source:
            self.skipTest('requires ' + variable)
        root = Path(tempfile.mkdtemp(prefix='uao-stage-'))
        self.addCleanup(shutil.rmtree, root, True)
        shutil.copytree(source, root / 'kit-tool', ignore=shutil.ignore_patterns('__pycache__', 'operations'))
        return root / 'kit-tool'

    def check(self, kit_tool):
        before = (kit_tool / 'deploy.py').read_bytes()
        applied = staging.stage(kit_tool)
        optional_notice = int(b'After activation, auto-online restores' not in before)
        self.assertEqual(len(applied), len(staging.DEPLOY_EDITS) + 1 - optional_notice)
        after = (kit_tool / 'deploy.py').read_bytes()
        self.assertEqual(b'\r\n' in before, b'\r\n' in after)
        self.assertEqual(after.count(b'\r\n'), before.count(b'\r\n') + (7 if b'\r\n' in before else 0))
        self.assertEqual(staging.stage(kit_tool), [])
        self.assertEqual((kit_tool / 'deploy.py').read_bytes(), after)
        text = after.decode()
        compile(text, 'deploy.py', 'exec')
        self.assertIn("upgrade complete.');self.claude_default();importlib.import_module('upgrade_online_kit').after_activation(self,op)", text)
        self.assertTrue((kit_tool / 'upgrade_online_kit.py').is_file())
        self.assertTrue((kit_tool / 'kit_activation_preflight.py').is_file())
        # The CLI accepts the new mode and flag without touching any host.
        probe = ('import sys;sys.argv=["deploy.py","activate","--no-auto-online"];import deploy;'
                 'deploy.Deploy=lambda:(_ for _ in ()).throw(SystemExit("parsed:"+str(sys.argv)));deploy.main()')
        env = {k: v for k, v in os.environ.items() if not k.upper().startswith('CUTEX')}
        run = subprocess.run([sys.executable, '-B', '-c', probe], cwd=kit_tool, capture_output=True, text=True, env=env)
        self.assertIn("parsed:", run.stderr + run.stdout)
        self.assertIn('set -- activate', (kit_tool / 'owner.sh').read_text())

    def test_stages_r276(self):
        self.check(self.copy('CUTEX_R276_KIT_TOOL_SOURCE'))

    def test_stages_r2761_crlf_beside_prune_protection(self):
        self.check(self.copy('CUTEX_R2761_KIT_TOOL_SOURCE'))

    def test_refuses_unknown_deploy_without_partial_edits(self):
        root = Path(tempfile.mkdtemp(prefix='uao-stage-'))
        self.addCleanup(shutil.rmtree, root, True)
        (root / 'deploy.py').write_text('print(1)\n')
        with self.assertRaisesRegex(ValueError, 'anchor preflight.process-evidence matched 0 times'):
            staging.stage(root)
        self.assertEqual((root / 'deploy.py').read_text(), 'print(1)\n')
        self.assertFalse((root / 'upgrade_online_kit.py').exists())


if __name__ == '__main__':
    unittest.main()
