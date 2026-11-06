"""Committed per-turn and per-thread usage totals (design 058 S2, claude-r36).

`turns.usage` stays the canonical record (`modelRequests`, unchanged format,
readable by the previous component). These tables are a rebuildable read model:
a turn's row holds the sums of its own `modelRequests`, the thread row holds
the sum of its turn rows. A write replaces the turn's sums and moves the thread
row by the difference, so `thread/tokenUsage/updated` no longer re-reads every
historical turn. Repeating the same record changes nothing (no double count);
a correction moves the totals by exactly its difference.

Numbers equal the former full re-sum: every turn of the thread counts,
reverted turns included (billing, not occupancy), forked copies carry no
`modelRequests`. Any write that bypasses `write_turn_usage` (an older component
after an explicit repin, a fork insert, a delete) marks the thread stale through
SQLite triggers, and the next read rebuilds that one thread once.
"""
from __future__ import annotations

import json
from typing import NamedTuple

# `Engine._totals` order: plain input, cache write, cache read, output.
KEYS = ('input_tokens', 'cache_creation_input_tokens', 'cache_read_input_tokens', 'output_tokens')
COLUMNS = ('input', 'cache_write', 'cache_read', 'output',
           'bg_input', 'bg_cache_write', 'bg_cache_read', 'bg_output')
ZERO = (0,) * len(COLUMNS)


class ThreadTotals(NamedTuple):
    sums: tuple  # COLUMNS order: all requests, then background-task requests
    revision: int


def ensure_schema(db) -> None:
    """Additive tables and triggers; the `meta` schema version stays 2."""
    counters = ', '.join(f'{c} INTEGER NOT NULL DEFAULT 0' for c in COLUMNS)
    db.execute(f'''CREATE TABLE IF NOT EXISTS usage_turn_totals(
        turn_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, revision INTEGER NOT NULL, {counters})''')
    db.execute('CREATE INDEX IF NOT EXISTS usage_turn_totals_thread ON usage_turn_totals(thread_id)')
    db.execute(f'''CREATE TABLE IF NOT EXISTS usage_thread_totals(
        thread_id TEXT PRIMARY KEY, revision INTEGER NOT NULL, {counters})''')
    db.execute('CREATE TABLE IF NOT EXISTS usage_totals_stale(thread_id TEXT PRIMARY KEY)')
    # Plain SQL only: these also run inside a previous component's writes.
    db.execute('''CREATE TRIGGER IF NOT EXISTS usage_totals_turn_updated AFTER UPDATE OF usage, thread_id ON turns BEGIN
        INSERT OR IGNORE INTO usage_totals_stale VALUES(NEW.thread_id);
        INSERT OR IGNORE INTO usage_totals_stale VALUES(OLD.thread_id); END''')
    db.execute('''CREATE TRIGGER IF NOT EXISTS usage_totals_turn_inserted AFTER INSERT ON turns WHEN NEW.usage <> '{}' BEGIN
        INSERT OR IGNORE INTO usage_totals_stale VALUES(NEW.thread_id); END''')
    db.execute('''CREATE TRIGGER IF NOT EXISTS usage_totals_turn_deleted AFTER DELETE ON turns BEGIN
        INSERT OR IGNORE INTO usage_totals_stale VALUES(OLD.thread_id); END''')


def request_sums(calls) -> tuple:
    """Sums of one turn's `modelRequests`, exactly as `Engine._totals` adds them."""
    if not isinstance(calls, list):
        return ZERO
    usage = [c['usage'] for c in calls if isinstance(c, dict) and isinstance(c.get('usage'), dict)]
    background = [c['usage'] for c in calls
                  if isinstance(c, dict) and c.get('kind') == 'background-task' and isinstance(c.get('usage'), dict)]
    return (*(sum(u.get(k, 0) for u in usage) for k in KEYS),
            *(sum(u.get(k, 0) for u in background) for k in KEYS))


def _record_sums(text) -> tuple:
    try:
        record = json.loads(text)
    except (TypeError, ValueError):
        return ZERO
    return request_sums(record.get('modelRequests', [])) if isinstance(record, dict) else ZERO


def _stale(db, tid: str) -> bool:
    return (db.execute('SELECT 1 FROM usage_totals_stale WHERE thread_id=?', (tid,)).fetchone() is not None
            or db.execute('SELECT 1 FROM usage_thread_totals WHERE thread_id=?', (tid,)).fetchone() is None)


def _rebuild(db, tid: str) -> None:
    """One thread's first build (or after an outside write): O(its turns), once."""
    previous = db.execute('SELECT revision FROM usage_thread_totals WHERE thread_id=?', (tid,)).fetchone()
    db.execute('DELETE FROM usage_turn_totals WHERE thread_id=?', (tid,))
    total = [0] * len(COLUMNS)
    for turn_id, text in db.execute('SELECT id, usage FROM turns WHERE thread_id=?', (tid,)).fetchall():
        sums = _record_sums(text)
        if sums != ZERO:
            _put_turn(db, turn_id, tid, 1, sums)
            total = [a + b for a, b in zip(total, sums)]
    _put_thread(db, tid, (previous[0] + 1) if previous else 1, tuple(total))
    db.execute('DELETE FROM usage_totals_stale WHERE thread_id=?', (tid,))


def _put_turn(db, turn_id, tid, revision, sums) -> None:
    db.execute(f'INSERT OR REPLACE INTO usage_turn_totals(turn_id, thread_id, revision, {", ".join(COLUMNS)}) '
               f'VALUES(?,?,?{",?" * len(COLUMNS)})', (turn_id, tid, revision, *sums))


def _put_thread(db, tid, revision, sums) -> None:
    db.execute(f'INSERT OR REPLACE INTO usage_thread_totals(thread_id, revision, {", ".join(COLUMNS)}) '
               f'VALUES(?,?{",?" * len(COLUMNS)})', (tid, revision, *sums))


def write_turn_usage(store, turn_id: str, record: dict) -> None:
    """Replace one turn's usage record and move the totals by its difference."""
    from .store import dump
    db = store.db
    with store.transaction():
        row = db.execute('SELECT thread_id FROM turns WHERE id=?', (turn_id,)).fetchone()
        if row is None:
            return
        tid = row[0]
        stale = _stale(db, tid)  # Before the update: its own trigger marks the thread too.
        db.execute('UPDATE turns SET usage=? WHERE id=?', (dump(record), turn_id))
        if stale:
            _rebuild(db, tid)
            return
        db.execute('DELETE FROM usage_totals_stale WHERE thread_id=?', (tid,))
        sums = request_sums(record.get('modelRequests', []))
        old = db.execute(f'SELECT revision, {", ".join(COLUMNS)} FROM usage_turn_totals WHERE turn_id=?', (turn_id,)).fetchone()
        before = tuple(old[1:]) if old else ZERO
        if sums == before:
            return
        _put_turn(db, turn_id, tid, (old[0] + 1) if old else 1, sums)
        current = db.execute(f'SELECT revision, {", ".join(COLUMNS)} FROM usage_thread_totals WHERE thread_id=?', (tid,)).fetchone()
        _put_thread(db, tid, current[0] + 1, tuple(t + s - b for t, s, b in zip(current[1:], sums, before)))


def thread_totals(store, tid: str) -> ThreadTotals:
    """Committed thread totals: a point read, after at most one rebuild."""
    db = store.db
    if _stale(db, tid):
        with store.transaction():
            if _stale(db, tid):
                _rebuild(db, tid)
    row = db.execute(f'SELECT revision, {", ".join(COLUMNS)} FROM usage_thread_totals WHERE thread_id=?', (tid,)).fetchone()
    return ThreadTotals(tuple(row[1:]), row[0])
