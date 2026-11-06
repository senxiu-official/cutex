"""Offline S7 -> exact r275 9cf4cbab v1 JSON. Stop every writer first.

Exports current SQL (never copies the stale import JSON). Each store is an
independent operation. Stages and validates before atomic replacement. Leaves
the current database intact; activation of a previous build is kit-owned.
"""
import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import tempfile


def export(root: Path, store: str, output: Path):
    if store not in ('session-activity', 'session-projection'):
        raise ValueError('unknown S7 store')
    source = root / (store + '.sqlite3')
    if not source.is_file():
        raise ValueError('canonical SQL is missing')
    with closing(sqlite3.connect(source.resolve().as_uri() + '?mode=ro', uri=True)) as db:
        db.execute('BEGIN')
        if db.execute('PRAGMA quick_check').fetchone() != ('ok',):
            raise ValueError('SQL integrity check failed')
        if not db.execute('SELECT 1 FROM session_rows_ready WHERE id=1').fetchone():
            raise ValueError('SQL import is not ready')
        output.parent.mkdir(parents=True, exist_ok=True)
        fd, staging = tempfile.mkstemp(prefix=output.name + '.r1-', dir=output.parent)
        try:
            count = deleted = 0
            digest = hashlib.sha256()
            with os.fdopen(fd, 'w', encoding='utf-8', newline='\n') as f:
                f.write('{"version":1,"sessions":{\n')
                for key, raw in db.execute('SELECT id,data FROM session_rows WHERE deleted=0 ORDER BY id'):
                    value = json.loads(raw)
                    revision = value.get('revision')
                    if not key or not isinstance(revision, int) or not 0 <= revision <= 9007199254740991:
                        raise ValueError('invalid session row revision')
                    if store == 'session-projection' and not 1 <= value['focus']['revision'] <= 9007199254740991:
                        raise ValueError('invalid focus revision')
                    encoded = json.dumps(value, ensure_ascii=False, separators=(',', ':'), sort_keys=True)
                    digest.update(json.dumps(key).encode() + encoded.encode())
                    if count:
                        f.write(',')
                    f.write(json.dumps(key) + ':' + encoded + '\n')
                    count += 1
                f.write('}\n')
                if store == 'session-activity':
                    f.write(',"deletedSessions":[\n')
                    for (key,) in db.execute('SELECT id FROM session_rows WHERE deleted=1 ORDER BY id'):
                        if deleted:
                            f.write(',')
                        f.write(json.dumps(key) + '\n')
                        digest.update(b'deleted:' + key.encode())
                        deleted += 1
                    f.write(']\n')
                elif db.execute('SELECT 1 FROM session_rows WHERE deleted=1 LIMIT 1').fetchone():
                    raise ValueError('previous projection format cannot represent deletion fences')
                f.write('}\n')
                f.flush()
                os.fsync(f.fileno())
            # Independent streamed validation through the same SQL rows avoids
            # loading the full output document. The test harness additionally
            # gives this exact file to the pinned previous reader/writer.
            expected = db.execute('SELECT count(*) FROM session_rows WHERE deleted=0').fetchone()[0]
            if count != expected:
                raise ValueError('R1 row count mismatch')
            verify_export(Path(staging), db, store, count, deleted, digest.hexdigest())
            os.replace(staging, output)
            return dict(store=store, rows=count, deleted=deleted, digest=digest.hexdigest(), output=str(output))
        except BaseException:
            Path(staging).unlink(missing_ok=True)
            raise


def verify_export(path, db, store, expected_rows, expected_deleted, expected_digest):
    count = deleted = 0
    digest = hashlib.sha256()
    with path.open(encoding='utf-8') as f:
        if f.readline() != '{"version":1,"sessions":{\n':
            raise ValueError('invalid R1 header')
        for line in f:
            if line == '}\n':
                break
            record = json.loads('{' + line.lstrip(',').strip() + '}')
            if len(record) != 1:
                raise ValueError('invalid R1 row')
            key, value = next(iter(record.items()))
            sql = db.execute('SELECT data FROM session_rows WHERE id=? AND deleted=0', (key,)).fetchone()
            if sql is None or value != json.loads(sql[0]):
                raise ValueError('R1 body/revision/focus mismatch')
            encoded = json.dumps(value, ensure_ascii=False, separators=(',', ':'), sort_keys=True)
            digest.update(json.dumps(key).encode() + encoded.encode())
            count += 1
        if store == 'session-activity':
            if f.readline() != ',"deletedSessions":[\n':
                raise ValueError('missing deletion fences')
            for line in f:
                if line == ']\n':
                    break
                key = json.loads(line.lstrip(','))
                if not db.execute('SELECT 1 FROM session_rows WHERE id=? AND deleted=1', (key,)).fetchone():
                    raise ValueError('R1 deletion mismatch')
                digest.update(b'deleted:' + key.encode())
                deleted += 1
        if f.read() != '}\n':
            raise ValueError('invalid R1 trailer')
    if (count, deleted, digest.hexdigest()) != (expected_rows, expected_deleted, expected_digest):
        raise ValueError('R1 exported count/digest mismatch')


if __name__ == '__main__':
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--root', type=Path, required=True, help='stopped copy runtime/management-v2')
    p.add_argument('--store', choices=['session-activity', 'session-projection'], required=True)
    p.add_argument('--output', type=Path, required=True)
    args = p.parse_args()
    print(json.dumps(export(args.root, args.store, args.output)))
