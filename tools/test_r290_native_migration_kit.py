"""Boundary checks for owner-stop fencing, offline migration and recorded undo."""
import copy
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock
import r290_native_migration_kit as kit
import upgrade_online_kit as online


class MigrationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.op = self.root / 'operation'
        self.op.mkdir()
        self.bundle = self.root / 'selected.json'
        self.bundle.write_text(json.dumps({'native_patch_commit': 'final63', 'executable': {'sha256': 'native63'}}))
        self.plan = {'release': 'r290', 'home': str(self.root), 'installBase': str(self.root / 'install'),
                     'nativeUpdate': {'version': kit.TARGET, 'commit': 'final63', 'artifacts': {'executable': {'sha256': 'native63'}}},
                     'nativeMigration': {'from': kit.SOURCE, 'to': kit.TARGET, 'scope': 'all-existing-r62', 'rollback': 'recorded-moves-only'},
                     'autoOnline': {'minAvailableMiB': 0, 'minAvailablePercent': 0}}
        self.rows = []
        self.sessions = {}
        self.requests = []
        self.previous = {'version': 4, 'native_id': 'id', 'native_home': str(self.root / 'native'),
                         'bundle_manifest': 'old62', 'bundle_sha256': 'oldhash'}
        self.target = {**self.previous, 'bundle_manifest': 'new63', 'bundle_sha256': 'newhash'}
        self.marker = {'available': True, 'generation': 3, 'token': 'kit-stop'}
        self.fail_apply = False
        self.owner_stop_after_apply = False
        self.owner_pin_before_review = False
        self.review_generation = 3

    def add(self, sid, originally_online=True, state='pending'):
        self.rows.append({'id': sid, 'originalOnline': originally_online, 'previous': self.previous,
                          'state': state, 'reason': None})
        self.sessions[sid] = {'cutexSessionId': sid, 'updatedAt': 'stopped',
                              'runtime': {'status': 'offline', 'appServerConnected': False, 'runtimeGeneration': 3}}

    def phase(self, chosen=('a',)):
        online.write(self.op / 'manual-online.json', [
            {'id': sid, 'selectedForOfflineByThisOperation': True, 'offlineOutcome': 'confirmed',
             'runtimeUpdatedAtAfterOffline': 'stopped', 'runtimeStopReceiptAfterOffline': self.marker} for sid in chosen])
        report = {'agents': self.rows, 'beforeDefaults': {}, 'policy': self.plan['nativeMigration']}
        online.write(self.op / kit.NAME, report)
        online.write(self.op / kit.INVENTORY, kit.inventory_view(report))
        online.write(self.op / 'operation.json', {'phase': 'complete', 'nativeMigrationInventorySha256': kit.sha(self.op / kit.INVENTORY)})
        phase = online.Phase(self.op, self.plan, self.api, out=lambda line: None)
        phase.begin(False)
        return phase

    def api(self, path, method='GET', body=None, timeout=60):
        self.requests.append((path, method, copy.deepcopy(body)))
        if method == 'GET':
            return copy.deepcopy(self.sessions[path.rsplit('/', 1)[1]])
        sid = (body.get('cutex_session_id') or body.get('review', {}).get('subject', {}).get('cutex_session_id'))
        if body['operation'] == 'review_runtime_package_upgrade':
            return {'subject': {'cutex_session_id': sid, 'runtime_generation': self.review_generation},
                    'current_contract': {} if self.owner_pin_before_review else self.previous,
                    'target_contract': self.target, 'current_runtime_version': kit.SOURCE,
                    'target_runtime_version': kit.TARGET, 'selected_manifest': str(self.bundle),
                    'selected_manifest_sha256': kit.sha(self.bundle)}
        if body['operation'] == 'apply_runtime_package_upgrade':
            self.sessions[sid]['updatedAt'] = 'owner-newer' if self.owner_stop_after_apply else 'committed'
            if self.fail_apply:
                raise TimeoutError('unknown outcome')
            return {'action_id': body['action_id'], 'review': body['review'], 'committed_at': 'committed'}
        raise AssertionError(body)

    def run_forward(self, phase):
        with mock.patch.object(online, 'runtime_stop_marker', return_value=self.marker):
            phase.runtime()
        return kit.load(self.op / kit.NAME)

    def test_online_and_offline_migrate_but_only_owned_stop_fence_advances(self):
        self.add('a'); self.add('cold', False)
        phase = self.phase()
        report = self.run_forward(phase)
        self.assertEqual(report['counts'], {'migratedOnline': 1, 'migratedOffline': 1, 'skipped': 0, 'skippedArchived':0,'skippedOther':0,'unknown': 0, 'failed': 0})
        self.assertEqual(phase.agent('a')['runtimeUpdatedAtAfterOffline'], 'committed')
        self.assertEqual([a['id'] for a in phase.result['agents']], ['a'])
        self.assertFalse(any('/runtime-packages/runs' in p for p, _, _ in self.requests))

    def test_owner_stop_before_migration_still_migrates_but_never_authorizes_online(self):
        self.add('a'); phase = self.phase()
        self.sessions['a']['updatedAt'] = 'owner-newer'
        self.assertEqual(self.run_forward(phase)['agents'][0]['state'], 'moved')
        self.assertIn('a', phase.blocked)
        self.assertEqual(phase.agent('a')['runtimeUpdatedAtAfterOffline'], 'stopped')

    def test_owner_stop_during_migration_does_not_advance_fence(self):
        self.add('a'); phase = self.phase(); self.owner_stop_after_apply = True
        self.run_forward(phase)
        self.assertIn('a', phase.blocked)
        self.assertEqual(phase.agent('a')['runtimeUpdatedAtAfterOffline'], 'stopped')

    def test_repeated_stop_receipt_blocks_even_without_timestamp_change(self):
        self.add('a'); phase = self.phase()
        changed = {**self.marker, 'token': 'owner-stop'}
        with mock.patch.object(online, 'runtime_stop_marker', return_value=changed):
            phase.runtime()
        self.assertIn('a', phase.blocked)

    def test_unknown_apply_is_not_replayed(self):
        self.add('a'); phase = self.phase(); self.fail_apply = True
        self.assertEqual(self.run_forward(phase)['counts']['unknown'], 1)
        self.run_forward(phase)
        self.assertEqual(sum((b or {}).get('operation') == 'apply_runtime_package_upgrade' for _, _, b in self.requests), 1)
        self.assertIn('a', phase.blocked)

    def test_older_native_pin_is_reported_without_request(self):
        self.add('older', False, 'skipped'); phase = self.phase(())
        self.assertEqual(self.run_forward(phase)['counts']['skipped'], 1)
        self.assertEqual(self.requests, [])

    def test_archived_r62_is_frozen_as_skipped_without_runtime_review(self):
        manifest=self.root/'archived-r62.json';kit.write(manifest,{'native_patch_commit':kit.SOURCE_COMMIT})
        contract={**self.previous,'bundle_manifest':str(manifest),'bundle_sha256':kit.sha(manifest)}
        outer=self
        class Deploy:
            plan=outer.plan;home=outer.root
            session_records=[{'cutex_session_id':'a','explicit_launch':contract,'registration_class':'persistent','lifecycle':'retired'}]
            def persisted(inner):return {}
            def sessions(inner):return [{'cutexSessionId':'a','lifecycle':'retired','retiredAt':'2026-11-18T03:00:00Z','runtime':{'appServerConnected':False}}]
            def record(inner,op,phase,**fields):kit.bind_operation(op,**fields)
        kit.write(self.op/'operation.json',{'phase':'preparing'})
        kit.capture(Deploy(),self.op)
        kit.write(self.op/'manual-online.json',[])
        phase=online.Phase(self.op,self.plan,self.api,out=lambda line:None);phase.begin(False)
        report=self.run_forward(phase)
        self.assertEqual(report['agents'][0]['state'],'skipped')
        self.assertEqual(report['agents'][0]['reason'],'archived')
        self.assertTrue(report['agents'][0]['archivedAtCapture'])
        self.assertEqual(report['counts']['skippedArchived'],1)
        self.assertEqual(report['counts']['skippedOther'],0)
        self.assertEqual(self.requests,[])

    def test_archive_after_capture_skips_without_package_apply(self):
        self.add('a');phase=self.phase();self.sessions['a'].update(lifecycle='retired',retiredAt='2026-11-18T03:00:00Z')
        report=self.run_forward(phase)
        self.assertEqual(report['agents'][0]['state'],'skipped')
        self.assertEqual(report['agents'][0]['reason'],'archived')
        self.assertFalse(any(path==kit.API for path,_,_ in self.requests))

    def test_archived_real_resource_shape_never_requests_online(self):
        self.add('a');phase=self.phase();self.sessions['a'].update(lifecycle='retired',retiredAt='2026-11-18T03:00:00Z')
        phase.online_inner('a')
        self.assertEqual(phase.agent('a')['code'],'notActive')
        self.assertFalse(any(path.endswith('/cutex/requests') for path,_,_ in self.requests))

    def test_hidden_archive_uses_explicit_retired_get_without_migration_or_start(self):
        self.add('a');phase=self.phase();resource=self.sessions['a']
        resource.update(lifecycle='retired',retiredAt='2026-11-18T03:00:00Z');calls=[]
        def api(path,method='GET',body=None,timeout=60):
            calls.append((path,method,body))
            if path=='/v2/sessions/a':raise online.HttpError(404,{'error':{'code':'not_found','message':'hidden by active view'}})
            if path=='/v2/sessions/a?lifecycle=retired':return copy.deepcopy(resource)
            raise AssertionError('archived agent must never be migrated or started')
        phase.http=api
        with mock.patch.object(online,'runtime_stop_marker',return_value=self.marker):phase.runtime()
        self.assertEqual(kit.load(self.op/kit.NAME)['counts']['skippedArchived'],1)
        phase.online_inner('a');self.assertEqual(phase.agent('a')['code'],'notActive')
        self.assertTrue(all(method=='GET' for _,method,_ in calls))

    def test_owner_changed_pin_is_not_overwritten(self):
        self.add('a'); phase = self.phase(); self.owner_pin_before_review = True
        self.assertEqual(self.run_forward(phase)['counts']['failed'], 1)
        self.assertFalse(any((b or {}).get('operation') == 'apply_runtime_package_upgrade' for _, _, b in self.requests))

    def test_generation_change_between_get_and_review_blocks_apply(self):
        self.add('a'); phase = self.phase(); self.review_generation = 4
        self.assertEqual(self.run_forward(phase)['counts']['failed'], 1)
        self.assertFalse(any((b or {}).get('operation') == 'apply_runtime_package_upgrade' for _, _, b in self.requests))

    def test_failure_before_requested_can_retry_with_fresh_review(self):
        self.add('a'); phase = self.phase(); self.owner_pin_before_review = True
        self.assertEqual(self.run_forward(phase)['counts']['failed'], 1)
        self.owner_pin_before_review = False
        self.assertEqual(self.run_forward(phase)['counts']['migratedOnline'], 1)
        self.assertNotIn('a', phase.blocked)

    def test_failed_report_write_after_move_does_not_overwrite_moved(self):
        self.add('a'); phase = self.phase(); original = kit.write; failed = False
        def write_once(path, value):
            nonlocal failed
            if Path(path).name == kit.NAME and value['agents'][0]['state'] == 'moved' and not failed:
                failed = True
                raise OSError('injected local report write failure')
            return original(path, value)
        with mock.patch.object(kit, 'write', side_effect=write_once):
            report = self.run_forward(phase)
        self.assertEqual(report['agents'][0]['state'], 'moved')
        self.assertEqual(report['counts']['failed'], 0)

    def test_requested_can_settle_from_receipt_and_committed_store_without_post(self):
        self.add('a'); phase = self.phase(); self.fail_apply = True
        report = self.run_forward(phase); row = report['agents'][0]
        path = Path(self.target['native_home']) / 'runtime-package-upgrades' / (hashlib.sha256(row['actionId'].encode()).hexdigest() + '.json')
        path.parent.mkdir(parents=True)
        kit.write(path, {'action_id': row['actionId'], 'review': row['review'], 'activated_revision': 7, 'committed_at': 'committed'})
        records = [{'cutex_session_id': 'a', 'explicit_launch': self.target, 'revision': 7}]
        with mock.patch.object(kit, 'read_records', return_value=records):
            report = self.run_forward(phase)
        self.assertEqual(report['agents'][0]['state'], 'moved')
        self.assertTrue(report['agents'][0]['settledByReadOnlyReceipt'])
        self.assertEqual(sum((b or {}).get('operation') == 'apply_runtime_package_upgrade' for _, _, b in self.requests), 1)
        self.assertEqual(phase.agent('a')['runtimeUpdatedAtAfterOffline'], 'committed')

    def test_receipt_before_sql_commit_cannot_settle_requested(self):
        self.add('a'); phase = self.phase(); self.fail_apply = True
        row = self.run_forward(phase)['agents'][0]
        path = Path(self.target['native_home']) / 'runtime-package-upgrades' / (hashlib.sha256(row['actionId'].encode()).hexdigest() + '.json')
        path.parent.mkdir(parents=True)
        kit.write(path, {'action_id': row['actionId'], 'review': row['review'], 'activated_revision': 7, 'committed_at': 'committed'})
        records = [{'cutex_session_id': 'a', 'explicit_launch': self.previous, 'revision': 6}]
        with mock.patch.object(kit, 'read_records', return_value=records):
            report = self.run_forward(phase)
        self.assertEqual(report['counts']['unknown'], 1)
        self.assertEqual(sum((b or {}).get('operation') == 'apply_runtime_package_upgrade' for _, _, b in self.requests), 1)

    def test_no_auto_online_still_runs_authorized_migration(self):
        self.add('cold', False); phase = self.phase(())
        with mock.patch.object(online, 'runtime_stop_marker', return_value=self.marker), \
                mock.patch.object(phase, 'waiting'), mock.patch.object(phase, 'report'):
            phase.run(skip=True)
        self.assertEqual(kit.load(self.op / kit.NAME)['counts']['migratedOffline'], 1)
        self.assertFalse(any(p.endswith('/cutex/requests') for p, _, _ in self.requests))

    def test_revert_only_owned_moves_keeps_new63_and_claude_pins(self):
        self.add('a'); phase = self.phase(); self.run_forward(phase)
        rollback = self.root / 'rollback'; rollback.mkdir()
        kit.write(rollback / 'operation.json', {'phase': 'rollback-stopping'})
        lenses = {'a': {'explicitLaunch': self.target, 'claude': 'unchanged'},
                  'born63': {'explicitLaunch': self.target, 'claude': None},
                  'claude': {'explicitLaunch': None, 'claude': 'r51'}}
        class Deploy:
            plan = self.plan
            session_records=[{'cutex_session_id':'a','explicit_launch':self.target,'revision':3,'updated_at':'before'}]
            def persisted(inner): return {'hash': 'current'}
            def record(inner, op, phase, **fields): kit.bind_operation(op, **fields)
            def api(inner, path, method, body):
                self.assertEqual(body['cutex_session_id'], 'a')
                lenses['a']['explicitLaunch'] = self.previous
                return {'cutex_session_id': 'a', 'replaced_contract': self.target, 'restored_contract': self.previous, 'committed_at': 'undo'}
        with mock.patch.object(kit, 'pin_lenses', side_effect=lambda d: copy.deepcopy(lenses)):
            kit.revert(Deploy(), self.op, rollback, {'hash': 'current'})
        self.assertEqual(lenses['born63']['explicitLaunch'], self.target)
        self.assertEqual(lenses['claude']['claude'], 'r51')
        with self.assertRaisesRegex(RuntimeError, 'already attempted'):
            kit.revert(Deploy(), self.op, rollback, {})

    def test_unknown_apply_blocks_rollback_before_revert(self):
        self.add('a'); phase = self.phase(); self.fail_apply = True; self.run_forward(phase)
        class Deploy:
            plan = self.plan
        with self.assertRaisesRegex(RuntimeError, 'outcome unknown'):
            kit.revert(Deploy(), self.op, self.root, {})

    def test_capture_only_persistent_agents_not_local_ephemeral_or_missing_registration(self):
        manifest=self.root/'r62.json';kit.write(manifest,{'native_patch_commit':kit.SOURCE_COMMIT})
        contract={**self.previous,'bundle_manifest':str(manifest),'bundle_sha256':kit.sha(manifest)}
        outer=self
        class Deploy:
            plan=outer.plan;home=outer.root
            session_records=[{'cutex_session_id':name,'explicit_launch':contract,**fields} for name,fields in [
                ('managed',{'registration_class':'persistent'}),('local',{'registration_class':'local_only'}),
                ('ephemeral',{'registration_class':'ephemeral'}),('legacy',{}),
                ('archived',{'registration_class':'persistent','lifecycle':'retired'})]]
            def persisted(inner):return {}
            def sessions(inner):return []
            def record(inner,op,phase,**fields):kit.bind_operation(op,**fields)
        kit.write(self.op/'operation.json',{'phase':'preparing'})
        kit.capture(Deploy(),self.op)
        rows=kit.load(self.op/kit.NAME)['agents']
        self.assertEqual([r['id'] for r in rows],['managed','archived'])
        self.assertEqual([r['state'] for r in rows],['pending','skipped'])
        self.assertEqual(rows[1]['reason'],'archived')

    def test_invalid_model_effort_review_never_records_or_sends_apply(self):
        self.add('gmn-2');phase=self.phase(('gmn-2',));api=phase.http
        def refuse_review(path,method='GET',body=None,timeout=60):
            if method=='POST' and body.get('operation')=='review_runtime_package_upgrade':
                self.requests.append((path,method,copy.deepcopy(body)))
                raise online.HttpError(400,{'error':{'code':'invalid_request','message':'reasoning effort xhigh is not offered by model gemini-4-argon; choose one of: low, medium, high'}})
            return api(path,method,body,timeout)
        phase.http=refuse_review
        report=self.run_forward(phase)
        self.assertEqual(report['counts']['failed'],1)
        self.assertNotIn('actionId',report['agents'][0])
        self.assertFalse(any((b or {}).get('operation')=='apply_runtime_package_upgrade' for _,_,b in self.requests))
        self.assertIn('xhigh',report['agents'][0]['reason'])

    def test_phase_wide_review_refusal_in_second_agent_means_zero_moves(self):
        self.add('good');self.add('gmn-2');phase=self.phase(('good','gmn-2'));api=phase.http
        def refuse_second(path,method='GET',body=None,timeout=60):
            if method=='POST' and body.get('operation')=='review_runtime_package_upgrade' and body.get('cutex_session_id')=='gmn-2':
                self.requests.append((path,method,copy.deepcopy(body)))
                raise online.HttpError(400,{'error':{'code':'invalid_request','message':'xhigh is not offered by model gemini-4-argon'}})
            return api(path,method,body,timeout)
        phase.http=refuse_second;report=self.run_forward(phase)
        self.assertEqual(report['preflight']['state'],'refused')
        self.assertEqual(report['preflight']['reviewedAgents'],['good'])
        self.assertEqual(report['preflight']['refusedAgents'],['gmn-2'])
        self.assertEqual([r['state'] for r in report['agents']],['pending','failed'])
        self.assertTrue(all('actionId' not in r for r in report['agents']))
        self.assertFalse(any((body or {}).get('operation')=='apply_runtime_package_upgrade' for _,_,body in self.requests))
        self.assertTrue({'good','gmn-2'}<=phase.blocked)

    def test_phase_wide_success_reviews_all_agents_before_first_apply(self):
        self.add('a');self.add('b');phase=self.phase(('a','b'));report=self.run_forward(phase)
        operations=[body.get('operation') for _,method,body in self.requests if method=='POST']
        self.assertEqual(operations,['review_runtime_package_upgrade','review_runtime_package_upgrade',
                                     'apply_runtime_package_upgrade','apply_runtime_package_upgrade'])
        self.assertEqual(report['counts']['migratedOnline'],2)
        self.assertEqual(report['preflight']['state'],'passed')

    def test_defaults_capture_recovers_each_local_checkpoint_failure(self):
        self.add('a');phase=self.phase();initial=kit.load(self.op/kit.NAME)
        original=kit.write
        for name in [kit.NAME,kit.DEFAULTS_PROOF,'operation.json']:
            with self.subTest(checkpoint=name):
                original(self.op/kit.NAME,initial)
                (self.op/kit.DEFAULTS_PROOF).unlink(missing_ok=True)
                operation=kit.load(self.op/'operation.json');operation.pop('nativeMigrationDefaultsSha256',None);original(self.op/'operation.json',operation)
                failed=False
                def fail_after_write(path,value):
                    nonlocal failed
                    original(path,value)
                    if Path(path).name==name and not failed:
                        failed=True;raise OSError('crash after checkpoint')
                with mock.patch.object(kit,'write',side_effect=fail_after_write):
                    with self.assertRaisesRegex(OSError,'checkpoint'):kit.capture_defaults(phase,copy.deepcopy(initial))
                report=kit.load(self.op/kit.NAME)
                kit.capture_defaults(phase,report);kit.verify_defaults(self.op,report)

    def test_legacy_proof_before_report_can_resume_without_recapturing(self):
        self.add('a');phase=self.phase();report=kit.load(self.op/kit.NAME)
        live={str(p):None for p in kit.metadata_paths_for_phase(phase)}
        kit.write(self.op/kit.DEFAULTS_PROOF,{'before':report['beforeDefaults'],'after':live})
        kit.capture_defaults(phase,report);kit.verify_defaults(self.op,report)
        self.assertEqual(report['afterDefaults'],live)

    def test_interrupted_defaults_capture_refuses_owner_changed_defaults(self):
        self.add('a');phase=self.phase();report=kit.load(self.op/kit.NAME)
        live={str(p):None for p in kit.metadata_paths_for_phase(phase)}
        kit.write(self.op/kit.DEFAULTS_PROOF,{'before':report['beforeDefaults'],'after':live})
        selected=kit.metadata_paths_for_phase(phase)[0];selected.parent.mkdir(parents=True);kit.write(selected,{'owner':'changed'})
        with self.assertRaisesRegex(RuntimeError,'changed during interrupted'):kit.capture_defaults(phase,report)
        self.assertNotIn('afterDefaults',kit.load(self.op/kit.NAME))

    def test_selector_rollback_retains_r63_registry_and_rebinds_strict_snapshot(self):
        rollback = self.root / 'rollback'; rollback.mkdir()
        old_bundle = self.root / 'old.json'; old_bundle.write_text('{}')
        new_bundle = self.root / 'new.json'; new_bundle.write_text('{}')
        class Deploy:
            home = self.root
            plan = self.plan
            def persisted(inner):
                return {'sessions': 'same-pins', 'count': 3,
                        'native': {str(inner.home / 'runtime/light/deployment.json'): kit.sha(inner.home / 'runtime/light/deployment.json')}}
            def daemon_guard(inner): return guard
            def record(inner, op, phase, **kw):
                record = kit.load(op / 'operation.json'); record.update(kw); kit.write(op / 'operation.json', record)
        d = Deploy()
        paths = kit.metadata_paths(d)
        before = {str(paths[0]): {'bundle_manifest': str(old_bundle)},
                  str(paths[1]): {'latest_cli': {'version': '62'}, 'latest_runtime': {'version': '62'}, 'runtime_artifacts': {'old': '62'}},
                  str(paths[2]): {'adapter_source': str(self.root / 'claude51')},
                  str(paths[3]): {'claude-runtime': {'selected': '51'}, 'kimi-runtime': {'selected': 'r3'}}}
        after = copy.deepcopy(before)
        after[str(paths[0])] = {'bundle_manifest': str(new_bundle)}
        after[str(paths[1])] = {'latest_cli': {'version': '63'}, 'latest_runtime': {'version': '63'}, 'runtime_artifacts': {'old': '62', 'new': '63'}}
        after[str(paths[2])] = {'adapter_source': str(self.root / 'claude52')}
        after[str(paths[3])]['claude-runtime'] = {'selected': '52'}
        for p in paths:
            p.parent.mkdir(parents=True, exist_ok=True); kit.write(p, after[str(p)])
        class Guard:
            def snapshot(inner, context):
                default = kit.load(paths[0])['bundle_manifest']
                return {'files': {**{str(p): kit.sha(p) for p in paths}, default: kit.sha(default)},
                        'protectedPaths': [default, kit.load(paths[2])['adapter_source']]}
        guard = Guard()
        kit.write(self.op / kit.NAME, {'beforeDefaults': before, 'afterDefaults': after, 'agents': []})
        report=kit.load(self.op / kit.NAME);report['policy']=self.plan['nativeMigration'];kit.write(self.op / kit.NAME,report)
        kit.write(self.op / kit.INVENTORY,kit.inventory_view(report))
        kit.write(self.op / kit.DEFAULTS_PROOF,{'before':before,'after':after})
        kit.write(self.op / 'operation.json',{'nativeMigrationInventorySha256':kit.sha(self.op / kit.INVENTORY),
                  'nativeMigrationDefaultsSha256':kit.sha(self.op / kit.DEFAULTS_PROOF)})
        kit.write(rollback / 'native-r290-revert.json', {'agents': [], 'sessionPinsValidated': True})
        kit.write(rollback / 'runtime-selections-before.json', guard.snapshot(d))
        kit.write(rollback / 'operation.json', {'phase': 'rollback-stopping',
                  'nativeRevertReceiptSha256':kit.sha(rollback / 'native-r290-revert.json'),
                  'runtimeSelectionsBeforeSha256': kit.sha(rollback / 'runtime-selections-before.json')})
        result = kit.restore_defaults(d, self.op, rollback, d.persisted())
        registry = kit.load(paths[1])
        self.assertEqual(registry['latest_runtime'], {'version': '62'})
        self.assertEqual(registry['runtime_artifacts'], {'old': '62', 'new': '63'})
        self.assertEqual(kit.load(paths[2]), before[str(paths[2])])
        self.assertEqual(kit.load(paths[3])['claude-runtime'], {'selected': '51'})
        self.assertEqual(kit.load(rollback / 'runtime-selections-before.json'), guard.snapshot(d))
        self.assertTrue((rollback / 'runtime-selections-original.json').is_file())
        self.assertEqual(result['sessions'], 'same-pins')


if __name__ == '__main__':
    unittest.main()
