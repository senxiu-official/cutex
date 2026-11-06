"""Synthetic proof + patched real r276 deploy flow, without services/credentials.

Set CUTEX_R276_KIT_TOOL_SOURCE to the read-only r276 kit-tool source directory
to run the patch/integration cases as well as the portable proof tests.
"""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import types
import unittest
from unittest.mock import patch

import runtime_prune_kit as kit
import runtime_prune_protection as protection
import stage_runtime_prune_kit as staging
import test_runtime_prune_protection as fixtures


class KitProofTests(unittest.TestCase):
    def setUp(self):
        self.f = fixtures.ProtectionTests()
        self.f.setUp()
        self.addCleanup(self.f.doCleanups)
        self.d = types.SimpleNamespace(root=self.f.op.parent.parent, home=self.f.home,
            plan={'selectedImage': str(self.f.active_executable), 'installBase': str(self.f.base)})

    def test_publish_and_invalidate_bind_actual_completed_operation(self):
        report = kit.publish(self.d, self.f.op)
        index = protection.read(kit.output(self.d))
        self.assertEqual(report['status'], 'exported')
        self.assertEqual(index['kitOperation'], str(self.f.op))
        self.assertEqual(set(index['protectedPaths']), self.f.expected)
        self.assertEqual(index['provenance']['operationSha256'], protection.digest(self.f.op/'operation.json'))
        kit.invalidate(self.d)
        self.assertEqual(protection.read(kit.output(self.d))['schema'],
                         'cutex/runtime-prune-protection/invalidated')

    def test_recovered_previous_binds_previous_actual_binary(self):
        self.f.record['phase'] = 'recovered-previous'
        self.f.save()
        previous = self.f.root/'previous-cutex'
        previous.write_bytes(b'synthetic previous binary')
        self.d.plan['selectedImage'] = str(previous)
        kit.publish(self.d, self.f.op)
        self.assertEqual(protection.read(kit.output(self.d))['activeExecutableSha256'], protection.digest(previous))

    def test_nonfinal_or_corrupt_evidence_keeps_cleanup_invalidated(self):
        kit.invalidate(self.d)
        self.f.record['phase'] = 'selected'
        self.f.save()
        with self.assertRaises(protection.ProtectionError):
            kit.publish(self.d, self.f.op)
        self.f.record['phase'] = 'complete'
        self.f.save()
        (self.f.op/'backup/runtime/light/deployment.json').write_bytes(b'changed')
        with self.assertRaises(protection.ProtectionError):
            kit.publish(self.d, self.f.op)
        self.assertEqual(protection.read(kit.output(self.d))['schema'],
                         'cutex/runtime-prune-protection/invalidated')

    def test_cannot_publish_some_other_kits_operation(self):
        self.d.root = self.f.root/'other-kit'
        (self.d.root/'operations').mkdir(parents=True)
        with self.assertRaisesRegex(protection.ProtectionError, 'belong to this kit'):
            kit.publish(self.d, self.f.op)


@unittest.skipUnless(os.environ.get('CUTEX_R276_KIT_TOOL_SOURCE'), 'requires read-only r276 deploy source')
class PatchedDeployTests(KitProofTests):
    def setUp(self):
        super().setUp()
        self.root = self.d.root
        source = Path(os.environ['CUTEX_R276_KIT_TOOL_SOURCE'])
        self.original = (source/'deploy.py').read_bytes()
        (self.root/'deploy.py').write_bytes(self.original)
        staging.stage(self.root)
        spec = importlib.util.spec_from_file_location('synthetic_prune_deploy', self.root/'deploy.py')
        self.deploy = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.deploy)
        self.d = self.deploy.Deploy.__new__(self.deploy.Deploy)
        self.d.root = self.root
        self.d.home = self.f.home
        self.d.plan = {'selectedImage':str(self.f.active_executable), 'installBase':str(self.f.base)}
        self.d.token = 'synthetic-management-token'
        self.d.managed_roots = []
        self.d.j = self.deploy.Journal(self.f.op, self.d.token)
        self.events = []
        self.d.integrity = lambda: self.events.append('integrity')
        self.d.latest = lambda: self.f.op
        self.d.restore_component_policy = lambda op: self.events.append('restore-policy')
        self.d.service = self.service
        self.d.current_readiness = lambda: self.events.append('ready')
        self.d.ready_retry = lambda fn: fn()

    def service(self, action):
        self.assertEqual(protection.read(kit.output(self.d))['schema'],
                         'cutex/runtime-prune-protection/invalidated')
        self.events.append('service-'+action)

    def test_patch_staging_is_idempotent_and_does_not_edit_source(self):
        first = (self.root/'deploy.py').read_bytes()
        staging.stage(self.root)
        self.assertEqual((self.root/'deploy.py').read_bytes(), first)
        self.assertEqual((Path(os.environ['CUTEX_R276_KIT_TOOL_SOURCE'])/'deploy.py').read_bytes(), self.original)
        for name in ['runtime_prune_protection.py','runtime_prune_kit.py']:
            self.assertEqual((self.root/name).read_bytes(), (staging.TOOLS/name).read_bytes())
        # Owner wrappers run Python isolated; helper import must follow the
        # deploy script's explicit sys.path insertion. --help opens no config.
        result = subprocess.run([sys.executable,'-I','-B',str(self.root/'deploy.py'),'--help'],
                                capture_output=True,text=True)
        self.assertEqual(result.returncode,0,result.stderr)

    def test_completed_recovery_republishes_without_changing_operation(self):
        before = (self.f.op/'operation.json').read_bytes()
        self.d.recover()
        self.assertEqual(self.events, ['integrity','restore-policy','service-start','ready'])
        self.assertEqual((self.f.op/'operation.json').read_bytes(), before)
        self.assertEqual(protection.read(kit.output(self.d))['schema'], protection.SCHEMA)

    def test_staging_a_kit_nested_inside_a_checkout_applies_instead_of_skipping(self):
        subprocess.run(['git','init','-q',str(self.f.root)],check=True)
        nested = self.f.root/'build'/'kit-tool'
        nested.mkdir(parents=True)
        (nested/'deploy.py').write_bytes(self.original)
        staging.stage(nested)
        self.assertIn(b'runtime_prune_kit.publish(self,op)',(nested/'deploy.py').read_bytes())

    def test_final_complete_publishes_after_final_record(self):
        self.f.record['phase'] = 'selected'
        self.f.save()
        self.d.complete_operation(self.f.op)
        self.assertEqual(protection.read(self.f.op/'operation.json')['phase'], 'complete')
        self.assertEqual(protection.read(kit.output(self.d))['provenance']['operationSha256'],
                         protection.digest(self.f.op/'operation.json'))

    def test_unchanged_recovery_invalidates_before_start_and_then_publishes(self):
        self.f.record.update(phase='stopping', previous={'commit':'synthetic-previous'})
        self.f.save()
        self.d.swap_state = lambda op: {'changed':False,'complete':False,'entries':[]}
        self.d.previous_health = lambda pins, commit: self.events.append('previous-ready')
        self.assertEqual(self.d.reconcile(self.f.op), 'unchanged-previous')
        self.assertEqual(self.events, ['restore-policy','service-start','previous-ready'])
        self.assertEqual(protection.read(self.f.op/'operation.json')['phase'], 'recovered-previous')
        self.assertEqual(protection.read(kit.output(self.d))['schema'], protection.SCHEMA)

    def test_forward_recovery_runs_selection_start_post_and_final_publication(self):
        self.f.record.update(phase='swapping', previous={'commit':'synthetic-previous'})
        self.f.save()
        self.d.swap_state = lambda op: {'changed':True,'complete':False,'entries':[{}]}
        self.d.fence = lambda: self.events.append('fence')
        self.d.swap = lambda op: self.events.append('swap')
        self.d.verify_pins = lambda pins, op: self.events.append('verify-pins')
        self.d.select_component_stage = lambda op: self.events.append('select')
        self.d.input_ledger_step = lambda op: self.events.append('ledger')
        self.d.mark_backfill_log = lambda op: self.events.append('mark')
        self.d.migrate_stores = lambda op: self.events.append('migrate')
        self.d.post = lambda pins: self.events.append('post')
        self.d.read_backfill = lambda op: self.events.append('backfill')
        self.assertEqual(self.d.reconcile(self.f.op), 'completed-forward')
        self.assertLess(self.events.index('swap'), self.events.index('select'))
        self.assertLess(self.events.index('select'), self.events.index('service-start'))
        self.assertLess(self.events.index('post'), self.events.index('backfill'))
        self.assertEqual(protection.read(self.f.op/'operation.json')['phase'], 'complete')
        self.assertEqual(protection.read(kit.output(self.d))['schema'], protection.SCHEMA)

    def test_export_failure_keeps_final_phase_for_safe_completed_recovery(self):
        kit.invalidate(self.d)
        (self.f.op/'component-prepared.json').write_bytes(b'changed')
        with self.assertRaises(protection.ProtectionError):
            self.d.complete_operation(self.f.op)
        self.assertEqual(protection.read(self.f.op/'operation.json')['phase'], 'complete')
        self.assertEqual(protection.read(kit.output(self.d))['schema'],
                         'cutex/runtime-prune-protection/invalidated')

    def activation_fixture(self, fail_swap=False):
        kit.publish(self.d,self.f.op)
        self.d.plan['host'] = 'synthetic'
        self.d.check = lambda: {'pins':self.f.record['pins']}
        self.d.inherited_manual = lambda: ([],[])
        self.d.api = lambda path: {'cutexBuild':{'commit':'synthetic-previous'}}
        self.d.idle = lambda wait=False: ([],None)
        self.d.capture_roots = lambda rows: None
        self.d.offline = lambda op, rows: self.events.append('offline')
        self.d.fence = lambda: self.events.append('fence')
        self.d.storage = lambda: None
        self.d.persisted = lambda: self.f.record['pins']
        self.d.prepare_images = lambda op: None
        def prepare(op):
            shutil.copyfile(self.f.op/'component-prepared.json',op/'component-prepared.json')
            shutil.copyfile(self.f.op/'native-managed-before.json',op/'native-managed-before.json')
            self.d.record(op,'preparing', componentPreparedSha256=self.f.record['componentPreparedSha256'],
                componentPreparationReport=self.f.prepared,
                nativeManagedBeforeSha256=self.f.record['nativeManagedBeforeSha256'])
        self.d.prepare_component_stage = prepare
        def backup(op):
            for name in ['backup','originals']:
                shutil.copytree(self.f.op/name,op/name,dirs_exist_ok=True)
            for name in ['backup-manifest.json','originals-manifest.json']:
                shutil.copyfile(self.f.op/name,op/name)
        self.d.backup = backup
        def service(action):
            schema = protection.read(kit.output(self.d))['schema']
            self.assertEqual(schema, protection.SCHEMA if action=='stop'
                             else 'cutex/runtime-prune-protection/invalidated')
            self.events.append('service-'+action)
        self.d.service = service
        def swap(op):
            self.assertEqual(protection.read(kit.output(self.d))['schema'],
                             'cutex/runtime-prune-protection/invalidated')
            self.events.append('swap')
            if fail_swap:
                raise RuntimeError('synthetic-before-image-change')
        self.d.swap = swap
        self.d.verify_pins = lambda pins,op: None
        self.d.select_component_stage = lambda op: self.events.append('select')
        self.d.input_ledger_step = lambda op: None
        self.d.mark_backfill_log = lambda op: None
        self.d.migrate_stores = lambda op: self.events.append('migrate')
        self.d.post = lambda pins: self.events.append('post')
        self.d.read_backfill = lambda op: self.events.append('backfill')
        self.d.previous_health = lambda pins,commit: self.events.append('previous-ready')
        self.d.swap_state = lambda op: {'changed':False,'complete':False,'entries':[]}
        self.d.summary = lambda: None
        self.d.claude_default = lambda: None

    def activate(self):
        capture = types.SimpleNamespace(capture_mcp=lambda plan: {})
        with patch('builtins.input', return_value='ACTIVATE r276 synthetic'), \
             patch.object(self.deploy,'module',return_value=capture):
            self.d.activate()

    def test_activation_invalidates_after_stop_before_swap_and_publishes_after_post(self):
        self.activation_fixture()
        self.activate()
        self.assertLess(self.events.index('service-stop'),self.events.index('swap'))
        self.assertLess(self.events.index('swap'),self.events.index('select'))
        self.assertLess(self.events.index('service-start'),self.events.index('post'))
        index = protection.read(kit.output(self.d))
        self.assertNotEqual(index['kitOperation'],str(self.f.op))
        self.assertEqual(protection.read(Path(index['kitOperation'])/'operation.json')['phase'],'complete')

    def test_preswap_failure_restores_previous_service_and_publishes_recovery_proof(self):
        self.activation_fixture(fail_swap=True)
        with self.assertRaisesRegex(RuntimeError,'synthetic-before-image-change'):
            self.activate()
        self.assertIn('service-start',self.events)
        self.assertIn('previous-ready',self.events)
        index = protection.read(kit.output(self.d))
        self.assertEqual(protection.read(Path(index['kitOperation'])/'operation.json')['phase'],
                         'recovered-previous')


if __name__ == '__main__':
    unittest.main()
