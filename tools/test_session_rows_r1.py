import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('s7_r1', Path(__file__).with_name('session_rows_r1.py'))
r1 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(r1)


class ExportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='s7-r1-', dir=os.environ.get('TEMP'))
        self.root = Path(self.temp.name)

    def tearDown(self):
        self.temp.cleanup()

    def database(self, store):
        db = sqlite3.connect(self.root / (store + '.sqlite3'))
        db.executescript('''PRAGMA journal_mode=WAL;
            CREATE TABLE session_rows(id TEXT PRIMARY KEY,deleted INTEGER NOT NULL,data BLOB);
            CREATE TABLE session_rows_ready(id INTEGER PRIMARY KEY,source TEXT);
            INSERT INTO session_rows_ready VALUES(1,'fixture');''')
        return db

    def test_activity_exports_current_wal_new_modified_deleted_and_unicode(self):
        store = 'session-activity'
        db = self.database(store)
        for key, revision in [('new汉🙂', 6), ('changed', 19)]:
            db.execute('INSERT INTO session_rows VALUES(?,0,?)', (key, json.dumps(dict(revision=revision, lastConversationAt='2026-11-04T01:02:03Z'))))
        db.execute("INSERT INTO session_rows VALUES('gone',1,NULL)")
        db.commit()
        output = self.root / (store + '-state.json')
        output.write_text('stale source')
        result = r1.export(self.root, store, output)
        value = json.loads(output.read_text(encoding='utf-8'))
        self.assertEqual(value['sessions']['changed']['revision'], 19)
        self.assertEqual(value['sessions']['new汉🙂']['revision'], 6)
        self.assertEqual(value['deletedSessions'], ['gone'])
        self.assertEqual((result['rows'], result['deleted']), (2, 1))
        db.close()

    def test_projection_preserves_focus_and_fingerprint(self):
        store = 'session-projection'
        db = self.database(store)
        row = dict(revision=18, fingerprint='new', focus=dict(revision=7, owner='mobile', mobileMuted=True, source='fixture', updatedAt='2026-11-04T01:02:03Z'))
        db.execute('INSERT INTO session_rows VALUES(?,0,?)', ('selected', json.dumps(row)))
        db.commit()
        output = self.root / 'out.json'
        r1.export(self.root, store, output)
        self.assertEqual(json.loads(output.read_text())['sessions']['selected'], row)
        self.assertNotIn('deletedSessions', json.loads(output.read_text()))
        db.close()

    def test_failure_preserves_previous_output_and_sql(self):
        for store, row in [('session-activity', dict(revision=9007199254740992)), ('session-projection', dict(revision=1, focus=dict(revision=0)))]:
            db = self.database(store)
            db.execute('INSERT INTO session_rows VALUES(?,0,?)', ('bad', json.dumps(row)))
            db.commit()
            output = self.root / 'out.json'
            output.write_text('unchanged')
            with self.assertRaises(ValueError):
                r1.export(self.root, store, output)
            self.assertEqual(output.read_text(), 'unchanged')
            self.assertEqual(db.execute('SELECT count(*) FROM session_rows').fetchone()[0], 1)
            db.close()

    def test_missing_or_unready_canonical_is_refused(self):
        output = self.root / 'out.json'
        with self.assertRaises(ValueError):
            r1.export(self.root, 'session-activity', output)
        db = self.database('session-activity')
        db.execute('DELETE FROM session_rows_ready')
        db.commit()
        with self.assertRaises(ValueError):
            r1.export(self.root, 'session-activity', output)
        self.assertFalse(output.exists())
        db.close()

    def test_unrepresentable_projection_fences_refuse_replacement(self):
        db = self.database('session-projection')
        db.execute("INSERT INTO session_rows VALUES('gone',1,NULL)")
        db.commit()
        output = self.root / 'out.json'
        with self.assertRaises(ValueError):
            r1.export(self.root, 'session-projection', output)
        self.assertFalse(output.exists())
        db.close()


if __name__ == '__main__':
    unittest.main()
