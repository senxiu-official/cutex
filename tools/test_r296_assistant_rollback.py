import ast
import contextlib
import importlib
import io
import json
import os
from pathlib import Path
import shutil
import sqlite3
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import r296_assistant_rollback as preflight
import stage_r296_assistant_rollback as staging


class RollbackTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.home = Path(scratch.name)

    def database(self, index, version):
        file = self.home / "runtime" / preflight.STORES[index][0]
        file.parent.mkdir(parents=True, exist_ok=True)
        with contextlib.closing(sqlite3.connect(file)) as db:
            db.execute(f"PRAGMA user_version={version}")
        return file

    def test_missing_and_predecessor_versions_are_quiet(self):
        self.assertEqual(preflight.warn(self.home, io.StringIO()), [])
        self.assertEqual(list(self.home.iterdir()), [])
        self.database(0, 3)
        self.database(1, 1)
        self.assertEqual(preflight.warn(self.home, io.StringIO()), [])

    def test_detects_each_store_independently_and_names_ordinary_schedules(self):
        self.database(1, 2)
        out = io.StringIO()
        self.assertEqual(len(preflight.warn(self.home, out)), 1)
        self.assertIn("普通所有者定时任务", out.getvalue())
        self.database(0, 4)
        self.assertEqual(len(preflight.warn(self.home, out)), 2)
        self.assertIn("Director 收件箱", out.getvalue())
        self.database(1, 1)
        self.assertEqual(len(preflight.warn(self.home, out)), 1)

    def test_reads_committed_wal_and_excludes_uncommitted_version(self):
        file = self.database(1, 1)
        with contextlib.closing(sqlite3.connect(file)) as writer:
            writer.execute("PRAGMA journal_mode=WAL")
            writer.execute("PRAGMA wal_autocheckpoint=0")
            writer.execute("BEGIN")
            writer.execute("PRAGMA user_version=2")
            self.assertEqual(preflight.warn(self.home, io.StringIO()), [])
            writer.commit()
            self.assertEqual(len(preflight.warn(self.home, io.StringIO())), 1)

    def test_unreadable_version_never_reports_a_clean_check(self):
        self.database(0, 3).write_bytes(b"invalid database")
        with self.assertRaisesRegex(RuntimeError, "无法核对个人助理"):
            preflight.warn(self.home, io.StringIO())

    def test_closed_wal_database_creates_no_sidecars(self):
        file = self.database(1, 2)
        with contextlib.closing(sqlite3.connect(file)) as writer:
            writer.execute("PRAGMA journal_mode=WAL")
        before = {p.name:p.read_bytes() for p in file.parent.iterdir()}
        self.assertEqual(len(preflight.warn(self.home, io.StringIO())),1)
        self.assertEqual({p.name:p.read_bytes() for p in file.parent.iterdir()}, before)

    def test_restoring_consistent_predecessor_snapshots_clears_each_warning(self):
        files = [self.database(0,3), self.database(1,1)]
        for file in files:
            with contextlib.closing(sqlite3.connect(file)) as current, contextlib.closing(sqlite3.connect(file.with_suffix(".backup"))) as old:
                current.execute("CREATE TABLE example(id TEXT)")
                current.execute("INSERT INTO example VALUES('before')")
                current.commit()
                current.backup(old)
                current.execute("UPDATE example SET id='after'")
                current.execute("PRAGMA user_version=4")
                current.commit()
        self.assertEqual(len(preflight.warn(self.home, io.StringIO())),2)
        for file in files:
            with contextlib.closing(sqlite3.connect(file)) as current, contextlib.closing(sqlite3.connect(file.with_suffix(".backup"))) as old:
                old.backup(current)
                self.assertEqual(current.execute("SELECT id FROM example").fetchall(),[("before",)])
        self.assertEqual(preflight.warn(self.home, io.StringIO()),[])

    def test_kit_calls_preflight_before_mutation_and_on_rollback_recovery(self):
        driver = self.home / "deploy.py"
        driver.write_text("""import importlib
class Deploy:
    def rollback(self,act):
        importlib.import_module('r290_native_migration_kit').rollback_preflight(self,act)
        self.stop()
    def recover(self,op,r):
        importlib.import_module('r290_native_migration_kit').recover_preflight(self,op,r)
        self.stop()
""", encoding="utf-8")
        staging.stage(self.home)
        content = driver.read_bytes()
        staging.stage(self.home)
        self.assertEqual(driver.read_bytes(), content)
        self.assertTrue((self.home / "R296-ASSISTANT-RESTORE.zh-CN.md").is_file())
        events = []
        old = SimpleNamespace(rollback_preflight=lambda *a:events.append("prior"),
                              recover_preflight=lambda *a:events.append("prior"))
        module = {}
        exec(compile(content, str(driver), "exec"), module)
        d = module["Deploy"]()
        d.plan, d.home, d.stop = {"release":"r296"}, self.home, lambda:events.append("stop")
        with patch.dict("sys.modules", {"r290_native_migration_kit":old}), patch.object(preflight,"warn",side_effect=lambda _:events.append("version")):
            d.rollback(None)
            d.recover(None, {"kind":"rollback"})
            d.recover(None, {"kind":"activate"})
            d.plan = {"release":"r295"}
            d.rollback(None)
        self.assertEqual(events, ["version","prior","stop"]*2 + ["prior","stop"]*2)
        (self.home / "READY.json").write_text("{}")
        with self.assertRaisesRegex(RuntimeError,"sealed"):
            staging.stage(self.home)

    @unittest.skipUnless(os.environ.get("CUTEX_R296_KIT_TOOL_SOURCE"), "supply the predecessor kit's deploy.py")
    def test_predecessor_controller_warns_before_any_offline_step(self):
        track = self.home / "track"
        track.mkdir()
        shutil.copyfile(Path(os.environ["CUTEX_R296_KIT_TOOL_SOURCE"]) / "deploy.py", track / "deploy.py")
        (track / "shared").mkdir()
        shutil.copyfile(Path(os.environ["CUTEX_R296_KIT_TOOL_SOURCE"]) / "shared/sqlite_snapshot.py", track / "shared/sqlite_snapshot.py")
        staging.stage(track)
        first = (track / "deploy.py").read_bytes()
        staging.stage(track)
        self.assertEqual(first, (track / "deploy.py").read_bytes())
        deploy = next(n for n in ast.parse(first).body if isinstance(n, ast.ClassDef) and n.name=="Deploy")
        methods = [n for n in deploy.body if getattr(n,"name",None) in ("rollback","recover")]
        self.assertEqual(len(methods), 2)
        scope = dict(importlib=importlib, os=SimpleNamespace(name="nt"), require=lambda ok,msg:self.assertTrue(ok,msg),
                     private_dir=lambda *_:None, load=lambda p:json.loads(p.read_text()), Journal=lambda *_:None)
        exec(compile(ast.Module(body=methods,type_ignores=[]), "staged-deploy", "exec"),scope)
        op = self.home / "operation"
        op.mkdir()
        (op / "operation.json").write_text('{"kind":"rollback","phase":"rollback-stopping"}')
        d = SimpleNamespace(home=self.home, root=track, plan={"release":"r296"}, token="fixture",
                            integrity=lambda:None, activation=lambda:op, latest=lambda:op)
        self.database(0,4)
        self.database(1,2)
        class ReachedNextCheck(Exception):
            pass
        def next_check(*_):
            raise ReachedNextCheck()
        existing = SimpleNamespace(rollback_preflight=next_check, recover_preflight=next_check)
        spec = importlib.util.spec_from_file_location("r296_assistant_rollback",track / "r296_assistant_rollback.py")
        staged = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(staged)
        with contextlib.closing(sqlite3.connect(self.home / "runtime" / preflight.STORES[1][0])) as writer:
            writer.execute("PRAGMA journal_mode=WAL")
            writer.execute("PRAGMA user_version=2")
            for name in ("rollback","recover"):
                out = io.StringIO()
                with patch.dict("sys.modules",{"r290_native_migration_kit":existing,"r296_assistant_rollback":staged}), contextlib.redirect_stderr(out), self.assertRaises(ReachedNextCheck):
                    scope[name](d)
                self.assertIn("Director 收件箱",out.getvalue())
                self.assertIn("普通所有者定时任务",out.getvalue())


if __name__ == "__main__":
    unittest.main()
