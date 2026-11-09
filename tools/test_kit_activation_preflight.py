import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock
import subprocess
import threading
import shutil
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(Path(__file__).resolve().parent))
import types
import kit_activation_preflight as preflight
import upgrade_online_kit as online
import stage_upgrade_online_kit as staging


class ActivationFixes(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='kit-fixes-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.op = self.root / 'operations' / 'retry'
        self.op.mkdir(parents=True)

    def write(self, path, value):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value), encoding='utf-8')

    def test_retry_inherits_confirmed_offline_provenance(self):
        old = self.root / 'operations' / 'failed'
        self.write(old / 'operation.json', {'phase': 'recovered'})
        self.write(old / 'manual-online.json', [
            {'id': 'a', 'selectedForOfflineByThisOperation': True, 'offlineOutcome': 'confirmed'},
            {'id': 'b', 'selectedForOfflineByThisOperation': True, 'offlineOutcome': 'requested'},
        ])
        inherited = [dict(r, selectedForOfflineByThisOperation=False) for r in json.loads((old / 'manual-online.json').read_text())]
        parents = ['failed']
        self.write(self.op / 'operation.json', {'phase': 'complete', 'retryOf': parents})
        self.write(self.op / 'manual-online.json', inherited)
        chosen, excluded = online.targets(self.op)
        self.assertEqual([r['id'] for r in chosen], ['a'])
        self.assertEqual([r['id'] for r in excluded], ['b'])

    def test_busy_retries_with_new_ids_and_backoff(self):
        self.write(self.op / 'manual-online.json', [{'id': 'a', 'selectedForOfflineByThisOperation': True,
                                                  'offlineOutcome': 'confirmed'}])
        requests = []
        connected = False
        def http(path, method='GET', body=None, timeout=60):
            nonlocal connected
            if method == 'POST':
                receipt = json.loads((self.op / online.RESULT).read_text())
                self.assertIn(body['requestId'], receipt['agents'][0]['requestIds'])
                requests.append(body)
                if len(requests) < 3:
                    raise online.HttpError(503, {'error': {'code': 'admission_busy', 'message': 'busy'}})
                connected = True
                return {'ok': True}
            return {'cutexSessionId': 'a', 'runtime': {'status': 'online' if connected else 'offline',
                    'appServerConnected': connected, 'runtimeGeneration': 7}}
        sleeps = []
        phase = online.Phase(self.op, {}, http, memory_probe=lambda: None, sleep=sleeps.append, out=lambda _: None)
        phase.begin(False)
        phase.online('a')
        self.assertEqual(phase.agent('a')['status'], 'online')
        self.assertEqual(len(requests), 3)
        self.assertEqual(len({r['requestId'] for r in requests}), 3)
        self.assertEqual(sleeps, [1, 2])

    def test_orphan_agent_serve_report_windows_and_linux(self):
        for image, command in [(r'D:\Apps\cutex\bin\cutex.exe', '"D:\\Apps\\cutex\\bin\\cutex.exe" agent serve --port 50596'),
                               ('/opt/cutex/releases/r277/cutex', '/opt/cutex/releases/r277/cutex agent serve --port 50596')]:
            with self.subTest(image=image):
                d = types.SimpleNamespace()
                d.orphan_agent_preflight = lambda: preflight.orphan_agent_preflight(d)
                d.plan = {'writerRoots': [image], 'selectedImage': image}
                d.process_rows = lambda: [
                    {'pid': 40, 'ppid': 2, 'born': 10, 'image': image, 'name': 'cutex', 'commandLine': 'cutex management serve'},
                    {'pid': 41, 'ppid': 40, 'born': 12, 'image': image, 'name': 'cutex', 'commandLine': 'cutex agent serve'},
                    {'pid': 149112, 'ppid': 999, 'born': 11, 'image': image, 'name': 'cutex', 'commandLine': command}]
                d.service_process_ids = lambda: {40}
                with self.assertRaisesRegex(RuntimeError, '149112') as error:
                    d.orphan_agent_preflight()
                self.assertIn(command, str(error.exception))
                self.assertIn('parent PID 999 is absent', str(error.exception))
                self.assertNotIn('PID 41:', str(error.exception))

    def test_inheritance_requires_retry_ancestor_and_confirmed_stop(self):
        for phase, kind, selected, outcome, linked in [
            ('complete', None, True, 'confirmed', True),
            ('recovered', 'rollback', True, 'confirmed', True),
            ('recovered', None, False, 'confirmed', True),
            ('recovered', None, True, 'requested', True),
            ('recovered', None, True, 'confirmed', False),
        ]:
            with self.subTest(phase=phase, kind=kind, selected=selected, outcome=outcome, linked=linked):
                old = self.root / 'operations' / 'old'
                self.write(old / 'operation.json', {'phase': phase, 'kind': kind})
                self.write(old / 'manual-online.json', [{'id': 'a', 'selectedForOfflineByThisOperation': selected, 'offlineOutcome': outcome}])
                self.write(self.op / 'operation.json', {'phase': 'complete', 'retryOf': ['old'] if linked else []})
                self.write(self.op / 'manual-online.json', [{'id': 'a', 'selectedForOfflineByThisOperation': False, 'offlineOutcome': 'confirmed'}])
                self.assertEqual(online.targets(self.op)[0], [])

    def test_agents_restored_by_ancestor_are_not_started_again(self):
        old = self.root / 'operations' / 'old'
        self.write(old / 'operation.json', {'phase': 'recovered-previous'})
        self.write(old / 'manual-online.json', [{'id': 'a', 'selectedForOfflineByThisOperation': True, 'offlineOutcome': 'confirmed'}])
        self.write(old / online.RESULT, {'agents': [{'id': 'a', 'status': 'online'}]})
        self.write(self.op / 'operation.json', {'phase': 'complete', 'retryOf': ['old', '../outside']})
        self.write(self.op / 'manual-online.json', [{'id': 'a', 'selectedForOfflineByThisOperation': False, 'offlineOutcome': 'confirmed'}])
        self.assertEqual(online.targets(self.op)[0], [])

    def test_busy_exhaustion_and_other_http_errors_do_not_loop(self):
        for status, code, expected in [(503, 'admission_busy', 4), (503, 'unavailable', 1), (409, 'admission_busy', 1)]:
            with self.subTest(status=status, code=code):
                self.write(self.op / 'manual-online.json', [{'id': 'a', 'selectedForOfflineByThisOperation': True, 'offlineOutcome': 'confirmed'}])
                requests = []
                def http(path, method='GET', body=None, timeout=60):
                    if method == 'POST':
                        requests.append(body)
                        raise online.HttpError(status, {'error': {'code': code, 'message': 'busy'}})
                    return {'runtime': {'status': 'offline', 'runtimeGeneration': len(requests)}}
                sleeps = []
                phase = online.Phase(self.op, {'autoOnline': {'admissionBusyRetries': 3, 'admissionBusyMaxBackoffSeconds': 2}}, http,
                                     memory_probe=lambda: None, sleep=sleeps.append, out=lambda _: None)
                (self.op / online.RESULT).unlink(missing_ok=True)
                phase.begin(False)
                phase.online('a')
                self.assertEqual(len(requests), expected)
                self.assertEqual(sleeps, [1, 2, 2] if expected == 4 else [])
                self.assertEqual(phase.agent('a')['status'], 'failed')
                self.assertEqual([r['params']['expectedRuntimeGeneration'] for r in requests], list(range(expected)))

    def test_pid_reuse_does_not_hide_orphan(self):
        d = types.SimpleNamespace()
        d.orphan_agent_preflight = lambda: preflight.orphan_agent_preflight(d)
        d.plan = {'writerRoots': ['/opt/cutex'], 'selectedImage': '/opt/cutex/cutex'}
        d.service_process_ids = lambda: {50}
        d.process_rows = lambda: [
            {'pid': 50, 'ppid': 1, 'born': 20, 'image': '/opt/cutex/cutex', 'commandLine': 'cutex management serve'},
            {'pid': 51, 'ppid': 50, 'born': 10, 'image': '/opt/cutex/cutex', 'commandLine': 'cutex agent serve'}]
        with self.assertRaisesRegex(RuntimeError, 'PID was reused'):
            d.orphan_agent_preflight()

    def test_owned_service_and_descendant_are_accepted(self):
        d = types.SimpleNamespace()
        d.orphan_agent_preflight = lambda: preflight.orphan_agent_preflight(d)
        d.plan = {'writerRoots': ['/opt/cutex'], 'selectedImage': '/opt/cutex/cutex'}
        d.service_process_ids = lambda: {50, 52}
        d.process_rows = lambda: [
            {'pid': 50, 'ppid': 1, 'born': 10, 'image': '/opt/cutex/cutex', 'commandLine': 'cutex management serve'},
            {'pid': 51, 'ppid': 50, 'born': 11, 'image': '/opt/cutex/cutex', 'commandLine': 'cutex agent serve'},
            {'pid': 52, 'ppid': 1, 'born': 9, 'image': '/opt/cutex/cutex', 'commandLine': 'cutex agent serve'}]
        self.assertEqual(d.orphan_agent_preflight(), {'orphanAgentServe': []})

    def test_linux_pid_one_parent_is_orphan_but_declared_bus_is_owned(self):
        d = types.SimpleNamespace(plan={'writerRoots': ['/opt/cutex']})
        rows = [
            {'pid': 1, 'ppid': 0, 'born': 1, 'image': '/sbin/init', 'commandLine': '/sbin/init'},
            {'pid': 50, 'ppid': 1, 'born': 10, 'image': '/opt/cutex/cutex', 'commandLine': 'cutex agent serve --port 40000'}]
        d.process_rows = lambda: rows
        d.service_process_ids = lambda: {50}
        self.assertEqual(preflight.orphan_agent_preflight(d), {'orphanAgentServe': []})
        d.service_process_ids = lambda: {60}
        with mock.patch.object(preflight, 'os', types.SimpleNamespace(name='posix', path=os.path, sep=os.sep)):
            with self.assertRaisesRegex(RuntimeError, 'parent PID 1 is not owned') as error:
                preflight.orphan_agent_preflight(d)
        self.assertIn('kill -TERM 50', str(error.exception))

    def test_service_ids_come_from_declared_runtime_host_systemd_or_private_receipts(self):
        runtime = types.SimpleNamespace(plan={'adapter': 'runtime-host', 'controller': 'controller', 'services': ['bus', 'mgmt']},
            command=lambda args, timeout: json.dumps({'services': [{'id': 'bus', 'pid': 10, 'state': 'running'},
                {'id': 'mgmt', 'pid': 11, 'state': 'running'}, {'id': 'other', 'pid': 12, 'state': 'running'},
                {'id': 'bus', 'pid': 13, 'state': 'stopped'}]}))
        self.assertEqual(preflight.service_process_ids(runtime), {10, 11})
        linux = types.SimpleNamespace(plan={'adapter': 'systemd', 'services': ['bus', 'mgmt']},
                                      command=lambda args, timeout: '10' if 'bus' in args else '11')
        self.assertEqual(preflight.service_process_ids(linux), {10, 11})
        state = self.root / 'private-service-state'
        self.write(state / 'services.json', {'bus': str(state / 'bus.json'), 'management': str(state / 'mgmt.json')})
        self.write(state / 'bus.json', {'pid': 20})
        self.write(state / 'mgmt.json', {'pid': 21})
        private = types.SimpleNamespace(plan={'adapter': 'private'}, root=self.root)
        self.assertEqual(preflight.service_process_ids(private), {20, 21})

    def test_windows_process_capture_has_actual_own_parent_birth_and_command(self):
        if os.name != 'nt':
            self.skipTest('Windows CIM capture')
        d = types.SimpleNamespace(command=lambda args, timeout: subprocess.run(args, capture_output=True, text=True, check=True, timeout=timeout).stdout)
        own = next(r for r in preflight.process_rows(d) if r['pid'] == os.getpid())
        self.assertGreater(own['ppid'], 0)
        self.assertGreater(own['born'], 0)
        self.assertIn('python', own['commandLine'].lower())

    def test_linux_process_capture_of_actual_own_occurrence(self):
        if os.name != 'posix' or not Path('/proc/self/stat').is_file():
            self.skipTest('Linux /proc capture')
        stat = Path('/proc/self/stat').read_text()
        born = int(stat[stat.rfind(')') + 2:].split()[19])
        d = types.SimpleNamespace(_legacy_process_rows=lambda: [{'pid': os.getpid(), 'born': born}])
        own = preflight.process_rows(d)[0]
        self.assertTrue(own['argv'])
        self.assertIn('python', own['commandLine'].lower())

    def test_linux_process_capture_rejects_reused_occurrence_and_preserves_argv(self):
        fake_proc = self.root / 'proc'
        for pid, born in [(123, 10), (124, 20)]:
            entry = fake_proc / str(pid)
            entry.mkdir(parents=True)
            (entry / 'cmdline').write_bytes(b'\0'.join([b'/opt/cutex/cutex', b'agent', b'serve', b'--port', b'40000', b'']))
            (entry / 'stat').write_text(str(pid) + ' (cutex (old)) ' + ' '.join(map(str, ['S', 1] + [0] * 17 + [born])))
        d = types.SimpleNamespace(_legacy_process_rows=lambda: [{'pid': 123, 'born': 10}, {'pid': 124, 'born': 10}])
        with mock.patch.object(preflight, 'os', types.SimpleNamespace(name='posix')), mock.patch.object(preflight, 'pathlib', types.SimpleNamespace(Path=lambda value: fake_proc / Path(value).name)):
            rows = preflight.process_rows(d)
        self.assertEqual(rows[0]['argv'][1:3], ['agent', 'serve'])
        self.assertIn('--port', rows[0]['commandLine'])
        self.assertNotIn('commandLine', rows[1])

    def test_loopback_http_admission_busy_retries_then_confirms(self):
        self.write(self.op / 'manual-online.json', [{'id': 'a', 'selectedForOfflineByThisOperation': True, 'offlineOutcome': 'confirmed'}])
        posts = []
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass
            def reply(self, status, body):
                raw = json.dumps(body).encode()
                self.send_response(status)
                self.send_header('Content-Length', str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)
            def do_GET(self):
                ready = len(posts) >= 3
                self.reply(200, {'runtime': {'status': 'online' if ready else 'offline', 'appServerConnected': ready, 'runtimeGeneration': 4}})
            def do_POST(self):
                posts.append(json.loads(self.rfile.read(int(self.headers['Content-Length']))))
                if len(posts) < 3:
                    self.reply(503, {'error': {'code': 'admission_busy', 'message': 'busy'}})
                else:
                    self.reply(200, {'ok': True})
        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            d = types.SimpleNamespace(plan={'apiOrigin': 'http://127.0.0.1:%d' % server.server_port}, token='test-only')
            phase = online.Phase(self.op, {}, online.http_for(d), memory_probe=lambda: None, sleep=lambda _: None, out=lambda _: None)
            phase.begin(False)
            phase.online('a')
            self.assertEqual(phase.agent('a')['status'], 'online')
            self.assertEqual(len(posts), 3)
            self.assertEqual(len({r['requestId'] for r in posts}), 3)
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


class StagedKitTests(unittest.TestCase):
    def test_real_kit_staging_and_retry_inheritance_lf_and_crlf(self):
        source = os.environ.get('CUTEX_R280_KIT_TOOL_SOURCE')
        if not source:
            self.skipTest('requires CUTEX_R280_KIT_TOOL_SOURCE (read-only kit source)')
        for newline in ('\n', '\r\n'):
            with self.subTest(newline=repr(newline)), tempfile.TemporaryDirectory(prefix='kit-staging-') as temporary:
                root = Path(temporary) / 'kit'
                shutil.copytree(source, root, ignore=shutil.ignore_patterns('operations', '__pycache__'))
                deploy_file = root / 'deploy.py'
                deploy_file.write_bytes(deploy_file.read_text(encoding='utf-8').replace('\n', newline).encode('utf-8'))
                staging.stage(root)
                staged = deploy_file.read_bytes()
                self.assertEqual(staging.stage(root), [])
                self.assertEqual(deploy_file.read_bytes(), staged)
                self.assertIn(b'self.orphan_agent_preflight();self.integrity()', staged)
                self.assertEqual(b'\r\n' in staged, newline == '\r\n')
                spec = importlib.util.spec_from_file_location('staged_deploy_test', deploy_file)
                module = importlib.util.module_from_spec(spec)
                previous_path = list(sys.path)
                try:
                    spec.loader.exec_module(module)
                finally:
                    sys.path[:] = previous_path
                old = root / 'operations' / 'failed'
                old.mkdir(parents=True)
                online.write(old / 'operation.json', {'phase': 'recovered-previous'})
                online.write(old / 'manual-online.json', [{'id': 'a', 'selectedForOfflineByThisOperation': True, 'offlineOutcome': 'confirmed'}])
                d = module.Deploy.__new__(module.Deploy)
                d.root = root
                inherited, parents = d.inherited_manual()
                retry = root / 'operations' / 'retry'
                retry.mkdir()
                online.write(retry / 'operation.json', {'phase': 'complete', 'retryOf': parents})
                online.write(retry / 'manual-online.json', inherited)
                online.write(old / 'operation.json', {'phase': 'recovered-previous', 'supersededBy': 'retry'})
                self.assertEqual([r['id'] for r in online.targets(retry)[0]], ['a'])


if __name__ == '__main__':
    unittest.main()
