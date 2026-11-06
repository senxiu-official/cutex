"""Visible display-item order; payloads and cursor identities stay canonical.

This additive read model is maintained by plain SQL, including writes from an
explicitly selected older component. Only the writer migrates it, atomically
with its marker. Offline readers without a completed model keep the old query.
"""

MARKER = 'history_item_order'
VERSION = '1'
TRIGGERS = ('item_inserted', 'item_deleted', 'item_moved', 'turn_changed', 'turn_deleted')


def available(db):
    return db.execute('''SELECT 1 FROM meta WHERE key=? AND value=?
        AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='history_item_order')''',
                      (MARKER, VERSION)).fetchone() is not None


def ensure_schema(db):
    """Called inside Store's migration transaction, never by an offline reader."""
    if available(db):
        return
    # Also covers a not-yet-published/incomplete prototype. SQLite rolls the
    # entire migration back on failure; no page can observe a partial backfill.
    for suffix in TRIGGERS:
        db.execute('DROP TRIGGER IF EXISTS history_order_' + suffix)
    db.execute('DROP TABLE IF EXISTS history_item_order')
    db.execute('CREATE INDEX IF NOT EXISTS history_item_page ON items(turn_id,seq)')
    db.execute('''CREATE TABLE history_item_order(
        thread_id TEXT NOT NULL REFERENCES threads(id),
        seq INTEGER NOT NULL REFERENCES items(seq) ON DELETE CASCADE ON UPDATE CASCADE,
        PRIMARY KEY(thread_id,seq)) WITHOUT ROWID''')
    # The FK's child key is not the leading primary-key column. Without this
    # index SQLite would scan all visible keys on each canonical item delete.
    db.execute('CREATE UNIQUE INDEX history_item_order_seq ON history_item_order(seq)')
    db.execute('''INSERT INTO history_item_order
        SELECT t.thread_id,i.seq FROM turns t JOIN items i ON i.turn_id=t.id WHERE t.reverted=0''')
    db.execute('''CREATE TRIGGER history_order_item_inserted AFTER INSERT ON items BEGIN
        INSERT INTO history_item_order SELECT thread_id,NEW.seq FROM turns
        WHERE id=NEW.turn_id AND reverted=0; END''')
    db.execute('''CREATE TRIGGER history_order_item_deleted AFTER DELETE ON items BEGIN
        DELETE FROM history_item_order WHERE seq=OLD.seq; END''')
    db.execute('''CREATE TRIGGER history_order_item_moved AFTER UPDATE OF turn_id,seq ON items
        WHEN OLD.turn_id IS NOT NEW.turn_id OR OLD.seq IS NOT NEW.seq BEGIN
        DELETE FROM history_item_order WHERE seq IN (OLD.seq,NEW.seq);
        INSERT INTO history_item_order SELECT thread_id,NEW.seq FROM turns
        WHERE id=NEW.turn_id AND reverted=0; END''')
    db.execute('''CREATE TRIGGER history_order_turn_changed AFTER UPDATE OF thread_id,reverted ON turns
        WHEN OLD.thread_id IS NOT NEW.thread_id OR OLD.reverted IS NOT NEW.reverted BEGIN
        DELETE FROM history_item_order WHERE thread_id=OLD.thread_id AND seq IN
        (SELECT seq FROM items WHERE turn_id=NEW.id);
        INSERT INTO history_item_order SELECT NEW.thread_id,seq FROM items
        WHERE turn_id=NEW.id AND NEW.reverted=0; END''')
    db.execute('''CREATE TRIGGER history_order_turn_deleted AFTER DELETE ON turns BEGIN
        DELETE FROM history_item_order WHERE thread_id=OLD.thread_id AND seq IN
        (SELECT seq FROM items WHERE turn_id=OLD.id); END''')
    db.execute('INSERT OR REPLACE INTO meta VALUES(?,?)', (MARKER, VERSION))
