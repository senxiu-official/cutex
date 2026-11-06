"""r274.1 deploy.py/native_managed.py-shaped fixtures; all I/O is temporary."""
import contextlib
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import runtime_prune_protection as protection


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2) + '\n', encoding='utf-8')


class ProtectionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.op = self.root / 'kit' / 'operations' / 'current'
        self.base = self.root / 'install'
        self.home = self.root / 'private-home'
        self.output = self.root / 'runtime' / 'prune-protection.json'
        self.active_executable = self.root / 'selected' / 'cutex.exe'
        self.active_executable.parent.mkdir(parents=True)
        self.active_executable.write_bytes(b'new selected cutex binary fixture')
        self.secret = str(self.home / 'auth' / 'credential.json')
        self.backups = {}
        self.originals = {}
        self.expected = set()
        self.protected = {}
        for name, fields in protection.FIELDS.items():
            deployment = {field: str(self.root / 'before' / name / field) for field in fields}
            self.expected.update(deployment.values())
            if name == 'claude':
                deployment['claude_home'] = str(self.home / 'claude')
            else:
                deployment['version'] = '0.2.1-rc.3'
            if name == 'kimi':
                deployment['rg'] = str(self.root / 'bin' / 'rg')
                self.expected.add(deployment['rg'])
            relative = 'runtime/' + name + '/deployment.json'
            self.backup(relative, deployment)
            self.protected[name + '-runtime'] = {'exists': True, 'sha256': self.backups[relative]}
        state = {name + '-runtime': {'selected': name + '-r2', 'origin': 'source',
                                    'installedAt': '2026-11-03T00:00:00Z'}
                 for name in protection.FIELDS}
        state['job-service'] = {'selected': 'job-r1', 'origin': 'source', 'installedAt': ''}
        state['unknown-future-component'] = {'selected': self.secret, 'credential': self.secret}
        for name in protection.FIELDS:
            self.expected.add(str(self.base / 'components' / (name + '-runtime') / (name + '-r2')))
        self.state_path = self.base / 'components' / 'state.json'
        self.original(self.state_path, state)
        state_sha = self.originals[str(self.state_path)]['sha256']
        self.protected['state'] = {'exists': True, 'sha256': state_sha}
        self.original(self.base / 'install.json', {'schema': 'cutex/install/v1',
                      'activeRelease': 'r273.1', 'previousRelease': 'r260', 'token': self.secret})
        self.binary = self.base / 'bin' / ('cutex.exe' if os.name == 'nt' else 'cutex')
        self.original(self.binary, {'fake': 'binary bytes'})
        self.expected.add(str(self.binary))
        # Unknown original keys must never become protectable runtime paths.
        self.original(Path(self.secret), {'secret': 'DO-NOT-EXPORT'})
        self.manifest_path = self.root / 'native' / 'old-bundle.json'
        self.manifest = self.make_bundle('old')
        write(self.manifest_path, self.manifest)
        self.light_path = self.home / 'runtime' / 'light' / 'deployment.json'
        light = {'native_home': str(self.home / 'native'), 'bundle_manifest': str(self.manifest_path),
                 'job_mcp': {'endpoint': self.secret, 'api_token_file': self.secret,
                             'grant_key_file': self.secret}}
        self.backup('runtime/light/deployment.json', light)
        pinned_path = self.root / 'native' / 'pinned-bundle.json'
        write(pinned_path, self.make_bundle('pinned'))
        self.before = {'schema': 'cutex/native-managed-before/v1',
                       'existingBundleFiles': {str(pinned_path): {'exists': True,
                                               'sha256': protection.digest(pinned_path)}},
                       'deploymentPath': str(self.light_path), 'deployment': light,
                       'deploymentSha256': self.backups['runtime/light/deployment.json'],
                       'manifest': self.manifest, 'manifestPath': str(self.manifest_path),
                       'manifestSha256': protection.digest(self.manifest_path),
                       'artifacts': {role: self.manifest[role] for role in protection.ROLES},
                       'registry': {'runtime_paths': {self.secret: {'version': 'old'}}},
                       'registrySha256': 'a' * 64}
        write(self.op / 'native-managed-before.json', self.before)
        self.prepared = {'phase': 'prepared', 'protected': self.protected, 'candidates': [],
                         'channel': 'release', 'sequence': 3, 'indexSha256': 'b' * 64}
        write(self.op / 'component-prepared.json', self.prepared)
        self.record = {'phase': 'complete', 'createdAtNs': 123,
                       'nativeManagedBeforeSha256': protection.digest(self.op / 'native-managed-before.json'),
                       'componentPreparedSha256': protection.digest(self.op / 'component-prepared.json'),
                       'componentPreparationReport': self.prepared,
                       'pins': {'sessions': 'c' * 64, 'count': 0, 'native': {
                           str(self.light_path): self.before['deploymentSha256'],
                           str(self.manifest_path): self.before['manifestSha256'],
                           self.secret: 'd' * 64}}}
        self.save()
        journal = [{'at': '2026-11-03', 'step': 'backup.verify', 'phase': 'begin'},
                   {'at': '2026-11-03', 'step': 'backup.verify', 'phase': 'end', 'exitCode': 0}]
        (self.op / 'steps.jsonl').write_text('\n'.join(json.dumps(row) for row in journal) + '\n')

    def backup(self, relative, value):
        path = self.op / 'backup' / relative
        write(path, value)
        self.backups[relative] = protection.digest(path)

    def original(self, original, value):
        file = str(len(self.originals))
        path = self.op / 'originals' / file
        write(path, value)
        self.originals[str(original)] = {'file': file, 'sha256': protection.digest(path)}

    def make_bundle(self, name):
        bundle = {role: {'path': str(self.root / 'artifacts' / name / role), 'sha256': 'e' * 64}
                  for role in protection.ROLES}
        self.expected.update(ref['path'] for ref in bundle.values())
        bundle.update(shared_config={'path': self.secret, 'sha256': 'f' * 64},
                      version=3, upstream_commit='3d2ee51ca2d5db578f328aa75e20aa22c0197c9a',
                      native_patch_commit='25cc8e7ab1c43005bdcfd6832d286c04a3bb337c',
                      launch_config_sha256=None)
        return bundle

    def save(self):
        write(self.op / 'backup-manifest.json', self.backups)
        write(self.op / 'originals-manifest.json', self.originals)
        write(self.op / 'operation.json', self.record)

    def test_exact_contract_and_no_secret_home_or_history_paths(self):
        # A corrupt historical operation is neither opened nor protected.
        write(self.op.parent / 'historical' / 'operation.json', {'path': self.secret})
        index = protection.build_index(self.op, self.active_executable)
        self.assertEqual(index['schema'], protection.SCHEMA)
        self.assertEqual(index['kitOperation'], str(self.op))
        self.assertEqual(index['activeExecutableSha256'], protection.digest(self.active_executable))
        self.assertEqual(index['provenance']['operationSha256'], protection.digest(self.op / 'operation.json'))
        self.assertEqual(index['protectedPaths'], sorted(self.expected))
        text = json.dumps(index)
        for forbidden in [self.secret, str(self.home), 'DO-NOT-EXPORT', 'historical', 'shared_config']:
            self.assertNotIn(forbidden, text)
        self.assertEqual(index, protection.build_index(self.op, self.active_executable, self.base))
        self.assertNotIn('activeRelease', index)  # before install.json is not current authority

    def test_all_five_original_native_artifacts_are_protected(self):
        paths = protection.build_index(self.op, self.active_executable)['protectedPaths']
        for role in protection.ROLES:
            self.assertIn(self.manifest[role]['path'], paths)

    def test_snapshot_tamper_and_missing_files_refuse(self):
        for relative in self.backups:
            path = self.op / 'backup' / relative
            data = path.read_bytes()
            with self.subTest(relative=relative):
                path.write_bytes(data + b' ')
                with self.assertRaisesRegex(protection.ProtectionError, 'SHA256 mismatch'):
                    protection.build_index(self.op, self.active_executable)
                path.unlink()
                with self.assertRaisesRegex(protection.ProtectionError, 'cannot hash file'):
                    protection.build_index(self.op, self.active_executable)
                path.write_bytes(data)
        for row in self.originals.values():
            path = self.op / 'originals' / row['file']
            data = path.read_bytes()
            path.write_bytes(data + b' ')
            with self.assertRaisesRegex(protection.ProtectionError, 'SHA256 mismatch'):
                protection.build_index(self.op, self.active_executable)
            path.write_bytes(data)

    def test_omitted_deployment_refuses_even_when_manifest_rewritten(self):
        for relative in list(self.backups):
            expected = self.backups.pop(relative)
            self.save()
            with self.assertRaisesRegex(protection.ProtectionError, 'missing backup reference'):
                protection.build_index(self.op, self.active_executable)
            self.backups[relative] = expected
        self.save()

    def test_operation_bindings_refuse_changes(self):
        for key in ['nativeManagedBeforeSha256', 'componentPreparedSha256']:
            expected = self.record[key]
            self.record[key] = '0' * 64
            self.save()
            with self.assertRaisesRegex(protection.ProtectionError, 'operation binding.*mismatch'):
                protection.build_index(self.op, self.active_executable)
            self.record[key] = expected
        self.record['pins']['native'][str(self.light_path)] = '0' * 64
        self.save()
        with self.assertRaisesRegex(protection.ProtectionError, 'operation pins mismatch'):
            protection.build_index(self.op, self.active_executable)

    def test_snapshot_must_be_finished_successfully(self):
        path = self.op / 'steps.jsonl'
        for rows in [[], [{'step': 'backup.verify', 'phase': 'begin'}],
                     [{'step': 'backup.verify', 'phase': 'begin'},
                      {'step': 'backup.verify', 'phase': 'end', 'exitCode': 1}]]:
            path.write_text('\n'.join(json.dumps(row) for row in rows))
            with self.assertRaisesRegex(protection.ProtectionError, 'complete successful snapshot'):
                protection.build_index(self.op, self.active_executable)

    def test_only_final_complete_or_recovered_previous_operations_supported(self):
        for phase in ['preparing', 'before-stop', 'stopping', 'swapping', 'selected']:
            with self.subTest(phase=phase):
                self.record['phase'] = phase
                self.save()
                with self.assertRaisesRegex(protection.ProtectionError, 'stable final'):
                    protection.build_index(self.op, self.active_executable)
        for phase in ['complete', 'recovered-previous']:
            self.record['phase'] = phase
            self.save()
            self.assertEqual(protection.build_index(self.op, self.active_executable)['protectedPaths'],
                             sorted(self.expected))

    def test_missing_component_state_reference_refuses(self):
        del self.originals[str(self.state_path)]
        self.save()
        with self.assertRaisesRegex(protection.ProtectionError, 'backed-up components/state.json required'):
            protection.build_index(self.op, self.active_executable)

    def test_missing_original_file_refuses(self):
        row = self.originals[str(self.binary)]
        (self.op / 'originals' / row['file']).unlink()
        with self.assertRaisesRegex(protection.ProtectionError, 'cannot hash file'):
            protection.build_index(self.op, self.active_executable)

    def test_pinned_manifest_missing_refuses(self):
        pinned = next(iter(self.before['existingBundleFiles']))
        Path(pinned).unlink()
        with self.assertRaisesRegex(protection.ProtectionError, 'cannot hash file'):
            protection.build_index(self.op, self.active_executable)

    def test_native_artifact_binding_refuses_changed_public_path(self):
        self.before['artifacts']['executable'] = {'path': str(self.root / 'changed'), 'sha256': 'e' * 64}
        write(self.op / 'native-managed-before.json', self.before)
        self.record['nativeManagedBeforeSha256'] = protection.digest(self.op / 'native-managed-before.json')
        self.save()
        with self.assertRaisesRegex(protection.ProtectionError, 'native before artifact mismatch'):
            protection.build_index(self.op, self.active_executable)

    def test_relative_runtime_path_refused_with_all_hashes_rebound(self):
        relative = 'runtime/dsh/deployment.json'
        deployment = protection.read(self.op / 'backup' / relative)
        deployment['adapter'] = 'relative/adapter.mjs'
        self.backup(relative, deployment)
        self.prepared['protected']['dsh-runtime']['sha256'] = self.backups[relative]
        write(self.op / 'component-prepared.json', self.prepared)
        self.record['componentPreparedSha256'] = protection.digest(self.op / 'component-prepared.json')
        self.save()
        with self.assertRaisesRegex(protection.ProtectionError, 'local absolute path required'):
            protection.build_index(self.op, self.active_executable)

    def test_unknown_strings_are_not_path_discovery(self):
        self.before['unrecognized'] = {'adapter': self.secret, 'runtime_paths': [self.secret]}
        write(self.op / 'native-managed-before.json', self.before)
        self.record['nativeManagedBeforeSha256'] = protection.digest(self.op / 'native-managed-before.json')
        self.save()
        self.assertEqual(protection.build_index(self.op, self.active_executable)['protectedPaths'], sorted(self.expected))

    def test_output_cannot_overwrite_operation_evidence(self):
        previous = (self.op / 'operation.json').read_bytes()
        with self.assertRaisesRegex(protection.ProtectionError, 'outside immutable operation evidence'):
            protection.export_index(self.op, self.op / 'operation.json', self.active_executable)
        self.assertEqual((self.op / 'operation.json').read_bytes(), previous)

    def test_state_binding_and_version_traversal_refuse(self):
        file = self.op / 'originals' / self.originals[str(self.state_path)]['file']
        state = protection.read(file)
        state['claude-runtime']['selected'] = '../escape'
        write(file, state)
        self.originals[str(self.state_path)]['sha256'] = protection.digest(file)
        self.save()
        with self.assertRaisesRegex(protection.ProtectionError, 'state binding.*mismatch'):
            protection.build_index(self.op, self.active_executable)
        self.prepared['protected']['state']['sha256'] = protection.digest(file)
        write(self.op / 'component-prepared.json', self.prepared)
        self.record['componentPreparedSha256'] = protection.digest(self.op / 'component-prepared.json')
        self.save()
        with self.assertRaisesRegex(protection.ProtectionError, 'invalid selected component version'):
            protection.build_index(self.op, self.active_executable)

    def test_native_manifest_unreadable_or_tampered_refuses(self):
        self.manifest_path.write_text('{}')
        with self.assertRaisesRegex(protection.ProtectionError, 'native manifest: SHA256 mismatch'):
            protection.build_index(self.op, self.active_executable)
        self.manifest_path.unlink()
        with self.assertRaisesRegex(protection.ProtectionError, 'cannot hash file'):
            protection.build_index(self.op, self.active_executable)

    def test_native_manifest_can_use_verified_original_snapshot(self):
        self.original(self.manifest_path, self.manifest)
        self.save()
        self.manifest_path.unlink()
        self.assertEqual(protection.build_index(self.op, self.active_executable)['protectedPaths'], sorted(self.expected))

    def test_snapshot_traversal_and_duplicate_keys_refuse(self):
        self.backups['../outside'] = '0' * 64
        self.save()
        with self.assertRaisesRegex(protection.ProtectionError, 'snapshot path escapes'):
            protection.build_index(self.op, self.active_executable)
        (self.op / 'backup-manifest.json').write_text('{"x":1,"x":2}')
        with self.assertRaisesRegex(protection.ProtectionError, 'duplicate JSON key'):
            protection.build_index(self.op, self.active_executable)

    def test_explicit_install_base_must_match(self):
        with self.assertRaisesRegex(protection.ProtectionError, '--install-base differs'):
            protection.build_index(self.op, self.active_executable, self.root / 'wrong-base')

    def test_atomic_export_and_failure_retains_previous_output(self):
        index = protection.export_index(self.op, self.output, self.active_executable)
        self.assertEqual(protection.read(self.output), index)
        previous = self.output.read_bytes()
        with patch.object(protection.os, 'replace', side_effect=OSError('replace refused')):
            with self.assertRaises(OSError):
                protection.export_index(self.op, self.output, self.active_executable)
        self.assertEqual(self.output.read_bytes(), previous)
        self.assertEqual(list(self.output.parent.iterdir()), [self.output])
        (self.op / 'native-managed-before.json').unlink()
        with self.assertRaises(protection.ProtectionError):
            protection.export_index(self.op, self.output, self.active_executable)
        self.assertEqual(self.output.read_bytes(), previous)

    def test_old_index_does_not_match_new_selected_executable_bytes(self):
        old = protection.export_index(self.op, self.output, self.active_executable)
        self.active_executable.write_bytes(b'next kit selected binary fixture')
        with self.assertRaisesRegex(protection.ProtectionError, 'current_exe: SHA256 mismatch'):
            protection.verified(self.active_executable, old['activeExecutableSha256'], 'current_exe')
        new = protection.build_index(self.op, self.active_executable)
        self.assertNotEqual(new['activeExecutableSha256'], old['activeExecutableSha256'])

    def test_tampered_executable_invalidates_published_index(self):
        index = protection.export_index(self.op, self.output, self.active_executable)
        with self.active_executable.open('ab') as stream:
            stream.write(b'tampering')
        with self.assertRaisesRegex(protection.ProtectionError, 'current_exe: SHA256 mismatch'):
            protection.verified(self.active_executable, index['activeExecutableSha256'], 'current_exe')

    def test_operation_change_invalidates_published_index_even_for_same_executable(self):
        index = protection.export_index(self.op, self.output, self.active_executable)
        self.record['at'] = 'later final record'
        self.save()
        protection.verified(self.active_executable, index['activeExecutableSha256'], 'current_exe')
        with self.assertRaisesRegex(protection.ProtectionError, 'operation: SHA256 mismatch'):
            protection.verified(self.op / 'operation.json', index['provenance']['operationSha256'], 'operation')

    def test_operation_or_executable_changing_during_build_refuses(self):
        snapshot_complete = protection.snapshot_complete

        def mutate_operation(op):
            snapshot_complete(op)
            self.record['at'] = 'changed while building'
            self.save()

        with patch.object(protection, 'snapshot_complete', side_effect=mutate_operation):
            with self.assertRaisesRegex(protection.ProtectionError, 'operation changed during export'):
                protection.build_index(self.op, self.active_executable)

        def mutate_executable(op):
            snapshot_complete(op)
            self.active_executable.write_bytes(b'changed while building')

        with patch.object(protection, 'snapshot_complete', side_effect=mutate_executable):
            with self.assertRaisesRegex(protection.ProtectionError, 'active executable changed during export'):
                protection.build_index(self.op, self.active_executable)

    def test_final_binding_rechecked_before_atomic_publication(self):
        protection.export_index(self.op, self.output, self.active_executable)
        previous = self.output.read_bytes()
        json_dump = json.dump
        for target, expected in [(self.active_executable, 'active executable'),
                                 (self.op / 'operation.json', 'operation')]:
            with self.subTest(target=target):
                original = target.read_bytes()

                def mutate_after_dump(*args, **kwargs):
                    json_dump(*args, **kwargs)
                    target.write_bytes(original + b' ')

                with patch.object(protection.json, 'dump', side_effect=mutate_after_dump):
                    with self.assertRaisesRegex(protection.ProtectionError, expected + ' changed before publication'):
                        protection.export_index(self.op, self.output, self.active_executable)
                self.assertEqual(self.output.read_bytes(), previous)
                self.assertEqual(list(self.output.parent.iterdir()), [self.output])
                target.write_bytes(original)

    def test_active_executable_required_readable_and_output_cannot_overwrite_it(self):
        with self.assertRaises(TypeError):
            protection.build_index(self.op)
        with self.assertRaises(TypeError):
            protection.export_index(self.op, self.output)
        with self.assertRaisesRegex(protection.ProtectionError, 'local absolute path required'):
            protection.build_index(self.op, Path('relative-cutex'))
        with self.assertRaisesRegex(protection.ProtectionError, 'cannot hash file'):
            protection.build_index(self.op, self.root / 'missing-cutex')
        previous = self.active_executable.read_bytes()
        with self.assertRaisesRegex(protection.ProtectionError, 'must not overwrite active executable'):
            protection.export_index(self.op, self.active_executable, self.active_executable)
        self.assertEqual(self.active_executable.read_bytes(), previous)

    def test_kit_invalidates_old_proof_even_when_active_binary_is_unchanged(self):
        protection.export_index(self.op, self.output, self.active_executable)
        binary = self.active_executable.read_bytes()
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(protection.main(['--invalidate', '--output', str(self.output)]), 0)
        self.assertNotEqual(protection.read(self.output)['schema'], protection.SCHEMA)
        self.assertEqual(self.active_executable.read_bytes(), binary)
        self.assertEqual(list(self.output.parent.iterdir()), [self.output])
        (self.op / 'backup' / 'runtime' / 'kimi' / 'deployment.json').unlink()
        with self.assertRaises(protection.ProtectionError):
            protection.export_index(self.op, self.output, self.active_executable)
        self.assertNotEqual(protection.read(self.output)['schema'], protection.SCHEMA)

    def test_invalidation_rejects_other_paths_and_mixed_export_arguments(self):
        for path in [Path('runtime/prune-protection.json'), self.active_executable,
                     self.root / 'prune-protection.json']:
            with self.subTest(path=path), self.assertRaises(protection.ProtectionError):
                protection.invalidate_index(path)
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as raised:
            protection.main(['--invalidate', '--output', str(self.output),
                             '--operation-dir', str(self.op)])
        self.assertEqual(raised.exception.code, 2)

    def test_cli_requires_active_executable(self):
        error = io.StringIO()
        with contextlib.redirect_stderr(error):
            with self.assertRaises(SystemExit) as raised:
                protection.main(['--operation-dir', str(self.op), '--output', str(self.output)])
        self.assertEqual(raised.exception.code, 2)
        self.assertIn('--active-executable', error.getvalue())
        self.assertFalse(self.output.exists())

    def test_cli_error_precise_without_creating_output(self):
        (self.op / 'backup' / 'runtime' / 'kimi' / 'deployment.json').unlink()
        error = io.StringIO()
        with contextlib.redirect_stderr(error):
            code = protection.main(['--operation-dir', str(self.op), '--output', str(self.output),
                                    '--active-executable', str(self.active_executable)])
        self.assertEqual(code, 1)
        self.assertIn('cannot hash file', error.getvalue())
        self.assertIn('deployment.json', error.getvalue())
        self.assertFalse(self.output.exists())


if __name__ == '__main__':
    unittest.main()
