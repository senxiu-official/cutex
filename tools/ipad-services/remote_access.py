"""Owner-local CLI. Never run migration before the new backend is deployed.

The HTTP bridge exposes no management endpoint; this process uses the original
host-local credential and fixed APIs. --apply explicitly authorizes migration.
"""
import argparse, hashlib, json, os, pathlib, socket
from gateway import Backend, ApiError, allowed, binding

def require_support(backend):
    value = backend.api('GET', '/v2/capabilities')
    if value.get('features', {}).get('servicesRemoteAccessV1') is not True:
        raise RuntimeError('Backend lacks services-remote-access-v1; keep the installed gateway unchanged')

def set_remote(backend, service, enabled):
    if not isinstance(service.get('createdAt'), str) or not service['createdAt']:
        raise ValueError('Service has no registration timestamp')
    return backend.api('PATCH', '/v2/services/'+service['slug'], {
        'remoteAccess': enabled,
        'ifRegistration': {k: service.get(k) for k in ('createdAt', 'ownerCutexSessionId')},
    })

def atomic_json(path, value):
    temp = path.with_suffix(path.suffix+'.tmp')
    temp.write_bytes(json.dumps(value, indent=2).encode()); os.replace(temp, path)

def migrate(cfg, backend, root, apply=False):
    require_support(backend)
    if apply and isinstance(cfg.get('port'), int):
        try:
            probe = socket.create_connection(('127.0.0.1', cfg['port']), timeout=.5)
        except ConnectionRefusedError:
            pass
        except OSError as error:
            raise RuntimeError('Cannot prove the private adapter stopped; no grants migrated') from error
        else:
            probe.close()
            raise RuntimeError('Stop the private adapter before retiring its legacy grant file')
    root = pathlib.Path(root)
    legacy = pathlib.Path(cfg.get('access_file', root/'access.json'))
    if legacy.resolve() != (root/'access.json').resolve() or legacy.is_symlink():
        raise ValueError('Legacy access file must be the private installation access.json')
    receipt = root/'remote-access-migration.json'
    lock = legacy.with_suffix('.lock')
    if apply:
        fd = os.open(lock, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600); os.close(fd)
    saved = None
    try:
        saved = json.loads(receipt.read_text()) if receipt.exists() else None
        if not legacy.exists():
            return saved or {'migrated': [], 'skipped': [], 'legacyAbsent': True}
        data = legacy.read_bytes()
        digest = hashlib.sha256(data).hexdigest()
        if saved and digest != saved['legacySha256']:
            raise RuntimeError('Legacy file changed after migration; refusing to revive it')
        # Receipts from the previous implementation existed only on completion.
        if saved and saved.get('status', 'complete') == 'complete':
            if apply: legacy.unlink()
            return saved
        grants = json.loads(data)
        if not isinstance(grants, dict): raise ValueError('Legacy grants must be an object')
        services = backend.services()
        matched, skipped = [], []
        for slug, grant in grants.items():
            service = services.get(slug)
            if service and grant == binding(service) and service.get('createdAt'):
                matched.append(slug)
            else: skipped.append(slug)
        result = saved or {'schemaVersion':2, 'status':'partial', 'migrated':[],
                           'completed':[], 'attempted':[], 'legacySha256':digest}
        result['skipped'] = skipped
        if not apply: return {**result, 'matched':matched, 'dryRun':True}
        atomic_json(receipt, result)
        for slug in matched:
            identity = binding(services[slug])
            if identity in result['completed']:
                continue  # Owner may have disabled it after a previous success.
            if identity in result['attempted']:
                # The previous process may have lost the reply after commit.
                # Never replay that grant; leave the current owner choice alone.
                continue
            result['attempted'].append(identity)
            atomic_json(receipt, result)
            try:
                set_remote(backend, services[slug], True)
            except ApiError as error:
                if error.status in (400, 401, 403, 404, 409, 422):
                    result['attempted'].remove(identity)  # Known pre-write refusal.
                raise
            result['completed'].append(identity)
            result['migrated'].append(slug)
            result['attempted'].remove(identity)
            atomic_json(receipt, result)
            current = backend.services().get(slug)
            if not current or binding(current) != identity or not allowed(cfg, current):
                raise RuntimeError('Migration grant changed while verifying; progress retained without replay')
        if legacy.read_bytes() != data:
            raise RuntimeError('Legacy file changed while migrating; not retiring it')
        result['skippedUnconfirmed'] = list(result['attempted'])
        result['status'] = 'complete'
        atomic_json(receipt, result)
        legacy.unlink()
        return result
    finally:
        # Preserve every acknowledged grant (and unconfirmed attempt) on failure.
        # A later retry must never revive permissions the owner just disabled.
        if apply:
            try:
                if 'result' in locals() and result.get('status') == 'partial':
                    atomic_json(receipt, result)
            finally:
                lock.unlink()

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('op', choices=['list', 'enable', 'disable', 'migrate'])
    ap.add_argument('slug', nargs='?'); ap.add_argument('--apply', action='store_true')
    ap.add_argument('--config', type=pathlib.Path, default=pathlib.Path(os.environ['LOCALAPPDATA'])/'ipad-services/bridge.json')
    a = ap.parse_args(); cfg = json.loads(a.config.read_text(encoding='utf-8-sig')); backend = Backend(cfg)
    if a.op == 'migrate': print(json.dumps(migrate(cfg, backend, a.config.parent, a.apply))); return
    require_support(backend); services = backend.services()
    if a.op == 'list':
        for s in services.values(): print(s['slug']+'\t'+str(allowed(cfg, s)).lower())
    else:
        if a.slug not in services: raise ValueError('Service not registered')
        set_remote(backend, services[a.slug], a.op == 'enable')
        print(a.slug+': '+a.op+'d')

if __name__ == '__main__': main()
