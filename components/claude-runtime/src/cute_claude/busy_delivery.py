"""Durable, at-most-once external input at an authenticated tool boundary.

An untouched `soon` input remains in the ordinary FIFO. Taking it for a hook
atomically removes it from that FIFO and associates it with the active turn.
A crash between claiming and SDK acknowledgement is uncertain, never replayed.
"""
from __future__ import annotations

import asyncio
import json
from .store import identifier


STEER_FRAME = ('The user sent this message while you were working. Read it now and take it into '
               'account before you continue:\n')


def steer_frame(text: str) -> str:
    return STEER_FRAME + text


def steer_item(mid: str, payload: dict) -> dict:
    """The timeline `userMessage` of a steered owner input (same shape as a
    turn's first user message, keyed by its inbox message id)."""
    parts = [{'type': 'text', 'text': payload['text'], 'text_elements': []}] if payload['text'] else []
    return {'type': 'userMessage', 'id': mid, 'clientId': payload.get('clientId'),
            'content': parts + list(payload.get('display') or []), 'steered': True}


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
            CREATE TABLE IF NOT EXISTS busy_stage_publications(
                event_seq INTEGER PRIMARY KEY REFERENCES events(seq) ON DELETE CASCADE,
                token TEXT NOT NULL);
        ''')
        if 'stage' not in {row[1] for row in self.store.db.execute('PRAGMA table_info(busy_inputs)')}:
            self.store.db.execute('ALTER TABLE busy_inputs ADD COLUMN stage TEXT')
        self.stage_tasks = {}
        self.item_tasks = {}
        self.claimed = {}
        from .context_receipts import ContextReceipts
        self.context_receipts = ContextReceipts(server, self.acknowledge_messages)
        # No receipt can be inferred from a previous process offering a hook.
        self.store.db.execute("UPDATE inputs SET state='uncertain' WHERE state='injecting'")

    @property
    def enabled(self):
        return self.server.settings.backend == 'subscription'

    def register(self, tid, mid, receipt, turn_id=None):
        self.store.db.execute('INSERT OR IGNORE INTO busy_inputs(thread_id,message_id,reserved_turn_id) VALUES(?,?,?)',
                              (tid, mid, receipt['turnId']))
        self.stage(tid, mid, turn_id or receipt['turnId'], 'registered')

    def client_id(self, tid, mid):
        row = self.store.db.execute('SELECT payload FROM inputs WHERE thread_id=? AND message_id=?', (tid, mid)).fetchone()
        payload = json.loads(row[0]) if row else {}
        value = payload.get('clientId')
        if isinstance(value, str) and value.strip():
            return value
        if payload.get('metadata', {}).get('nativeExternalInput') is True:
            return mid
        external = self.store.db.execute('SELECT 1 FROM external_inputs WHERE thread_id=? AND message_id=?', (tid, mid)).fetchone()
        return mid if external else None

    def stage(self, tid, mid, turn_id, stage):
        """Commit monotonic progress synchronously with the input/hook claim.

        Hook callbacks cannot yield between claiming input and returning context.
        Publish committed events afterwards, in the same per-thread order.
        """
        client_id = self.client_id(tid, mid)
        row = self.store.db.execute('SELECT stage FROM busy_inputs WHERE thread_id=? AND message_id=?', (tid, mid)).fetchone()
        ranks = {'registered': 0, 'offered': 1, 'contextPersisted': 2}
        if not client_id or row is None or ranks.get(row[0], -1) >= ranks[stage]:
            return
        self.store.db.execute('UPDATE busy_inputs SET stage=? WHERE thread_id=? AND message_id=?', (stage, tid, mid))
        params = {'threadId': tid, 'turnId': turn_id, 'messageId': mid, 'clientId': client_id, 'stage': stage}
        method = 'claude/input/stageChanged'
        seq = self.store.event(tid, method, params)
        token = identifier()
        self.store.db.execute('INSERT INTO busy_stage_publications(event_seq,token) VALUES(?,?)', (seq, token))
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            return  # The durable journal still supports replay without a live subscriber.
        previous = self.stage_tasks.get(tid)

        async def publish():
            if previous:
                await asyncio.gather(previous, return_exceptions=True)
            # A failed enclosing transaction must never publish a phantom stage.
            saved = self.store.db.execute('''SELECT e.method,e.params,p.token FROM events e
                JOIN busy_stage_publications p ON p.event_seq=e.seq WHERE e.seq=? AND e.thread_id=?''', (seq, tid)).fetchone()
            if saved and saved[2] == token and saved[0] == method and json.loads(saved[1]) == params:
                await self.server.engine.notify(tid, method, params)

        task = loop.create_task(publish())
        self.stage_tasks[tid] = task
        def finished(done):
            if self.stage_tasks.get(tid) is done:
                self.stage_tasks.pop(tid, None)
            if not done.cancelled():
                done.exception()
        task.add_done_callback(finished)

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
                if payload['source']['kind'] == 'user':
                    # Owner steer (`turn/steer`): a user message of this turn.
                    item, frame = steer_item(row['message_id'], payload), steer_frame(payload['text'])
                    self.store.put_item(turn_id, item)
                    self.store.db.execute("UPDATE inputs SET state='injecting' WHERE seq=?", (row['seq'],))
                    self.store.db.execute('UPDATE busy_inputs SET context_turn_id=? WHERE thread_id=? AND message_id=?',
                                          (turn_id, tid, row['message_id']))
                    # Reserved notes can only be consumed with their owner's hook.
                    carried = self.store.take_owner_carries(tid, row['message_id'], turn_id)
                    taken.extend(carried)
                    taken.append((item, frame))
                    continue
                # External provenance and ask metadata remain unchanged.
                item = self.store.external_item(row['message_id'], payload)
                self.store.put_item(turn_id, item)
                self.store.db.execute("UPDATE inputs SET state='injecting' WHERE seq=?", (row['seq'],))
                self.store.db.execute('UPDATE busy_inputs SET context_turn_id=? WHERE thread_id=? AND message_id=?',
                                      (turn_id, tid, row['message_id']))
                taken.append((item, self.store.project_input(payload)[0]['text']))
        # No await after the claim: the hook records offered and returns in the
        # same event-loop step. Every claimed item is published at once:
        # scheduled here, never awaited, so it runs right after the hook has
        # returned its context. That covers the owner's steered message (the
        # composer's pending row then reads "inserted") and, since claude-r35,
        # external wakes / notices, which r34 showed only at turn end. The item
        # is the claim's timeline projection, never a context receipt:
        # contextPersisted still follows separately (acknowledge_messages).
        self.claimed[(tid, turn_id)] = tuple(item['id'] for item, _ in taken)
        if taken:
            self._schedule_items(tid, turn_id, [item for item, _ in taken])
        return '\n\n'.join(frame for _, frame in taken)

    def _schedule_items(self, tid, turn_id, items):
        """Publish claimed items in claim order, after any earlier claim's items."""
        loop = asyncio.get_running_loop()
        previous = self.item_tasks.get(tid)

        async def publish():
            if previous:
                await asyncio.gather(previous, return_exceptions=True)
            for item in items:
                await self._publish(tid, turn_id, item)

        task = loop.create_task(publish())
        self.item_tasks[tid] = task

        def finished(done):
            if self.item_tasks.get(tid) is done:
                self.item_tasks.pop(tid, None)
            if not done.cancelled():
                done.exception()
        task.add_done_callback(finished)

    async def items_published(self, tid):
        """Wait until every item claimed so far on this thread has been published."""
        task = self.item_tasks.get(tid)
        if task:
            await asyncio.gather(task, return_exceptions=True)

    async def _publish(self, tid, turn_id, item):
        await self.server.engine.emit(tid, 'item/started', {'threadId': tid, 'turnId': turn_id, 'item': item})
        await self.server.engine.emit(tid, 'item/completed', {'threadId': tid, 'turnId': turn_id, 'item': item})

    def continue_quota_notes(self, tid, turn_id):
        """Carry quota-stopped agent notes into a fresh owner instruction.

        An offered hook is not a context receipt. Re-present only agent notes,
        with their original identities, so the new SDK result can acknowledge
        them. Never retry tools, delegations, owner steers or uncertain crashes.
        """
        frames = []
        with self.store.transaction():
            active = self.store.db.execute("SELECT 1 FROM turns WHERE thread_id=? AND id=? AND state='inProgress'", (tid, turn_id)).fetchone()
            thread = self.store.thread(tid)
            if not active or thread['halted'] or thread['archived'] or tid in self.store.history_mutations:
                return frames
            rows = self.store.db.execute('''SELECT i.*,b.context_turn_id AS previous_context_turn FROM inputs i JOIN busy_inputs b
                ON b.thread_id=i.thread_id AND b.message_id=i.message_id
                JOIN turns t ON t.thread_id=i.thread_id AND t.id=b.context_turn_id
                WHERE i.thread_id=? AND i.state IN ('failed','interrupted')
                  AND t.state IN ('failed','interrupted') AND t.reverted=0
                  AND json_extract(t.error,'$.codexErrorInfo')='usageLimitExceeded'
                  AND b.offered=1 AND b.persisted=0
                  AND json_extract(i.payload,'$.source.kind')='agent'
                  AND json_extract(i.payload,'$.eventType')='agent_message'
                ORDER BY i.seq LIMIT ?''', (tid, self.server.settings.max_pending)).fetchall()
            for row in rows:
                payload = json.loads(row['payload'])
                # One item identity per inbox message. Move its unacknowledged
                # claim projection; the old turn's event journal remains intact.
                self.store.db.execute('UPDATE items SET turn_id=? WHERE turn_id=? AND id=?', (turn_id, row['previous_context_turn'], row['message_id']))
                self.store.put_item(turn_id, self.store.external_item(row['message_id'], payload))
                self.store.db.execute("UPDATE inputs SET state='injecting' WHERE seq=?", (row['seq'],))
                self.store.db.execute('UPDATE busy_inputs SET context_turn_id=? WHERE thread_id=? AND message_id=?', (turn_id, tid, row['message_id']))
                frames.append(self.store.project_input(payload)[0]['text'])
        return frames

    async def acknowledge(self, tid, turn_id):
        """Successful terminal fallback when no earlier native receipt was read."""
        await self.acknowledge_messages(tid, turn_id)

    async def acknowledge_messages(self, tid, turn_id, message_ids=None):
        """Confirm only the inputs covered by native evidence, never a later claim."""
        if message_ids is not None and not message_ids:
            return
        selected = '' if message_ids is None else ' AND i.message_id IN (' + ','.join('?' for _ in message_ids) + ')'
        rows = self.store.db.execute('''SELECT i.message_id FROM inputs i JOIN busy_inputs b
            ON b.thread_id=i.thread_id AND b.message_id=i.message_id
            WHERE i.thread_id=? AND b.context_turn_id=? AND i.state='injecting' AND b.persisted=0 AND b.offered=1'''
            + selected, (tid, turn_id, *(message_ids or ()))).fetchall()
        if rows:
            # claude-r35: every item was published when its hook claimed it
            # (inject). Never again here, and never after its own receipt.
            await self.items_published(tid)
        note_selected = '' if message_ids is None else ' AND message_id IN (' + ','.join('?' for _ in message_ids) + ')'
        notes = [] if message_ids is None else self.store.db.execute('''SELECT message_id FROM notes WHERE thread_id=? AND turn_id=?
            AND state='injected' AND json_extract(payload,'$.metadata.ownerCarrier') IS NOT NULL
            AND NOT EXISTS(SELECT 1 FROM external_inputs e WHERE e.thread_id=notes.thread_id
                AND e.message_id=notes.message_id AND e.receipt IS NOT NULL)'''
            + note_selected, (tid, turn_id, *(message_ids or ()))).fetchall()
        if notes:
            await self.items_published(tid)
        for row in [*rows, *notes]:
            await self.server.engine.emit(tid, 'claude/input/contextPersisted',
                {'threadId': tid, 'turnId': turn_id, 'messageId': row['message_id'],
                 'clientId': self.client_id(tid, row['message_id'])})

    def offered(self, tid, turn_id, *, call_id=None, context=None, bridge=None, hook_name=None):
        # Synchronous final step of the authenticated hook: no await between
        # recording the offer and returning its additionalContext to the SDK.
        self.store.db.execute('UPDATE busy_inputs SET offered=1 WHERE thread_id=? AND context_turn_id=?', (tid, turn_id))
        for row in self.store.db.execute('SELECT message_id FROM busy_inputs WHERE thread_id=? AND context_turn_id=?', (tid, turn_id)).fetchall():
            self.stage(tid, row[0], turn_id, 'offered')
        message_ids = self.claimed.pop((tid, turn_id), ())
        if message_ids and call_id and context and bridge:
            self.context_receipts.offer(tid, bridge, call_id, message_ids, context, hook_name=hook_name)

    def observe(self, tid, method, params):
        if method == 'claude/input/contextPersisted':
            mid, turn = params.get('messageId'), params.get('turnId')
            with self.store.transaction():
                self.store.db.execute('''UPDATE busy_inputs SET persisted=1 WHERE thread_id=? AND message_id=?
                    AND ((context_turn_id=? AND offered=1) OR EXISTS(SELECT 1 FROM inputs i WHERE i.thread_id=? AND i.message_id=? AND i.turn_id=?))''',
                    (tid, mid, turn, tid, mid, turn))
                saved = self.store.db.execute('SELECT persisted FROM busy_inputs WHERE thread_id=? AND message_id=?', (tid, mid)).fetchone()
                if saved and saved[0]:
                    self.stage(tid, mid, turn, 'contextPersisted')
        if method == 'turn/completed':
            turn = params.get('turn') or {}
            self.store.db.execute('''UPDATE inputs SET state=? WHERE thread_id=? AND state='injecting'
                AND message_id IN (SELECT message_id FROM busy_inputs WHERE thread_id=? AND context_turn_id=?)''',
                (turn.get('status', 'uncertain'), tid, tid, turn.get('id')))
        # finish() makes all consumed inputs terminal; their durable persisted
        # bit distinguishes success from unknown/failed context on reconciliation.
