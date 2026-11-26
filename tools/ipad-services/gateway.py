"""Private browser bridge to the existing cutex service proxy; no public listener."""
import argparse, html, http.client, http.server, json, pathlib, re, secrets, threading, time
from urllib.parse import unquote, urlsplit

HOP = {'connection', 'keep-alive', 'proxy-authenticate', 'proxy-authorization',
       'te', 'trailer', 'transfer-encoding', 'upgrade', 'host', 'authorization',
       'content-length', 'accept-encoding', 'forwarded', 'x-real-ip', 'x-ipad-gate'}
CSP = "default-src 'self' data: blob:; script-src 'self' 'unsafe-inline' 'unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self'; worker-src 'none'; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'self'"

class ApiError(RuntimeError):
    def __init__(self, status):
        self.status = status
        super().__init__('Local service API HTTP '+str(status))

def binding(service):
    return {k: service.get(k) for k in ('slug', 'ownerCutexSessionId', 'createdAt')}

def allowed(cfg, service):
    # Registry is the only source of grants. Missing/old backend fields fail closed.
    return service.get('remoteAccess') is True

def safe_target(target):
    path = target.split('?', 1)[0]
    for _ in range(5):
        if (not path.startswith('/') or path.startswith('//') or '\\' in path
            or any(ord(c) < 32 or ord(c) == 127 for c in path)
            or any(p in ('.', '..') for p in path.split('/'))):
            return False
        decoded = unquote(path)
        if decoded == path:
            return True
        path = decoded
    return False

class Backend:
    def __init__(self, cfg):
        self.cfg = cfg
        self.cookies = {}; self.cookie_lock = threading.Lock()

    def api(self, method, path, body=None):
        valid = ((method == 'GET' and path in ('/v2/services', '/v2/services?visibility=all', '/v2/capabilities'))
                 or (method == 'POST' and re.fullmatch(r'/v2/services/[a-z0-9][a-z0-9-]*/view-ticket', path))
                 or (method == 'PATCH' and re.fullmatch(r'/v2/services/[a-z0-9][a-z0-9-]*', path)
                     and isinstance(body, dict) and type(body.get('remoteAccess')) is bool
                     and set(body) <= {'remoteAccess', 'ifRegistration'}))
        if not valid: raise ValueError('Management operation not allowed by the service bridge')
        # No redirects, environment proxies, credential logging or browser exposure.
        token = json.loads(pathlib.Path(self.cfg['cutex_config']).read_text(encoding='utf-8-sig'))['management_api_token']
        c = http.client.HTTPConnection('127.0.0.1', self.cfg['management_port'], timeout=20)
        try:
            c.request(method, path, body=json.dumps(body).encode() if body is not None else b'' if method == 'POST' else None,
                      headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
            r = c.getresponse()
            body = r.read(4 * 1024 * 1024 + 1)
            if r.status not in (200, 201):
                raise ApiError(r.status)
            if len(body) > 4 * 1024 * 1024:
                raise RuntimeError('Local service API unavailable')
            return json.loads(body)
        finally:
            c.close()

    def services(self):
        return {s['slug']: s for s in self.api('GET', '/v2/services?visibility=all')['services']
                if re.fullmatch('[a-z0-9][a-z0-9-]*', s['slug']) and not s.get('archived')
                and s.get('runtime') != 'worker'}

    def cookie(self, slug):
        with self.cookie_lock:
            cached = self.cookies.get(slug)
            if cached and cached[1] > time.monotonic(): return cached[0]
            value = self.new_cookie(slug)
            now = time.monotonic()
            self.cookies[slug] = (value, now + 10 * 60 * 60, now)
            return value

    def auth_refused(self, slug):
        with self.cookie_lock:
            cached = self.cookies.get(slug)
            if cached and time.monotonic() - cached[2] >= 60:
                self.cookies.pop(slug, None)

    def new_cookie(self, slug):
        # Per-request tickets stay inside the local host; never grant a browser management access.
        ticket = self.api('POST', '/v2/services/' + slug + '/view-ticket')
        c = http.client.HTTPConnection('127.0.0.1', self.cfg['management_port'], timeout=20)
        try:
            c.request('GET', ticket['authPath'], headers={'Host': slug + '.svc.localhost:' + str(self.cfg['management_port'])})
            r = c.getresponse(); r.read(65536)
            if r.status != 302 or not r.getheader('Set-Cookie'):
                raise RuntimeError('Local service ticket exchange unavailable')
            return r.getheader('Set-Cookie').split(';', 1)[0]
        finally:
            c.close()

class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def setup(self):
        super().setup(); self.connection.settimeout(30)
    def log_message(self, *_):
        pass  # URLs, cookies and bodies are never logged.

    def small(self, status, body, content_type='text/plain; charset=utf-8'):
        data = body.encode()
        self.send_response(status); self.send_header('Content-Type', content_type)
        self.send_header('Content-Length', str(len(data))); self.send_header('Cache-Control', 'no-store')
        self.send_header('Content-Security-Policy', CSP); self.send_header('Connection', 'close')
        self.end_headers(); self.close_connection = True
        if self.command != 'HEAD': self.wfile.write(data)

    def handle_request(self):
        self.response_started = False
        cfg = self.server.cfg
        if not secrets.compare_digest(self.headers.get('X-Ipad-Gate', '').encode(), cfg['bridge_secret'].encode()):
            return self.small(403, 'Forbidden')
        if self.headers.get('Host') != cfg['host'] or not safe_target(self.path):
            return self.small(400, 'Invalid request')
        if (self.headers.get('Origin') not in (None, 'https://' + cfg['host'])
            or self.headers.get('Sec-Fetch-Site') in ('same-site', 'cross-site')
            or (self.command not in ('GET', 'HEAD', 'OPTIONS') and self.headers.get('Origin') != 'https://' + cfg['host'])):
            return self.small(403, 'Cross-site request blocked')
        if (self.headers.get('Service-Worker') or self.headers.get('Sec-Fetch-Dest') == 'serviceworker'
            or self.headers.get('Upgrade') or self.headers.get('Transfer-Encoding')):
            return self.small(400, 'Service workers, upgrades and chunked uploads are unsupported')
        try:
            services = self.server.backend.services()
            if self.path == '/' and self.command in ('GET', 'HEAD'):
                items = []
                for slug, s in sorted(services.items()):
                    if not allowed(cfg, s): continue
                    path = s.get('path') or '/'
                    if not safe_target(path): path = '/'
                    items.append('<li><a href="/s/' + slug + html.escape(path, quote=True) + '">' + html.escape(s.get('displayName') or slug) + '</a> · ' + html.escape(s.get('health', 'unknown')) + '</li>')
                return self.small(200, '<!doctype html><html lang="zh-CN"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>cutex 服务</title><h1>cutex 服务 · ' + html.escape(cfg.get("host_label", cfg["host"])) + '</h1><p>打开已登记的页面。刷新可更新列表。</p><ul>' + ''.join(items) + '</ul></html>', 'text/html; charset=utf-8')
            match = re.fullmatch(r'/s/([a-z0-9][a-z0-9-]*)(/.*)?', self.path)
            if not match or match[1] not in services or not allowed(cfg, services[match[1]]):
                return self.small(404, 'Service not registered')
            slug, target = match[1], match[2] or '/'
            if match[2] is None:
                self.send_response(308); self.send_header('Location', '/s/' + slug + '/')
                self.send_header('Content-Length', '0'); self.end_headers(); return
            if target.split('?', 1)[0].startswith('/__cutex'):
                return self.small(404, 'Reserved path')
            if len(self.headers.get_all('Content-Length', [])) > 1:
                return self.small(400, 'Ambiguous body length')
            length = int(self.headers.get('Content-Length', '0'))
            if length < 0 or length > 8 * 1024 * 1024:
                return self.small(413, 'Upload too large')
            self.connection.settimeout(30)
            body = self.rfile.read(length) if length else None
            if length and len(body) != length: return self.small(400, 'Incomplete body')
            internal_host = slug + '.svc.localhost:' + str(cfg['management_port'])
            nominated = {v.strip().lower() for v in self.headers.get('Connection', '').split(',')}
            headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP | nominated
                       and not k.lower().startswith('x-forwarded-') and k.lower() != 'cookie'}
            cookie = self.server.backend.cookie(slug)
            browser_cookie = self.headers.get('Cookie', '')
            # Never accept native proxy cookies from the browser.
            cookies = [p.strip() for p in browser_cookie.split(';') if '=' in p and not p.strip().startswith('cutex_svc_')]
            headers.update({'Host': internal_host, 'Cookie': '; '.join(cookies + [cookie]), 'Connection': 'close'})
            if self.headers.get('Origin') is not None:
                headers = {k: v for k, v in headers.items() if k.lower() != 'origin'}
                headers['Origin'] = 'http://' + internal_host
            upstream = http.client.HTTPConnection('127.0.0.1', cfg['management_port'], timeout=30)
            try:
                upstream.request(self.command, target, body=body, headers=headers)
                response = upstream.getresponse()
                if response.status == 401 and response.getheader('X-Cutex-Service-Auth') == 'ticket-required':
                    # A daemon restart clears its memory-only browser grants.
                    # Refresh the next request; never replay a write automatically.
                    self.server.backend.auth_refused(slug)
                self.send_response(response.status)
                for key, value in response.getheaders():
                    low = key.lower()
                    if low in HOP | {'content-security-policy', 'set-cookie', 'location', 'alt-svc', 'report-to', 'reporting-endpoints', 'nel', 'content-security-policy-report-only', 'refresh', 'link'}: continue
                    self.send_header(key, value)
                location = response.getheader('Location')
                if location:
                    parsed = urlsplit(location)
                    if parsed.netloc and parsed.netloc != internal_host:
                        raise RuntimeError('External redirect refused')
                    if location.startswith('//') or '\\' in location:
                        raise RuntimeError('Unsafe redirect')
                    from urllib.parse import urljoin
                    redirected = urlsplit(urljoin(target, location))
                    self.send_header('Location', '/s/' + slug + (redirected.path or '/') + ('?' + redirected.query if redirected.query else '') + ('#' + redirected.fragment if redirected.fragment else ''))
                for key, value in response.getheaders():
                    if key.lower() != 'set-cookie' or value.startswith('cutex_svc_'): continue
                    parts = [p.strip() for p in value.split(';')]
                    parts = [p for p in parts if not p.lower().startswith(('domain=', 'path='))]
                    self.send_header('Set-Cookie', '; '.join(parts) + '; Path=/s/' + slug + '/; Secure')
                self.send_header('Content-Security-Policy', CSP)
                self.send_header('Referrer-Policy', 'no-referrer'); self.send_header('X-Content-Type-Options', 'nosniff')
                self.send_header('Connection', 'close'); self.end_headers(); self.response_started = True; self.close_connection = True
                if self.command != 'HEAD':
                    while chunk := response.read(65536): self.wfile.write(chunk)
            finally:
                upstream.close()
        except (BrokenPipeError, ConnectionResetError):
            self.close_connection = True
        except Exception:
            # Avoid a second HTTP response if upstream headers were already written.
            if getattr(self, '_headers_buffer', None):
                self._headers_buffer.clear()
            self.close_connection = True
            try:
                if not self.response_started: self.small(502, 'Service unavailable; refresh or ask its agent to check it.')
            except OSError: pass

    do_GET = do_HEAD = do_POST = do_PUT = do_PATCH = do_DELETE = do_OPTIONS = handle_request

class LimitedServer(http.server.ThreadingHTTPServer):
    daemon_threads = True
    def __init__(self, *args, **kwargs):
        self.slots = threading.BoundedSemaphore(16)
        super().__init__(*args, **kwargs)
    def process_request(self, request, address):
        if not self.slots.acquire(blocking=False):
            request.close(); return
        try: super().process_request(request, address)
        except Exception:
            self.slots.release(); raise
    def process_request_thread(self, request, address):
        try: super().process_request_thread(request, address)
        finally: self.slots.release()

def validate_config(cfg):
    host = cfg.get('host')
    if (not isinstance(host, str) or len(host) > 253
        or not re.fullmatch(r'[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)+', host)):
        raise ValueError('bridge.json requires an explicit lowercase DNS host, without scheme, port or path')
    if 'host_label' in cfg and (not isinstance(cfg['host_label'], str) or len(cfg['host_label']) > 200):
        raise ValueError('host_label must be a short string')
    return cfg

def serve(config):
    cfg = validate_config(json.loads(pathlib.Path(config).read_text(encoding='utf-8-sig')))
    server = LimitedServer(('127.0.0.1', cfg['port']), Handler)
    server.daemon_threads = True; server.cfg = cfg; server.backend = Backend(cfg)
    server.serve_forever()

if __name__ == '__main__':
    ap = argparse.ArgumentParser(); ap.add_argument('config'); serve(ap.parse_args().config)
