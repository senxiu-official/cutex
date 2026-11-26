"""Read-only orphan writer preflight for staged owner release kits."""
import importlib.util
import json
import os
import pathlib
import sqlite3
import stat
from contextlib import closing


def load(path):
    return json.loads(pathlib.Path(path).read_text(encoding='utf-8-sig'))


def _require(ok, message):
    if not ok:
        raise RuntimeError(message)


def _ordered_snapshot_reader():
    root = pathlib.Path(__file__).resolve().parent
    source = root / 'shared/sqlite_snapshot.py'
    if not source.is_file():
        source = root.parent / 'scripts/r266-compat/sqlite_snapshot.py'
    spec = importlib.util.spec_from_file_location('ordered_input_snapshot', source)
    reader = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(reader)
    return reader


def _sqlite_file_stamp(path):
    try:
        entry = path.lstat()
    except FileNotFoundError:
        return None
    _require(stat.S_ISREG(entry.st_mode) and not getattr(entry, 'st_file_attributes', 0) & 0x400,
             f'CHECK ordered automatic input: unsafe SQLite path {path}')
    return (entry.st_dev, entry.st_ino, entry.st_mtime_ns, entry.st_size)


def _read_ordered_database(database, read):
    wal = database.with_name(database.name + '-wal')
    journal = database.with_name(database.name + '-journal')
    paths = database, wal, journal
    for _ in range(3):
        before = tuple(map(_sqlite_file_stamp, paths))
        main_stamp, wal_stamp, journal_stamp = before
        _require(journal_stamp is None or journal_stamp[-1] == 0,
                 f'CHECK ordered automatic input: unresolved SQLite recovery journal {journal}')
        if main_stamp is None:
            _require(wal_stamp is None, f'CHECK ordered automatic input: missing SQLite database {database}')
            result = []
        elif wal_stamp is None or wal_stamp[-1] == 0:
            # A plain READONLY connection can leave a WAL and SHM on a closed database.
            uri = database.absolute().as_uri() + '?mode=ro&immutable=1'
            with closing(sqlite3.connect(uri, uri=True)) as db:
                db.execute('PRAGMA trusted_schema=OFF')
                result = read(db)
        else:
            reader = _ordered_snapshot_reader()
            try:
                main = reader.read_file(database)
                log = reader.read_file(wal)
            except reader.Refusal as error:
                if error.code == 'source_changed':
                    continue
                raise
            if before != tuple(map(_sqlite_file_stamp, paths)):
                continue
            db, _ = reader.connect_snapshot(main, log)
            with closing(db):
                result = read(db)
        if before == tuple(map(_sqlite_file_stamp, paths)):
            return result
    raise RuntimeError(f'CHECK ordered automatic input: SQLite database changed while being read: {database}')


def _ordered_manifest_ids(database):
    def read(db):
        if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='automatic_input_publications'").fetchone():
            return []
        return [row[0] for row in db.execute("SELECT batch FROM automatic_input_publications ORDER BY rowid LIMIT 10")]

    return _read_ordered_database(database, read)


def _ordered_inbox_ids(database, thread):
    def read(db):
        tables = {row[0] for row in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        active = []
        if 'inputs' in tables:
            active.extend(row[0] for row in db.execute("""
                SELECT message_id FROM inputs WHERE thread_id=? AND state IN ('queued','processing')
                AND json_type(payload,'$.metadata.automaticInputOrder')='array' LIMIT 10
                """, (thread,)))
        if 'notes' in tables:
            active.extend(row[0] for row in db.execute("""
                SELECT message_id FROM notes WHERE thread_id=? AND state IN ('pending','reserved')
                AND (json_extract(payload,'$.metadata.automaticNoteBatch') IS NOT NULL
                     OR json_extract(payload,'$.metadata.automaticBatchMember')=1) LIMIT 10
                """, (thread,)))
        return active

    return _read_ordered_database(database, read)


def ordered_input_preflight(deploy, *, previous=False):
    """Before selecting a build without ordered input, require drained inboxes."""
    supported = deploy.plan.get('orderedAutomaticInput', {}).get('previous' if previous else 'target', False)
    _require(type(supported) is bool, 'CHECK ordered automatic input: malformed compatibility flag')
    if supported:
        return
    home = deploy.home
    manifests = _ordered_manifest_ids(home / 'runtime/management-v2/management-state.sqlite3')
    deploy.persisted()
    pending = []
    for row in deploy.session_records:
        claude = row.get('claude_session')
        if claude and claude.get('state_dir') and claude.get('thread_id'):
            ids = _ordered_inbox_ids(pathlib.Path(claude['state_dir']) / 'state.sqlite3', claude['thread_id'])
            if ids:
                pending.append(f"{row['cutex_session_id']}: {', '.join(ids)}")
    _require(not (manifests or pending),
             'CHECK ordered automatic input: drain or reconcile with the ordered-capable build before starting '
             'the older build. Unresolved daemon batches: ' + repr(manifests)
             + '; adapter inboxes: ' + repr(pending))


def process_rows(deploy):
    """Add complete command/parent evidence without changing the writer fence."""
    if os.name == 'nt':
        code = "@(Get-CimInstance Win32_Process | ForEach-Object { [pscustomobject]@{pid=$_.ProcessId;ppid=$_.ParentProcessId;commandLine=$_.CommandLine;image=$_.ExecutablePath;name=$_.Name;born=$(if($_.CreationDate){$_.CreationDate.ToFileTimeUtc()}else{0})} }) | ConvertTo-Json -Compress"
        rows = json.loads(deploy.command(['powershell.exe', '-NoProfile', '-NonInteractive', '-Command', code], timeout=60))
        return rows if isinstance(rows, list) else [rows] if rows else []
    import shlex
    rows = deploy._legacy_process_rows()
    for row in rows:
        entry = pathlib.Path('/proc/' + str(row['pid']))
        try:
            args = (entry / 'cmdline').read_bytes().decode('utf-8', 'replace').rstrip('\0').split('\0')
            stat = (entry / 'stat').read_text()
            born = int(stat[stat.rfind(')') + 2:].split()[19])
            if born == row.get('born'):
                row['argv'] = args
                row['commandLine'] = shlex.join(args)
        except (OSError, ValueError, IndexError):
            pass
    return rows


def service_process_ids(self):
    p=self.plan
    if p['adapter']=='runtime-host':
        status=json.loads(self.command([p['controller'],'status'],timeout=80))
        return {r['pid'] for r in status['services'] if r['id'] in p['services'] and r.get('state')=='running' and r.get('pid',0)>0}
    if p['adapter']=='systemd':
        return {int(self.command(['systemctl','--user','show',unit,'--property=MainPID','--value'],timeout=30).strip()) for unit in p['services']}
    # service_health has already verified these private service receipts.
    state=self.root/'private-service-state'
    owned=load(state/'services.json')
    return {load(path)['pid'] for path in owned.values()}
def orphan_agent_preflight(self):
    import ntpath,shlex
    rows=self.process_rows();by_pid={r['pid']:r for r in rows}
    services=self.service_process_ids()
    roots=[os.path.normcase(str(pathlib.Path(x).resolve())).rstrip('\\/') for x in self.plan['writerRoots']]
    def relevant(image):
        value=os.path.normcase(str(pathlib.Path(image.removesuffix(' (deleted)')).resolve()))
        return any(value==root or value.startswith(root+os.sep) for root in roots)
    def owned(row):
        seen=set()
        while row and row['pid'] not in seen:
            if row['pid'] in services:return True
            seen.add(row['pid']);parent=by_pid.get(row.get('ppid'))
            # PID reuse must not turn an orphan into an owned descendant.
            if parent and (not parent.get('born') or not row.get('born') or parent['born']>row['born']):return False
            row=parent
        return False
    offenders=[]
    for row in rows:
        image=row.get('image') or ''
        if not image or ntpath.basename(image.removesuffix(' (deleted)')).lower() not in ('cutex','cutex.exe') or not relevant(image):continue
        command=row.get('commandLine')
        args=row.get('argv')
        if args is None and command:
            try:args=shlex.split(command,posix=False)
            except ValueError:args=[]
        if not args or args[1:3]!=['agent','serve'] or owned(row):continue
        parent=by_pid.get(row.get('ppid'))
        reason=('parent PID %s is absent' % row.get('ppid')) if parent is None else ('parent PID %s is not owned by the running kit services (or PID was reused)' % row.get('ppid'))
        stop=('Stop-Process -Id %d' if os.name=='nt' else 'kill -TERM %d') % row['pid']
        offenders.append('PID %d: %s\n  Reason: %s; this Cutex writer survives the managed-service stop and fails CHECK 3.\n  Stop this exact process after verifying its command line: %s' % (row['pid'],command or '<unreadable>',reason,stop))
    _require(not offenders,'CHECK ONLY: orphan Cutex agent serve process(es):\n'+'\n'.join(offenders)+'\nNo process was stopped. Re-run check after stopping the listed orphan(s).')
    return {'orphanAgentServe':[]}
