"""Fixed private-copy catalog projection; no source DB, history or auth access.

Invoked only by the Linux Human maintenance boundary, with an isolated Python
interpreter and bounded stdin. No arbitrary SQL, imports, script paths or hooks.
The Rust caller fences the exact database/WAL identities before and after.
"""
import json
import sqlite3
import sys
from urllib.parse import quote

try:
    request = json.loads(sys.stdin.buffer.read(8193))
    if set(request) != {'path', 'native_id', 'wal_bytes'}:
        raise ValueError('invalid catalog request')
    # Only exclusive disposable copies: SQLite builds its own SHM, validates WAL
    # checksums and resolves committed transactions. Never copy source SHM.
    connection = sqlite3.connect('file:' + quote(request['path'], safe='/') + '?mode=rw',
                                 uri=True, timeout=0.2)
    connection.execute('PRAGMA trusted_schema=OFF')
    if request['wal_bytes']:
        page_size = connection.execute('PRAGMA page_size').fetchone()[0]
        busy, frames, done = connection.execute('PRAGMA wal_checkpoint(PASSIVE)').fetchone()
        # Conservative: SQLite must recognize every frame of this captured WAL.
        # Stale tails/uncommitted suffixes are explicitly unsupported rather
        # than silently treating discarded bytes as absent committed metadata.
        if busy or frames <= 0 or done != frames or request['wal_bytes'] != 32 + frames * (24 + page_size):
            raise ValueError('WAL not fully recognized; corrupt or unsupported trailing frames')
    connection.execute('PRAGMA query_only=ON')
    if connection.execute('PRAGMA quick_check').fetchall() != [('ok',)]:
        raise ValueError('catalog consistency check failed')
    row = connection.execute(
        'SELECT id, rollout_path, memory_mode, history_mode FROM threads WHERE id=?',
        (request['native_id'],)).fetchall()
    connection.close()
    if len(row) != 1:
        raise ValueError('catalog identity missing or ambiguous')
    result = dict(zip(('native_id', 'rollout_path', 'memory_mode', 'history_mode'), row[0]))
    output = json.dumps(result).encode()
    if len(output) > 8192:
        raise ValueError('catalog projection exceeds bound')
    sys.stdout.buffer.write(output)
except Exception as error:
    # Never retain SQL diagnostics, environment or database contents.
    sys.stderr.write('native catalog read failed: ' + type(error).__name__ + '\n')
    sys.exit(1)
