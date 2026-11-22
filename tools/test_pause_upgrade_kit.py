"""r290 P9: stop/unknown/race behavior, no live services or credentials."""
import copy
import json
import os
from pathlib import Path
import sys
import subprocess
import tempfile
import types
import unittest
from unittest import mock

import pause_upgrade_kit as kit
import stage_pause_upgrade_kit as staging
import process_scope_kit as scope


def session(sid='paused', *, connected=True, active=True, paused=True, generation=7):
    return {'cutexSessionId': sid, 'displayName': sid,
            'agentPause': {'requestedAt': '2026-11-17T00:00:00Z', 'by': 'owner',
                           'phase': 'paused', 'held': [{'callId': 'old-tool'}]} if paused else None,
            'pauseReasons': [{'kind': 'agent', 'phase': 'paused'}] if paused else [],
            'runtime': {'runtimeGeneration': generation, 'appServerConnected': connected,
                        'activeTurnId': 'turn' if active else None,
                        'threadStatus': 'active' if active else 'idle',
                        'status': 'online' if connected else 'offline'}}


class Clock:
    def __init__(self): self.value = 0
    def now(self): return self.value
    def sleep(self, seconds): self.value += seconds


class Fake:
    def __init__(self, rows, asks=()):
        self.rows = copy.deepcopy(rows)
        self.asks = list(asks)
        self.plan = {'pauseUpgrade': {'readinessTimeoutSeconds': 6, 'offlineTimeoutSeconds': 2}}
        self.calls = []
        self.reply = {'ok': True}
        self.stops = True
        self.lose_pause = False
    def sessions(self): return copy.deepcopy(self.rows)
    def api(self, path, method='GET', body=None):
        self.calls.append((path, method, body))
        if method == 'GET': return {'asks': self.asks, 'hasMore': False}
        assert body['method'] == 'cutex/runtime/offline'
        sid = path.split('/')[3]
        row = next(row for row in self.rows if row['cutexSessionId'] == sid)
        if self.stops:
            row['runtime'].update(status='offline', appServerConnected=False, activeTurnId=None, threadStatus='idle')
            if self.lose_pause:
                row.update(agentPause=None, pauseReasons=[])
        return self.reply


class PauseUpgradeTests(unittest.TestCase):
    def idle(self, d):
        clock = Clock()
        return kit.idle(d, True, clock=clock.now, sleep=clock.sleep, out=lambda *a, **kw: None)
    def offline(self, d, rows, op):
        clock = Clock()
        return kit.offline(d, op, rows, clock=clock.now, sleep=clock.sleep, out=lambda *a, **kw: None)
    def test_paused_turn_and_its_admitted_attempt_enter_controlled_stop(self):
        d = Fake([session()], [{'askId': 'ask-1', 'target': 'paused', 'attempt': {'state': 'submitting'}}])
        rows, info = self.idle(d)
        self.assertFalse(info['waitingFor'])
        self.assertFalse(info['submitting'])
        self.assertEqual(info['pausedAdmittedPreserved'][0]['id'], 'ask-1')
        with tempfile.TemporaryDirectory() as directory:
            self.offline(d, rows, Path(directory))
            record = json.loads((Path(directory) / 'manual-online.json').read_text())
            self.assertEqual(record[0]['offlineOutcome'], 'confirmed')
            self.assertEqual(record[0]['pauseBefore'], record[0]['pauseAfterOffline'])
        posts = [call for call in d.calls if call[1] == 'POST']
        self.assertEqual(len(posts), 1)
        self.assertFalse(posts[0][2]['params']['force'])
        self.assertEqual(d.asks[0]['askId'], 'ask-1')
    def test_unpaused_active_or_unrelated_admitted_is_bounded(self):
        for d in (Fake([session(paused=False)]), Fake([session()], [{'target': 'other'}])):
            with self.subTest(rows=d.rows):
                with self.assertRaisesRegex(RuntimeError, '截止时间'):
                    self.idle(d)
                self.assertFalse(any(call[1] == 'POST' for call in d.calls))
    def test_project_pause_enters_maintenance(self):
        row = session(paused=False, active=False)
        row['pauseReasons'] = [{'kind': 'project', 'pauseId': 'pause-1', 'epoch': 3}]
        self.assertEqual(self.idle(Fake([row]))[1]['pausedForMaintenance'], ['paused'])
    def test_project_after_turn_does_not_cancel_its_still_active_turn(self):
        row=session(paused=False);row['pauseReasons']=[{'kind':'project','projectId':'project','pauseId':'pause-1','epoch':3}]
        with self.assertRaisesRegex(RuntimeError,'截止时间'):
            self.idle(Fake([row]))
        # Native status is typed and can precede the bound turn-id event.
        row['runtime'].update(activeTurnId=None,threadStatus={'type':'active'})
        with self.assertRaisesRegex(RuntimeError,'截止时间'):
            self.idle(Fake([row]))
        row['agentPause']={'by':'owner','requestedAt':'now'}
        self.assertEqual(self.idle(Fake([row]))[1]['pausedForMaintenance'],['paused'])
    def test_original_offline_agent_is_never_selected(self):
        d = Fake([session(connected=False, active=False)])
        with tempfile.TemporaryDirectory() as directory:
            self.offline(d, d.sessions(), Path(directory))
            self.assertEqual(json.loads((Path(directory) / 'manual-online.json').read_text()), [])
        self.assertFalse(d.calls)
    def test_owner_offline_between_snapshot_and_stop_does_not_authorize_restore(self):
        d = Fake([session()]); before = d.sessions()
        d.rows[0]['runtime'].update(status='offline', appServerConnected=False)
        with tempfile.TemporaryDirectory() as directory:
            self.offline(d, before, Path(directory))
            row = json.loads((Path(directory) / 'manual-online.json').read_text())[0]
            self.assertFalse(row['selectedForOfflineByThisOperation'])
        self.assertFalse(d.calls)
    def test_new_generation_or_unpaused_turn_is_never_stopped(self):
        for change in ('generation', 'resume'):
            d = Fake([session()]); before = d.sessions()
            if change == 'generation': d.rows[0]['runtime']['runtimeGeneration'] += 1
            else: d.rows[0].update(agentPause=None, pauseReasons=[])
            with tempfile.TemporaryDirectory() as directory:
                with self.assertRaises(RuntimeError): self.offline(d, before, Path(directory))
            self.assertFalse(d.calls)
    def test_unknown_stop_is_not_replayed_or_authorized_for_auto_online(self):
        d = Fake([session()]); d.reply = {'outcomeUnknown': True}; d.stops = False
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(RuntimeError, '禁止重放'): self.offline(d, d.sessions(), Path(directory))
            row = json.loads((Path(directory) / 'manual-online.json').read_text())[0]
            self.assertEqual(row['offlineOutcome'], 'requested')
            self.assertFalse(row['offlinedByThisOperation'])
        self.assertEqual(len(d.calls), 1)
    def test_unconfirmed_stop_has_deadline(self):
        d = Fake([session()]); d.stops = False
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(RuntimeError, '截止时间'): self.offline(d, d.sessions(), Path(directory))
        self.assertEqual(len(d.calls), 1)
    def test_pause_loss_is_evidenced_and_never_repaired_by_resume_or_pause(self):
        d = Fake([session()]); d.lose_pause = True
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(RuntimeError, '暂停状态发生变化'): self.offline(d, d.sessions(), Path(directory))
            self.assertEqual(len(list(Path(directory).glob('pause-changed-*.json'))), 1)
        self.assertEqual(len(d.calls), 1)
    def test_same_operation_stop_is_never_replayed(self):
        d = Fake([session()]); before = d.sessions()
        with tempfile.TemporaryDirectory() as directory:
            self.offline(d, before, Path(directory))
            with self.assertRaisesRegex(RuntimeError, '不重放'):
                self.offline(d, before, Path(directory))
        self.assertEqual(len(d.calls), 1)
    def test_stage_is_idempotent_and_rejects_sealed_kit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = "import importlib\nclass Deploy:\n    def idle(self,wait=False):\n        return 'old'\n    def offline(self,op,rows):\n        pass\n    def process_rows(self):\n        pass\n    def activate(self):\n        raise RuntimeError('predecessor')\n"
            (root / 'deploy.py').write_text(source)
            (root / 'upgrade_backup.py').write_text("LEGACY = ['config.json']\ndef names():return LEGACY\n")
            staging.stage(root)
            first = (root / 'deploy.py').read_bytes()
            backup_first=(root/'upgrade_backup.py').read_bytes()
            namespace={};exec(backup_first,namespace)
            self.assertEqual(namespace['names'](),['config.json','runtime/management-v2/owner-held-inputs.json'])
            staging.stage(root)
            self.assertEqual((root/'upgrade_backup.py').read_bytes(),backup_first)
            self.assertEqual(first, (root / 'deploy.py').read_bytes())
            self.assertIn(b"pause_upgrade_kit", first)
            (root / 'READY.json').write_text('{}')
            with self.assertRaisesRegex(RuntimeError, 'unsealed'): staging.stage(root)
    def test_stage_requires_both_original_methods(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'deploy.py').write_text('class Deploy:\n    def idle(self,wait=False):\n        pass\n')
            with self.assertRaisesRegex(RuntimeError, 'exactly'): staging.stage(root)

    def test_exact_installed_target_is_verified_noop_without_operations(self):
        with tempfile.TemporaryDirectory() as directory:
            image = Path(directory) / 'cutex'
            image.write_bytes(b'fixture image bytes, never executed')
            calls = []
            d = types.SimpleNamespace(plan={'release':'r290','commit':'target',
                'binaryTargets':{'cutex':[str(image)]},'files':{'payload/cutex':kit.hashlib.sha256(image.read_bytes()).hexdigest()}},
                identity=lambda:{'cutexBuild':{'release':'r290','commit':'target'}},
                integrity=lambda:calls.append('integrity'),service_health=lambda:calls.append('health'))
            messages = []
            self.assertTrue(kit.already_target(d, out=lambda text, **kw:messages.append(text)))
            self.assertEqual(calls, ['integrity','health'])
            self.assertEqual(messages, ['已经是 r290，无需升级'])
            d.identity = lambda:{'cutexBuild':{'release':'r290','commit':'other'}}
            self.assertFalse(kit.already_target(d))
            self.assertEqual(calls, ['integrity','health'])
            d.identity = lambda:{'cutexBuild':{'release':'r290','commit':'target'}}
            image.write_bytes(b'changed')
            with self.assertRaisesRegex(RuntimeError, '不匹配'):kit.already_target(d)

    def test_linux_foreign_user_does_not_block_this_home_but_known_writer_remains(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory) / 'owner/.cutex'
            foreign = str(Path(directory) / 'foreign/.cutex')
            rows = [
                {'pid':1,'name':'cutex','image':None,'ownerUid':1001},
                {'pid':2,'name':'cutex','ownerUid':1001,'cutexHome':foreign},
                {'pid':3,'name':'cutex','ownerUid':1000,'cutexHome':str(home)},
                {'pid':4,'name':'cutex','ownerUid':1000,'cutexHome':foreign},
                {'pid':5,'name':'cutex','ownerUid':1000},
                {'pid':6,'name':'cutex','ownerUid':0,'cutexHome':str(home)},
                {'pid':7,'name':'cutex','image':None},
            ]
            self.assertEqual([row['pid'] for row in scope.select_rows(rows,home,1000)], [3,5,6,7])

    @unittest.skipUnless(os.name == 'posix' and Path('/proc').is_dir(), 'Linux /proc ownership proof')
    def test_foreign_process_uid_is_read_from_proc_before_unreadable_cutex_is_excluded(self):
        # A real foreign-user process supplies uid/birth evidence. Only the
        # unreadable-Cutex presentation is a fixture; nothing is signalled.
        foreign = None
        for entry in Path('/proc').iterdir():
            if not entry.name.isdigit(): continue
            try:
                uids = next(line.split()[1:] for line in (entry / 'status').read_text().splitlines() if line.startswith('Uid:'))
                uid, filesystem_uid = int(uids[1]), int(uids[3])
                fields = (entry / 'stat').read_text().rsplit(')', 1)[1].split()
                if uid != os.getuid() and filesystem_uid != os.getuid():
                    foreign = {'pid':int(entry.name),'ppid':int(fields[1]),'born':int(fields[19]),
                               'name':'cutex','image':None}
                    break
            except (OSError, ValueError, IndexError, StopIteration): continue
        if foreign is None: self.skipTest('no readable foreign-user process on this host')
        with tempfile.TemporaryDirectory() as directory:
            d = types.SimpleNamespace(home=Path(directory))
            adapter = types.SimpleNamespace(process_rows=lambda deploy:[foreign])
            with mock.patch.dict(sys.modules, {'kit_activation_preflight':adapter}):
                self.assertEqual(scope.process_rows(d), [])
            self.assertNotEqual(foreign['ownerUid'], Path(directory).stat().st_uid)

    @unittest.skipUnless(os.name == 'posix' and Path('/proc').is_dir(), 'Linux /proc ownership proof')
    def test_own_nondumpable_process_is_not_mistaken_for_a_foreign_root_writer(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            env = dict(os.environ, CUTEX_HOME=str(home))
            child = subprocess.Popen([sys.executable, '-c',
                'import ctypes,time; assert ctypes.CDLL(None).prctl(4,0,0,0,0)==0; print("ready",flush=True); time.sleep(30)'],
                env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
                self.assertEqual(child.stdout.readline().strip(), 'ready')
                entry = Path('/proc') / str(child.pid)
                fields = (entry / 'stat').read_text().rsplit(')',1)[1].split()
                row = {'pid':child.pid,'ppid':os.getpid(),'born':int(fields[19]),'name':'cutex','image':None}
                adapter = types.SimpleNamespace(process_rows=lambda deploy:[row])
                with mock.patch.dict(sys.modules, {'kit_activation_preflight':adapter}):
                    self.assertEqual(scope.process_rows(types.SimpleNamespace(home=home)), [row])
                self.assertEqual(row['ownerUid'], os.geteuid())
            finally:
                if child.poll() is None: child.terminate()
                child.wait(timeout=10)
                child.stdout.close(); child.stderr.close()


if __name__ == '__main__': unittest.main()
