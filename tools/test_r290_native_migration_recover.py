"""Exercise recovery using the actual staged Deploy.recover/rollback methods.

Set CUTEX_R290_KIT_TOOL_SOURCE to the full unsealed host tool directory.
Services/API writes are fakes; metadata/receipts and interrupted writes are real.
"""
import ast
import copy
import hashlib
import importlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import tempfile
import types
import unittest
from unittest import mock
import r290_native_migration_kit as kit
import stage_r290_native_migration_kit as staging


class RecoveryTests(unittest.TestCase):
    def setUp(self):
        source = os.environ.get('CUTEX_R290_KIT_TOOL_SOURCE')
        if not source:
            self.skipTest('full real deploy source required')
        self.temp = tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        tools = self.root / 'tools';tools.mkdir()
        shutil.copy2(Path(source) / 'deploy.py', tools / 'deploy.py')
        staging.stage(tools)
        first = (tools / 'deploy.py').read_bytes();staging.stage(tools)
        self.assertEqual(first, (tools / 'deploy.py').read_bytes())
        tree = ast.parse(first)
        cls = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == 'Deploy')
        functions = {n.name: n for n in cls.body if isinstance(n, ast.FunctionDef)}
        self.rollback_source=ast.get_source_segment(first.decode(),functions['rollback'])
        self.recover_source=ast.get_source_segment(first.decode(),functions['recover'])
        self.events = []
        self.act = self.root / 'operations/activate';self.act.mkdir(parents=True)
        self.op = self.root / 'operations/rollback';self.op.mkdir()
        self.home = self.root / 'home';self.home.mkdir()
        self.old = self.root / 'old.json';self.old.write_text('{}')
        self.new = self.root / 'new.json';self.new.write_text('{}')
        self.previous = {'version':4,'native_id':'native-id','native_home':str(self.home / 'native'),
                         'bundle_manifest':str(self.old),'bundle_sha256':kit.sha(self.old)}
        self.target = {**self.previous,'bundle_manifest':str(self.new),'bundle_sha256':kit.sha(self.new)}
        self.review = {'current_contract':self.previous,'target_contract':self.target,'subject':{'runtime_generation':3}}
        self.records = [{'cutex_session_id':'agent','codex_session_id':'native-id','explicit_launch':self.target,'revision':3,'updated_at':'before',
                         'runtime_backend':'host','claude_session':None},
                        {'cutex_session_id':'born63','explicit_launch':self.target,'claude_session':{'deployment':{'adapter_source':str(self.root / 'keep52')}}}]
        self.plan = {'release':'r290','nativeUpdate':{'version':kit.TARGET},'installBase':str(self.root / 'install'),
                     'previousCommit':'old-backend','binaryTargets':{'cutex':[]},
                     'nativeMigration':{'from':kit.SOURCE,'to':kit.TARGET,'scope':'all-existing-r62','rollback':'recorded-moves-only'}}
        world = self
        class Journal:
            def run(inner, name, call):world.events.append(name);return call()
        class Deploy:
            root=world.root;home=world.home;plan=world.plan;token='';j=Journal()
            fail_stop=False;fail_fence=False;fail_record_field=None
            def integrity(inner):world.events.append('integrity')
            def latest(inner):return world.op
            def activation(inner):return world.act
            def restore_component_policy(inner, op):world.events.append('policy')
            def persisted(inner):
                inner.session_records=copy.deepcopy(world.records)
                return {'sessions':hashlib.sha256(json.dumps(world.records,sort_keys=True).encode()).hexdigest(),
                        'count':len(world.records),'native':{str(world.paths[0]):kit.sha(world.paths[0])}}
            def record(inner, op, phase, **fields):
                if inner.fail_record_field in fields:
                    inner.fail_record_field=None;raise OSError('injected operation checkpoint failure')
                kit.bind_operation(op,phase=phase,**fields)
            def api(inner, path, method, body):
                world.assertEqual(body['operation'],'revert_runtime_package_upgrade')
                world.assertEqual(body['cutex_session_id'],'agent')
                world.records[0]['explicit_launch']=world.previous
                world.records[0].update(revision=4,updated_at='undo')
                return {'cutex_session_id':'agent','replaced_contract':world.target,'restored_contract':world.previous,'committed_at':'undo','activated_revision':4}
            def service(inner, operation):
                world.events.append('service.'+operation)
                if inner.fail_stop:raise OSError('injected service stop failure')
            def fence(inner):
                if inner.fail_fence:raise OSError('injected fence failure')
            def daemon_guard(inner):return world.guard
            def previous_health(inner, pins, commit):world.assertEqual(inner.persisted(),pins)
            def rollback_steps(inner, op, act, pins):
                world.assertEqual(kit.load(world.paths[0]),world.before[str(world.paths[0])])
                world.assertEqual(kit.load(world.paths[2]),world.before[str(world.paths[2])])
                world.assertEqual(inner.persisted(),pins);world.events.append('backend.restore')
                inner.record(op,'rolled-back')
        self.d = Deploy()
        self.paths = kit.metadata_paths(self.d)
        self.before = {str(self.paths[0]):{'bundle_manifest':str(self.old)},
                       str(self.paths[1]):{'latest_cli':{'version':'62'},'latest_runtime':{'version':'62'},'runtime_artifacts':{'old':'62'}},
                       str(self.paths[2]):{'adapter_source':str(self.root / 'claude51')},
                       str(self.paths[3]):{'claude-runtime':{'selected':'51'}}}
        self.after = copy.deepcopy(self.before)
        self.after[str(self.paths[0])]={'bundle_manifest':str(self.new)}
        self.after[str(self.paths[1])]={'latest_cli':{'version':'63'},'latest_runtime':{'version':'63'},'runtime_artifacts':{'old':'62','new':'63'}}
        self.after[str(self.paths[2])]={'adapter_source':str(self.root / 'claude52')}
        self.after[str(self.paths[3])]={'claude-runtime':{'selected':'52'}}
        for p in self.paths:p.parent.mkdir(parents=True,exist_ok=True);kit.write(p,self.after[str(p)])
        spec=importlib.util.spec_from_file_location('actual_guard',Path(source)/'daemon_only.py')
        self.guard=importlib.util.module_from_spec(spec);spec.loader.exec_module(self.guard)
        report={'policy':self.plan['nativeMigration'],'beforeDefaults':self.before,'afterDefaults':self.after,
                'agents':[{'id':'agent','previous':self.previous,'originalOnline':True,'state':'moved','actionId':'action',
                           'review':self.review,'receipt':{'action_id':'action','review':self.review,'committed_at':'move'}}]}
        kit.write(self.act / kit.NAME,report);kit.write(self.act / kit.INVENTORY,kit.inventory_view(report))
        kit.write(self.act / kit.DEFAULTS_PROOF,{'before':self.before,'after':self.after})
        kit.write(self.act / 'operation.json',{'nativeMigrationInventorySha256':kit.sha(self.act / kit.INVENTORY),
                  'nativeMigrationDefaultsSha256':kit.sha(self.act / kit.DEFAULTS_PROOF)})
        self.d.persisted();kit.write(self.op / 'runtime-selections-before.json',self.guard.snapshot(self.d))
        kit.write(self.op / 'operation.json',{'kind':'rollback','phase':'rollback-stopping','activation':'activate','pins':self.d.persisted(),
                  'runtimeSelectionsBeforeSha256':kit.sha(self.op / 'runtime-selections-before.json')})
        kit.write(self.op / 'previous-build-safety.json',{'signature':'old'})
        namespace={'load':kit.load,'require':kit.require,'importlib':importlib,'Path':Path,'os':os,
                   'private_dir':lambda path, parent:Path(path),'Journal':lambda op, token:Journal(),
                   'runtime_prune_kit':types.SimpleNamespace(invalidate=lambda d:self.events.append('prune')),
                   'module':lambda *args:types.SimpleNamespace(previous_build_gate=lambda *args,**kwargs:{'signature':'old'})}
        for name in ('recover','rollback'):
            method=copy.deepcopy(functions[name]);module=ast.fix_missing_locations(ast.Module(body=[method],type_ignores=[]))
            exec(compile(module,str(tools/'deploy.py'),'exec'),namespace)
            setattr(Deploy,name,namespace[name])

    def validated_revert(self):
        return kit.revert(self.d,self.act,self.op,self.d.persisted())

    def test_staged_rollback_restores_selectors_only_after_stopped_fence(self):
        source=self.rollback_source
        self.assertLess(source.index('.rollback_preflight(self,act)'),source.index("self.j.run('offline'"))
        self.assertLess(source.index("self.j.run('stopped.fence'"),source.index('.restore_defaults(self,act,op,pins)'))
        self.assertLess(self.recover_source.index('.recover_preflight(self,op,r)'),
                        self.recover_source.index("'prune.protection.invalidate'"))

    def test_unknown_apply_blocks_actual_rollback_before_offline(self):
        report=kit.load(self.act/kit.NAME);report['agents'][0]['state']='requested';kit.write(self.act/kit.NAME,report)
        with self.assertRaisesRegex(RuntimeError,'outcome unknown'):self.d.rollback()
        with self.assertRaisesRegex(RuntimeError,'outcome unknown'):self.d.recover()
        self.assertEqual(self.events,['integrity','integrity'])

    def test_missing_after_defaults_refuses_before_offline_or_recovery(self):
        report=kit.load(self.act/kit.NAME);report.pop('afterDefaults');kit.write(self.act/kit.NAME,report)
        with self.assertRaisesRegex(RuntimeError,'missing post-upgrade'):self.d.rollback()
        with self.assertRaisesRegex(RuntimeError,'missing post-upgrade'):self.d.recover()
        self.assertNotIn('service.stop',self.events)

    def test_changed_inventory_refuses_before_offline(self):
        report=kit.load(self.act/kit.NAME);report['agents'][0]['previous']['native_id']='changed';kit.write(self.act/kit.NAME,report)
        with self.assertRaisesRegex(RuntimeError,'inventory changed'):self.d.rollback()
        self.assertEqual(self.events,['integrity'])

    def test_no_revert_receipt_never_starts_previous_backend(self):
        with self.assertRaisesRegex(RuntimeError,'Run rollback again'):self.d.recover()
        self.assertEqual(self.events,['integrity'])

    def test_partial_or_unknown_revert_requires_inspection(self):
        kit.write(self.op/'native-r290-revert.json',{'agents':[{'state':'requested'}]})
        with self.assertRaisesRegex(RuntimeError,'inspect exact receipts'):self.d.recover()
        self.assertEqual(self.events,['integrity'])

    def test_new_rollback_cannot_replay_an_earlier_unknown_revert(self):
        kit.write(self.op/'native-r290-revert.json',{'agents':[{'state':'requested'}]})
        with self.assertRaisesRegex(RuntimeError,'do not retry rollback'):self.d.rollback()
        self.assertEqual(self.events,['integrity'])

    def test_recover_after_stop_failure_uses_reverted_pins_and_restores_selectors(self):
        self.validated_revert();self.d.fail_stop=True
        with self.assertRaisesRegex(OSError,'service stop'):self.d.recover()
        self.assertNotIn('backend.restore',self.events)
        self.d.fail_stop=False;self.d.recover()
        self.assertIn('backend.restore',self.events)
        self.assertEqual(self.records[0]['explicit_launch'],self.previous)
        self.assertEqual(self.records[1]['explicit_launch'],self.target)
        self.assertEqual(self.records[1]['claude_session'],{'deployment':{'adapter_source':str(self.root / 'keep52')}})

    def test_fence_failure_never_writes_selectors_or_restores_backend(self):
        self.validated_revert();self.d.fail_fence=True
        with self.assertRaisesRegex(OSError,'fence'):self.d.recover()
        self.assertEqual(kit.metadata(self.d),self.after)
        self.assertNotIn('backend.restore',self.events)

    def test_partial_selector_write_can_resume_after_stopped_fence(self):
        pins=self.validated_revert();original=kit.write;written=[]
        def fail_second(path,value):
            if Path(path) in self.paths:
                written.append(Path(path))
                if len(written)==2:raise OSError('injected second selector failure')
            return original(path,value)
        with mock.patch.object(kit,'write',side_effect=fail_second):
            with self.assertRaisesRegex(OSError,'second selector'):kit.restore_defaults(self.d,self.act,self.op,pins)
        self.assertEqual(kit.load(self.paths[0]),self.before[str(self.paths[0])])
        self.d.recover()
        self.assertIn('backend.restore',self.events)
        self.assertLess(self.events.index('recover.rollback.fence'),self.events.index('backend.restore'))
        self.assertEqual(kit.load(self.paths[1])['runtime_artifacts'],{'old':'62','new':'63'})

    def test_validated_revert_receipt_closes_operation_checkpoint_gap(self):
        self.d.fail_record_field='nativeRevertReceiptSha256'
        with self.assertRaisesRegex(OSError,'checkpoint'):self.validated_revert()
        self.d.recover()
        self.assertIn('backend.restore',self.events)

    def test_selector_checkpoint_failure_resumes_without_repeat_revert(self):
        pins=self.validated_revert();self.d.fail_record_field='nativeSelectorRestoreSha256'
        with self.assertRaisesRegex(OSError,'checkpoint'):kit.restore_defaults(self.d,self.act,self.op,pins)
        with mock.patch.object(self.d,'api',side_effect=AssertionError('revert must never be replayed')):
            self.d.recover()
        self.assertIn('backend.restore',self.events)

    def test_lost_revert_reply_settles_read_only_without_second_post(self):
        original=self.d.api;calls=[]
        def lost(*args):
            calls.append(args);original(*args);raise TimeoutError('lost revert reply')
        with mock.patch.object(self.d,'api',side_effect=lost):
            with self.assertRaisesRegex(TimeoutError,'lost revert'):self.validated_revert()
        self.assertEqual(len(calls),1)
        with mock.patch.object(self.d,'api',side_effect=AssertionError('must not replay revert')):
            self.d.recover()
        undo=kit.load(self.op/'native-r290-revert.json')
        self.assertTrue(undo['sessionPinsValidated']);self.assertTrue(undo['agents'][0]['settledByReadOnlyStore'])
        self.assertIn('backend.restore',self.events)

    def test_uncommitted_unknown_revert_remains_blocked_without_post(self):
        with mock.patch.object(self.d,'api',side_effect=TimeoutError('before commit')):
            with self.assertRaises(TimeoutError):self.validated_revert()
        with mock.patch.object(self.d,'api',side_effect=AssertionError('must not replay revert')):
            with self.assertRaisesRegex(RuntimeError,'outcome unknown'):self.d.recover()
        self.assertNotIn('backend.restore',self.events)

    def test_legacy_zero_revision_lost_revert_uses_durable_revision_default(self):
        self.records[0]['revision']=0
        def lost(path,method,body):
            self.records[0].update(explicit_launch=self.previous,revision=2,updated_at='undo')
            raise TimeoutError('lost reply')
        with mock.patch.object(self.d,'api',side_effect=lost):
            with self.assertRaises(TimeoutError):self.validated_revert()
        with mock.patch.object(self.d,'api',side_effect=AssertionError('must not replay')):
            self.d.recover()
        self.assertTrue(kit.load(self.op/'native-r290-revert.json')['sessionPinsValidated'])

    def test_unknown_revert_cannot_hide_owner_model_change_or_extra_revision(self):
        original=self.d.api
        def lost(*args):original(*args);raise TimeoutError('lost reply')
        with mock.patch.object(self.d,'api',side_effect=lost):
            with self.assertRaises(TimeoutError):self.validated_revert()
        before=copy.deepcopy(self.records[0])
        for change in [{'model_defaults':'owner-new-model'},{'revision':5}]:
            self.records[0]=dict(before,**change)
            with mock.patch.object(self.d,'api',side_effect=AssertionError('must not replay')):
                with self.assertRaisesRegex(RuntimeError,'conflicting session changes'):self.d.recover()
        self.assertNotIn('backend.restore',self.events)

    def test_lost_reply_recovery_only_sends_never_attempted_rows(self):
        report=kit.load(self.act/kit.NAME)
        other=copy.deepcopy(report['agents'][0]);other['id']='second';report['agents'].append(other)
        kit.write(self.act/kit.NAME,report);kit.write(self.act/kit.INVENTORY,kit.inventory_view(report))
        kit.bind_operation(self.act,nativeMigrationInventorySha256=kit.sha(self.act/kit.INVENTORY))
        self.records.append(dict(self.records[0],cutex_session_id='second'))
        pins=self.d.persisted();kit.bind_operation(self.op,pins=pins)
        calls=[]
        def api(path,method,body):
            sid=body['cutex_session_id'];calls.append(sid)
            record=next(r for r in self.records if r['cutex_session_id']==sid)
            record.update(explicit_launch=self.previous,revision=4,updated_at='undo')
            if sid=='agent':raise TimeoutError('lost first reply')
            return {'cutex_session_id':sid,'replaced_contract':self.target,'restored_contract':self.previous,'committed_at':'undo','activated_revision':4}
        with mock.patch.object(self.d,'api',side_effect=api):
            with self.assertRaises(TimeoutError):self.validated_revert()
            self.d.recover()
        self.assertEqual(calls,['agent','second'])
        self.assertTrue(kit.load(self.op/'native-r290-revert.json')['sessionPinsValidated'])


if __name__=='__main__':unittest.main()
