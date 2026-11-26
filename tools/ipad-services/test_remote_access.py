import copy, json, pathlib, tempfile, unittest, socket
from gateway import binding, ApiError
import remote_access

class FakeBackend:
    def __init__(self):
        self.supported = True; self.calls = []; self.replace_before_patch = False
        self.fail_before_once = set(); self.fail_after_once = set()
        self.records = {'notes': {'slug':'notes', 'ownerCutexSessionId':'agent1', 'createdAt':'registration1', 'remoteAccess':False}}
    def services(self): return copy.deepcopy(self.records)
    def api(self, method, path, body=None):
        self.calls.append((method, path, body))
        if path == '/v2/capabilities': return {'features':{'servicesRemoteAccessV1':self.supported}}
        record = self.records[path.rsplit('/', 1)[1]]
        slug = record['slug']
        if slug in self.fail_before_once:
            self.fail_before_once.remove(slug); raise ApiError(409)
        if self.replace_before_patch: record['createdAt'] = 'registration2'
        if body['ifRegistration'] != {k:record.get(k) for k in ('createdAt', 'ownerCutexSessionId')}:
            raise RuntimeError('409 service_registration_changed')
        record['remoteAccess'] = body['remoteAccess']
        if slug in self.fail_after_once:
            self.fail_after_once.remove(slug); raise ConnectionResetError('Reply lost after commit')
        return {'service':copy.deepcopy(record)}

class Migration(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='remote-access-fixture-')
        self.root = pathlib.Path(self.tmp.name); self.file = self.root/'access.json'
        self.cfg = {'access_file':str(self.file)}; self.backend = FakeBackend()
    def tearDown(self): self.tmp.cleanup()
    def write(self, value): self.file.write_text(json.dumps(value))
    def test_empty_is_noop_and_retired(self):
        self.write({}); result = remote_access.migrate(self.cfg, self.backend, self.root, True)
        self.assertEqual(result['migrated'], []); self.assertFalse(self.file.exists())
        self.assertFalse(any(m == 'PATCH' for m, _, _ in self.backend.calls))
    def test_matching_grants_only_and_one_time(self):
        self.write({'notes':binding(self.backend.records['notes']), 'deleted':{'slug':'deleted'}, 'taken':{'slug':'taken'}})
        preview = remote_access.migrate(self.cfg, self.backend, self.root)
        self.assertEqual(preview['matched'], ['notes']); self.assertFalse(self.backend.records['notes']['remoteAccess'])
        result = remote_access.migrate(self.cfg, self.backend, self.root, True)
        self.assertEqual(result['skipped'], ['deleted', 'taken']); self.assertFalse(self.file.exists())
        self.assertTrue(self.backend.records['notes']['remoteAccess'])
        self.backend.records['notes']['remoteAccess'] = False
        remote_access.migrate(self.cfg, self.backend, self.root, True)
        self.assertFalse(self.backend.records['notes']['remoteAccess'])
        self.assertEqual(sum(m == 'PATCH' for m, _, _ in self.backend.calls), 1)
        self.write({'new':{}})
        with self.assertRaisesRegex(RuntimeError, 'changed after migration'): remote_access.migrate(self.cfg, self.backend, self.root, True)
    def test_registration_or_owner_mismatch_cannot_inherit(self):
        for field in ('createdAt', 'ownerCutexSessionId'):
            grant = binding(self.backend.records['notes']); grant[field] = 'old'
            self.write({'notes':grant}); result = remote_access.migrate(self.cfg, self.backend, self.root)
            self.assertEqual(result['migrated'], []); self.assertEqual(result['skipped'], ['notes'])
    def test_concurrent_reregistration_is_fenced(self):
        self.write({'notes':binding(self.backend.records['notes'])}); self.backend.replace_before_patch = True
        with self.assertRaisesRegex(RuntimeError, '409'): remote_access.migrate(self.cfg, self.backend, self.root, True)
        self.assertTrue(self.file.exists()); self.assertFalse(self.backend.records['notes']['remoteAccess'])
        self.assertFalse(self.file.with_suffix('.lock').exists())
    def test_old_backend_cannot_retire_legacy(self):
        self.write({}); self.backend.supported = False
        with self.assertRaisesRegex(RuntimeError, 'lacks'): remote_access.migrate(self.cfg, self.backend, self.root, True)
        self.assertTrue(self.file.exists())
    def test_running_adapter_blocks_apply_before_any_grant(self):
        self.write({'notes':binding(self.backend.records['notes'])})
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0)); listener.listen()
            self.cfg['port'] = listener.getsockname()[1]
            with self.assertRaisesRegex(RuntimeError, 'Stop the private adapter'):
                remote_access.migrate(self.cfg, self.backend, self.root, True)
        self.assertTrue(self.file.exists())
        self.assertFalse(any(m == 'PATCH' for m, _, _ in self.backend.calls))
    def test_partial_receipt_skips_completed_grant_owner_disabled(self):
        self.backend.records['second'] = {**self.backend.records['notes'], 'slug':'second'}
        self.write({slug:binding(service) for slug, service in self.backend.records.items()})
        self.backend.fail_before_once.add('second')
        with self.assertRaises(ApiError): remote_access.migrate(self.cfg, self.backend, self.root, True)
        receipt = json.loads((self.root/'remote-access-migration.json').read_text())
        self.assertEqual(receipt['status'], 'partial')
        self.assertEqual(receipt['completed'], [binding(self.backend.records['notes'])])
        self.backend.records['notes']['remoteAccess'] = False
        result = remote_access.migrate(self.cfg, self.backend, self.root, True)
        self.assertEqual(result['status'], 'complete'); self.assertFalse(self.file.exists())
        self.assertFalse(self.backend.records['notes']['remoteAccess'])
        self.assertTrue(self.backend.records['second']['remoteAccess'])
        self.assertEqual(sum(m == 'PATCH' and p.endswith('/notes') for m,p,_ in self.backend.calls), 1)
    def test_lost_reply_is_not_replayed_after_owner_disables(self):
        self.write({'notes':binding(self.backend.records['notes'])})
        self.backend.fail_after_once.add('notes')
        with self.assertRaises(ConnectionResetError): remote_access.migrate(self.cfg, self.backend, self.root, True)
        self.backend.records['notes']['remoteAccess'] = False
        result = remote_access.migrate(self.cfg, self.backend, self.root, True)
        self.assertFalse(self.backend.records['notes']['remoteAccess'])
        self.assertEqual(result['skippedUnconfirmed'], [binding(self.backend.records['notes'])])
        self.assertEqual(sum(m == 'PATCH' for m,_,_ in self.backend.calls), 1)

if __name__ == '__main__': unittest.main()
