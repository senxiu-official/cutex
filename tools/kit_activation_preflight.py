"""Read-only orphan writer preflight for staged owner release kits."""
import json
import os
import pathlib


def load(path):
    return json.loads(pathlib.Path(path).read_text(encoding='utf-8-sig'))


def _require(ok, message):
    if not ok:
        raise RuntimeError(message)


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
