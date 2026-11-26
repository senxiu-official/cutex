"""Real loopback HTTP fixtures; never talks to live cutex or reads its home."""
import contextlib, http.client, http.server, json, pathlib, tempfile, threading, unittest
import gateway
from unittest.mock import patch

TEST_HOST = "services.example.test"
TEST_ORIGIN = "https://" + TEST_HOST

class FakeManagement(http.server.BaseHTTPRequestHandler):
    calls = []
    remote_access = True
    def log_message(self, *_): pass
    def do_GET(self): self.run_request()
    def do_POST(self): self.run_request()
    def run_request(self):
        body = self.rfile.read(int(self.headers.get('Content-Length', 0)))
        self.calls.append((self.command, self.path, dict(self.headers), body))
        host = self.headers['Host']
        if host.startswith('notes.svc.localhost:'):
            if self.path == '/__cutex/auth?ticket=test':
                self.send_response(302); self.send_header('Set-Cookie', 'cutex_svc_fake=private; HttpOnly'); self.end_headers(); return
            assert 'cutex_svc_fake=private' in self.headers['Cookie']
            assert not self.headers.get('Authorization')
            if self.path in ('/login-failed', '/ticket-expired'):
                self.send_response(401)
                if self.path == '/ticket-expired': self.send_header('X-Cutex-Service-Auth', 'ticket-required')
                self.send_header('Content-Length', '0'); self.end_headers(); return
            self.send_response(200); self.send_header('Content-Length', str(len(body or b'lecture notes')))
            self.send_header('Set-Cookie', 'app=x; Domain=svc.localhost; Path=/')
            self.end_headers(); self.wfile.write(body or b'lecture notes'); return
        assert self.headers['Authorization'] == 'Bearer fake-root'
        if self.path in ('/v2/services', '/v2/services?visibility=all'):
            result = {'services': [{'slug': 'notes', 'displayName': '<Notes>', 'runtime': 'static', 'path': '/', 'health': 'reachable', 'remoteAccess': self.remote_access}]}
        elif self.path == '/v2/services/notes/view-ticket' and self.command == 'POST':
            result = {'authPath': '/__cutex/auth?ticket=test'}
        else:
            self.send_error(500); return
        raw = json.dumps(result).encode(); self.send_response(200); self.send_header('Content-Length', str(len(raw))); self.end_headers(); self.wfile.write(raw)

class Gate(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory(prefix='ipad-gate-fixture-')
        cfgpath = pathlib.Path(cls.tmp.name) / 'fixture.json'; cfgpath.write_text('{"management_api_token":"fake-root"}')
        cls.mgmt = http.server.ThreadingHTTPServer(('127.0.0.1', 0), FakeManagement)
        threading.Thread(target=cls.mgmt.serve_forever, daemon=True).start()
        cls.bridge = http.server.ThreadingHTTPServer(('127.0.0.1', 0), gateway.Handler)
        cls.bridge.cfg = {'host': TEST_HOST, 'host_label': 'Fixture <Node>', 'cutex_config': str(cfgpath), 'management_port': cls.mgmt.server_port, 'bridge_secret': 'fixture-secret', 'access_file': str(pathlib.Path(cls.tmp.name) / 'access.json')}
        cls.bridge.backend = gateway.Backend(cls.bridge.cfg)
        FakeManagement.remote_access = True
        threading.Thread(target=cls.bridge.serve_forever, daemon=True).start()
    @classmethod
    def tearDownClass(cls):
        for s in (cls.bridge, cls.mgmt): s.shutdown(); s.server_close()
        cls.tmp.cleanup()
    def setUp(self):
        FakeManagement.remote_access = True

    def request(self, path='/', method='GET', headers=None, body=None):
        c = http.client.HTTPConnection('127.0.0.1', self.bridge.server_port)
        h = {'Host': TEST_HOST, 'X-Ipad-Gate': 'fixture-secret'}; h.update(headers or {})
        c.request(method, path, body=body, headers=h); r = c.getresponse()
        result = r.status, dict(r.getheaders()), r.read(); c.close(); return result
    def test_index_and_static_proxy(self):
        status, _, body = self.request(); self.assertEqual(status, 200); self.assertIn(b'&lt;Notes&gt;', body)
        self.assertIn(b'Fixture &lt;Node&gt;', body)
        status, headers, body = self.request('/s/notes/', headers={'Authorization': 'Bearer browser-secret', 'Cookie': 'cutex_svc_fake=evil; app=ok'})
        self.assertEqual((status, body), (200, b'lecture notes'))
        self.assertNotIn(b'private', body); self.assertIn("worker-src 'none'", headers['Content-Security-Policy'])
        self.assertIn('Path=/s/notes/', headers['Set-Cookie']); self.assertNotIn('Domain=', headers['Set-Cookie'])
        upstream_headers = FakeManagement.calls[-1][2]
        self.assertNotIn('Authorization', upstream_headers); self.assertNotIn('evil', upstream_headers['Cookie'])
    def test_write_origin_and_body(self):
        self.assertEqual(self.request('/s/notes/save', 'POST', body=b'data')[0], 403)
        self.assertEqual(self.request('/s/notes/save', 'POST', {'Origin': TEST_ORIGIN}, b'data')[2], b'data')
        self.assertEqual(FakeManagement.calls[-1][2]['Origin'], 'http://notes.svc.localhost:' + str(self.mgmt.server_port))
    def test_401_does_not_churn_proxy_cookies(self):
        self.request('/s/notes/')
        before = sum(path.endswith('/view-ticket') for _, path, _, _ in FakeManagement.calls)
        for _ in range(12): self.assertEqual(self.request('/s/notes/login-failed')[0], 401)
        self.assertEqual(sum(path.endswith('/view-ticket') for _, path, _, _ in FakeManagement.calls), before)
        for _ in range(5): self.request('/s/notes/ticket-expired')
        self.assertEqual(sum(path.endswith('/view-ticket') for _, path, _, _ in FakeManagement.calls), before)
        value, expiry, issued = self.bridge.backend.cookies['notes']
        self.bridge.backend.cookies['notes'] = (value, expiry, issued - 61)
        self.request('/s/notes/ticket-expired'); self.request('/s/notes/')
        self.assertEqual(sum(path.endswith('/view-ticket') for _, path, _, _ in FakeManagement.calls), before + 1)
    def test_refusals(self):
        before = len(FakeManagement.calls)
        for path in ('/v2/services', '/v2/sessions', '/manage', '/__gateway/access', '/s/notes/../v2/services', '/s/notes/%252e%252e/x', '//evil/', '/s/notes/__cutex/auth'):
            self.assertIn(self.request(path)[0], (400, 404))
        for headers in ({'X-Ipad-Gate': 'wrong'}, {'Origin': 'https://evil.test'}, {'Sec-Fetch-Site': 'cross-site'}, {'Service-Worker': 'script'}, {'Upgrade': 'websocket'}):
            self.assertIn(self.request(headers=headers)[0], (400, 403))
        # Reserved target validation currently follows registry fetch; no other API permitted.
        for method, path, _, _ in FakeManagement.calls[before:]:
            self.assertEqual((method, path), ('GET', '/v2/services?visibility=all'))
    def test_safe_paths(self):
        for target in ('/%2f/evil', '/%5cfoo', '/%00', '/%252e%252e/x', 'https://evil/'):
            self.assertFalse(gateway.safe_target(target), target)
        self.assertTrue(gateway.safe_target('/chapter.html?q=x'))
    def test_host_configuration_is_required_before_binding(self):
        path = pathlib.Path(self.tmp.name)/'invalid-bridge.json'
        for host in (None, '', 'https://services.example.test', 'services.example.test:443', 'bad\r\nhost'):
            cfg = {'port':0}
            if host is not None: cfg['host'] = host
            path.write_text(json.dumps(cfg))
            with patch.object(gateway, 'LimitedServer') as server:
                with self.assertRaises(ValueError): gateway.serve(path)
                server.assert_not_called()
        self.assertEqual(gateway.validate_config({'host': TEST_HOST})['host'], TEST_HOST)
    def test_requests_cannot_choose_another_configured_host(self):
        self.assertEqual(self.request(headers={'Host':'other.example.test'})[0], 400)
        self.assertEqual(self.request(headers={'Origin':'https://other.example.test'})[0], 403)
    def test_registry_is_only_source_of_remote_grants(self):
        FakeManagement.remote_access = False
        before = len(FakeManagement.calls)
        self.assertEqual(self.request('/s/notes/')[0], 404)
        self.assertNotIn(b'&lt;Notes&gt;', self.request()[2])
        self.assertTrue(all(p == '/v2/services?visibility=all' for _, p, _, _ in FakeManagement.calls[before:]))
        # Legacy file can never revive a registry-disabled or old-backend service.
        pathlib.Path(self.bridge.cfg['access_file']).write_text('{"notes":{"slug":"notes","ownerCutexSessionId":null,"createdAt":null}}')
        self.assertEqual(self.request('/s/notes/')[0], 404)
        for value in (False, None, 'true', 1):
            self.assertFalse(gateway.allowed(self.bridge.cfg, {'remoteAccess': value}))
        self.assertFalse(gateway.allowed(self.bridge.cfg, {}))
        FakeManagement.remote_access = True
        self.assertEqual(self.request('/s/notes/')[0], 200)


if __name__ == '__main__': unittest.main()
