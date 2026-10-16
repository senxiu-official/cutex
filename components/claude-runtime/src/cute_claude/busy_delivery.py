"""Durable, at-most-once external input at an authenticated tool boundary.

An untouched `soon` input remains in the ordinary FIFO. Taking it for a hook
atomically removes it from that FIFO and associates it with the active turn.
A crash between claiming and SDK acknowledgement is uncertain, never replayed.
"""
from __future__ import annotations

import json


class BusyDelivery:
    def __init__(self, server):
        self.server, self.store = server, server.store
        self.store.db.executescript('''
            CREATE TABLE IF NOT EXISTS busy_inputs(
                thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
                message_id TEXT NOT NULL, reserved_turn_id TEXT NOT NULL,
                context_turn_id TEXT, offered INTEGER NOT NULL DEFAULT 0, persisted INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(thread_id,message_id));
            CREATE INDEX IF NOT EXISTS busy_inputs_turn ON busy_inputs(thread_id,context_turn_id);
        ''')
        # No receipt can be inferred from a previous process offering a hook.
        self.store.db.execute("UPDATE inputs SET state='uncertain' WHERE state='injecting'")

    @property
    def enabled(self):
        return self.server.settings.backend == 'subscription'

    def register(self, tid, mid, receipt):
        self.store.db.execute('INSERT OR IGNORE INTO busy_inputs(thread_id,message_id,reserved_turn_id) VALUES(?,?,?)',
                              (tid, mid, receipt['turnId']))

    def receipt(self, receipt):
        row = self.store.db.execute('SELECT * FROM busy_inputs WHERE thread_id=? AND message_id=?',
                                    (receipt['threadId'], receipt['messageId'])).fetchone()
        if row is None:
            return receipt
        context = row['context_turn_id'] or receipt['turnId']
        turn = self.store.db.execute('SELECT reverted FROM turns WHERE id=?', (context,)).fetchone()
        if turn and turn[0]:
            return {**receipt, 'deliveryMode': 'soon', 'contextPersisted': False,
                    'contextTurnId': context, 'state': 'reverted'}
        return {**receipt, 'deliveryMode': 'soon', 'contextPersisted': bool(row['persisted']),
                'contextTurnId': context if row['context_turn_id'] or row['persisted'] else None}

    def inject(self, tid, turn_id):
        taken = []
        with self.store.transaction():
            active = self.store.db.execute("SELECT 1 FROM turns WHERE thread_id=? AND id=? AND state='inProgress'",
                                            (tid, turn_id)).fetchone()
            thread = self.store.thread(tid)
            if not active or thread['halted'] or thread['archived'] or tid in self.store.history_mutations:
                return ''
            rows = self.store.db.execute('''SELECT i.* FROM inputs i JOIN busy_inputs b
                ON b.thread_id=i.thread_id AND b.message_id=i.message_id
                WHERE i.thread_id=? AND i.state='queued' ORDER BY i.seq''', (tid,)).fetchall()
            for row in rows:
                payload = json.loads(row['payload'])
                # External provenance and ask metadata remain unchanged.
                item = self.store.external_item(row['message_id'], payload)
                self.store.put_item(turn_id, item)
                self.store.db.execute("UPDATE inputs SET state='injecting' WHERE seq=?", (row['seq'],))
                self.store.db.execute('UPDATE busy_inputs SET context_turn_id=? WHERE thread_id=? AND message_id=?',
                                      (turn_id, tid, row['message_id']))
                taken.append((item, self.store.project_input(payload)[0]['text']))
        # No await after the claim: the hook records offered and returns in the
        # same event-loop step. Publish timeline notifications at acknowledgement
        # so a terminal callback cannot overtake a not-yet-returned hook body.
        return '\n\n'.join(frame for _, frame in taken)

    async def acknowledge(self, tid, turn_id):
        """Called only when the SDK successfully completes the consuming turn.

        Hook return and ordinary output do not prove persisted native context.
        The r8 successful terminal acknowledgement remains the receipt boundary.
        """
        rows = self.store.db.execute('''SELECT i.message_id FROM inputs i JOIN busy_inputs b
            ON b.thread_id=i.thread_id AND b.message_id=i.message_id
            WHERE i.thread_id=? AND b.context_turn_id=? AND i.state='injecting' AND b.persisted=0 AND b.offered=1''', (tid, turn_id)).fetchall()
        items = {item['id']: item for item in self.store.items(turn_id)} if rows else {}
        for row in rows:
            item = items[row['message_id']]
            await self.server.engine.emit(tid, 'item/started', {'threadId': tid, 'turnId': turn_id, 'item': item})
            await self.server.engine.emit(tid, 'item/completed', {'threadId': tid, 'turnId': turn_id, 'item': item})
            await self.server.engine.emit(tid, 'claude/input/contextPersisted',
                {'threadId': tid, 'turnId': turn_id, 'messageId': row['message_id']})

    def offered(self, tid, turn_id):
        # Synchronous final step of the authenticated hook: no await between
        # recording the offer and returning its additionalContext to the SDK.
        self.store.db.execute('UPDATE busy_inputs SET offered=1 WHERE thread_id=? AND context_turn_id=?', (tid, turn_id))

    def observe(self, tid, method, params):
        if method == 'claude/input/contextPersisted':
            mid, turn = params.get('messageId'), params.get('turnId')
            with self.store.transaction():
                self.store.db.execute('''UPDATE busy_inputs SET persisted=1 WHERE thread_id=? AND message_id=?
                    AND ((context_turn_id=? AND offered=1) OR EXISTS(SELECT 1 FROM inputs i WHERE i.thread_id=? AND i.message_id=? AND i.turn_id=?))''',
                    (tid, mid, turn, tid, mid, turn))
        if method == 'turn/completed':
            turn = params.get('turn') or {}
            self.store.db.execute('''UPDATE inputs SET state=? WHERE thread_id=? AND state='injecting'
                AND message_id IN (SELECT message_id FROM busy_inputs WHERE thread_id=? AND context_turn_id=?)''',
                (turn.get('status', 'uncertain'), tid, tid, turn.get('id')))
        # finish() makes all consumed inputs terminal; their durable persisted
        # bit distinguishes success from unknown/failed context on reconciliation.
