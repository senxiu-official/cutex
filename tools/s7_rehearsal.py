"""Private previous -> S7 -> current-SQL R1 -> previous -> S7 rehearsal.

Only synthetic homes; only recorded child handles; loopback ports 24973/24963.
Uses the shared fixture/runner from pinned scale-gate 55b271e2. No provider calls.
"""
import argparse
import concurrent.futures
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import time
import uuid
import urllib.request


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--previous', type=Path, required=True)
    p.add_argument('--candidate', type=Path, required=True)
    p.add_argument('--gate-dir', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    args = p.parse_args()
    args.output.mkdir(parents=True)
    sys.path.insert(0, str(args.gate_dir.resolve()))
    from fixture import generate, write
    import run
    from session_rows_r1 import export
    from s7_scale_gate import seed
    home = Path.home() / ('.s7-rehearsal-' + uuid.uuid4().hex[:10])
    generate(home, .1, 1, 24973, 24963)
    seed(home)
    root = home / '.cutex/runtime/management-v2'
    sid, added, gone = 'cutex.scale-00000', 'cutex.scale-00165', 'cutex.scale-00003'
    at = '2026-11-04T01:02:03Z'
    source = root / 'session-activity-state.json'
    activity = json.loads(source.read_text())
    activity['sessions'].pop(added)
    write(source, activity)
    queue_path = root / 'user-input-state.json'
    queue = json.loads(queue_path.read_text())
    delivery = {}
    queue['sessions'][sid]['items'] = []
    queue['sessions'][sid]['pause'] = dict(pausedAt=at, reason='manual')
    for number, state in enumerate(('sending', 'unknown', 'accepted')):
        client = f's7-unresolved-{state}'
        value = dict(state=state, threadId='s7-thread')
        value.update(dict(acceptedAt=at, turnId='s7-turn') if state == 'accepted' else dict(since=at))
        queue['sessions'][sid]['identities'][client] = dict(canonicalSha256=str(number)*64, firstSeenAt=at, delivery=value)
        delivery[client] = value
        if state != 'accepted':
            queue['sessions'][sid]['items'].append(dict(managementRequestId='s7-'+state, threadId='s7-thread',
                item=dict(queueId='s7-queue-'+state, clientUserMessageId=client,
                    origin=dict(kind='tui', clientId='s7-fixture'), input=[dict(type='text', text='Synthetic pending '+state)],
                    position=number, revision=1, state='queued', createdAt=at, updatedAt=at)))
    write(queue_path, queue)
    report = dict(home=str(home), synthetic=True, previous_sha='9cf4cbabdf481fa0c51a8023b98f7930095ea5f3',
                  stages=[], executables={label: dict(path=str(exe), sha256=hashlib.file_digest(exe.open('rb'), 'sha256').hexdigest())
                      for label, exe in [('previous', args.previous), ('candidate', args.candidate)]})

    def save():
        (args.output / 'result.json').write_text(json.dumps(report, indent=2))

    class Instance(run.Instance):
        def request(self, path, body=None):
            request=urllib.request.Request(f'http://127.0.0.1:{self.mp}{path}',
                data=None if body is None else json.dumps(body).encode(),
                headers={'Authorization':f'Bearer {self.token}','Content-Type':'application/json'})
            with self.opener.open(request,timeout=180) as response:
                value=json.load(response)
            if isinstance(value,dict) and value.get('error'):
                raise RuntimeError(value['error'])
            return value

    def stage(label, exe, action):
        out = args.output / label
        out.mkdir()
        instance = Instance(exe.resolve(), home, out, 24973, 24963)
        started = time.perf_counter()
        try:
            instance.start()
            record = dict(stage=label, startup_ms=(time.perf_counter()-started)*1000, pids=[p.pid for p in instance.processes])
            report['stages'].append(record)
            action(instance)
            queue_value=instance.request(f'/v2/sessions/{sid}/cutex/requests',dict(requestId='s7-queue-'+uuid.uuid4().hex,
                method='cutex/userInput/queue/list',params={}))
            def walk(value):
                if isinstance(value,dict):
                    yield value
                    for child in value.values(): yield from walk(child)
                elif isinstance(value,list):
                    for child in value: yield from walk(child)
            projected={item['clientUserMessageId']:item.get('delivery') for item in walk(queue_value)
                if item.get('queueId') in ('s7-queue-sending','s7-queue-unknown')}
            assert projected=={'s7-unresolved-sending':'sending','s7-unresolved-unknown':'unknown'},queue_value
            record['unresolved_queue']=projected
        finally:
            instance.close()
            save()

    def session(instance, session_id=sid):
        value = instance.request('/v2/sessions/' + session_id)
        return value.get('session', value)

    def focus(instance, expected, owner):
        value = instance.request(f'/v2/sessions/{sid}/cutex/requests', dict(
            requestId='s7-' + uuid.uuid4().hex, method='cutex/focus/set',
            params=dict(expectedRevision=expected, owner=owner, mobileMuted=True, source='s7-rehearsal')))
        if value.get('cutex', {}).get('error'):
            raise ValueError(value)

    def previous_writes(instance):
        session(instance)
        focus(instance, 1, 'pc')

    stage('previous-writes', args.previous, previous_writes)
    frozen_sources = {store: (root / (store+'-state.json')).read_bytes() for store in ('session-activity', 'session-projection')}

    def migrate(label):
        for store in ('session-activity', 'session-projection'):
            started = time.perf_counter()
            command = [str(args.candidate), 'storage', 'session-rows-import', '--store', store, '--daemon-stopped']
            completed = subprocess.run(command, env=run.env_for(home, args.output / (label+'-'+store+'.jsonl')),
                capture_output=True, creationflags=getattr(subprocess, 'CREATE_NO_WINDOW', 0), timeout=180)
            if completed.returncode:
                raise RuntimeError(completed.stderr.decode(errors='replace'))
            report.setdefault('imports', []).append(dict(stage=label, store=store, ms=(time.perf_counter()-started)*1000,
                receipt=json.loads(completed.stdout), disk_bytes=sum(f.stat().st_size for f in root.glob(store+'.sqlite3*'))))
    migrate('first')
    for store, before in frozen_sources.items():
        assert (root / (store+'-state.json')).read_bytes() == before, 'import changed evidence source'

    def candidate_writes(instance):
        value = session(instance)
        assert value['focus']['revision'] == 2
        focus(instance, 2, 'mobile')
        assert session(instance)['focus']['revision'] == 3
        with concurrent.futures.ThreadPoolExecutor(max_workers=32) as pool:
            values=list(pool.map(lambda _:session(instance),range(32)))
        assert all(value['focus']['revision']==3 for value in values)
        report['parallel_point_readers']=dict(readers=32,passed=len(values))
    stage('candidate-writes', args.candidate, candidate_writes)

    # Upgrade-only add/change/delete injection into a stopped current SQL copy.
    # Normal typed event generation/revert/fences are covered by Rust tests.
    db = sqlite3.connect(root / 'session-activity.sqlite3')
    changed = json.loads(db.execute('SELECT data FROM session_rows WHERE id=?', (sid,)).fetchone()[0])
    changed.update(revision=changed['revision']+10, lastConversationAt='2026-11-04T06:07:08Z')
    db.execute('UPDATE session_rows SET data=? WHERE id=?', (json.dumps(changed), sid))
    new = json.loads(json.dumps(changed))
    new['revision'] = 4
    new['lastOutput']['association']['cutex_session_id'] = added
    db.execute('INSERT INTO session_rows VALUES(?,0,?)', (added, json.dumps(new)))
    db.execute('UPDATE session_rows SET deleted=1,data=NULL WHERE id=?', (gone,))
    db.commit()
    db.close()
    report['r1_injections'] = dict(added=added, changed=sid, deleted=gone, unresolved_deliveries=delivery)
    report['exports'] = []
    for store in ('session-activity', 'session-projection'):
        started = time.perf_counter()
        receipt = export(root, store, root / (store+'-state.json'))
        receipt['ms'] = (time.perf_counter()-started)*1000
        report['exports'].append(receipt)
    exported = json.loads(source.read_text())
    assert exported['sessions'][sid] == changed
    assert exported['sessions'][added] == new
    assert gone not in exported['sessions'] and gone in exported['deletedSessions']
    # Preserve the SQL+WAL evidence under an explicit kit displacement before
    # old writers resume. A later upgrade imports their current JSON afresh.
    displaced = args.output / 'displaced-sql'
    displaced.mkdir()
    for store in ('session-activity', 'session-projection'):
        for path in root.glob(store+'.sqlite3*'):
            shutil.move(str(path), displaced / path.name)

    def rolled_back(instance):
        value = session(instance)
        assert value['focus']['revision'] == 3 and value['focus']['owner'] == 'mobile'
        assert value['activity']['lastConversationAt'] == changed['lastConversationAt']
        assert value['activity']['revision'] == changed['revision']
        assert session(instance, added)['activity']['lastConversationAt'] == new['lastConversationAt']
        assert session(instance, added)['activity']['revision'] == new['revision']
        assert session(instance, gone)['activity']['lastConversationAt'] is None
        assert session(instance, gone)['activity']['revision'] == 0
        focus(instance, 3, 'pc')
        assert session(instance)['focus']['revision'] == 4
    stage('rollback-previous-reads-writes', args.previous, rolled_back)
    # R0: existing delivery JSON is still read by the exact preceding build.
    current = json.loads(queue_path.read_text())
    assert {key: current['sessions'][sid]['identities'][key]['delivery'] for key in delivery} == delivery
    migrate('reentry')
    stage('new-reentry', args.candidate, lambda instance: (
        session(instance)['focus']['revision'] == 4 or (_ for _ in ()).throw(AssertionError('stale SQL reentry'))))
    report['result'] = 'PASS'
    save()
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
