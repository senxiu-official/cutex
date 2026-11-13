"""Native external-input envelopes over the durable adapter inbox.

Wire contracts and domain-separated digests follow cute-codex's
protocol/{external_input,external_input_status}.rs. A local queue receipt is
never presented as context persistence. SDK inputs wait for an explicit SDK
acknowledgement; recovery never retries a claimed or uncertain request.
"""
from __future__ import annotations

import hashlib
import json
import re
import time
import uuid
from collections import OrderedDict

from . import asks
from .errors import RpcError, require, text
from .store import dump

METHODS = frozenset('thread/externalInput/' + action for action in ('submit', 'status', 'retry', 'resume', 'interrupt', 'returnCarries'))
BINDING_FIELDS = {'version', 'ownerId', 'threadId', 'runtimeGeneration'}


def _hash(domain: bytes, fields: list[str]):
    result = hashlib.sha256(domain)
    for field in fields:
        raw = field.encode('utf-8')
        result.update(len(raw).to_bytes(8, 'big'))
        result.update(raw)
    return result


def semantic_digest(envelope: dict) -> str:
    message = envelope['message']
    fields = [envelope['ownerId'], envelope['threadId'], message['id'], message['source']['kind'],
              message['source']['id'], message['type'], message['delivery'], message['text']]
    if envelope['version'] == 2:
        fields.append(json.dumps(envelope.get('view'), ensure_ascii=False, sort_keys=True, separators=(',', ':'), allow_nan=False))
    return _hash(f"codex:external-input:v{envelope['version']}\0".encode('ascii'), fields).hexdigest()


def _digest(value) -> str:
    require(isinstance(value, str) and re.fullmatch('[0-9a-f]{64}', value) is not None,
            'semanticSha256 must be a lowercase SHA-256 digest')
    return value


def _view(view) -> None:
    if view is None:
        return
    require(isinstance(view, dict) and set(view) == {'schema', 'data'} and isinstance(view['data'], dict),
            'view must be {schema, data}')
    require(isinstance(view['schema'], str) and re.fullmatch('[A-Za-z0-9._-]{1,128}', view['schema']) is not None,
            'invalid view schema')
    entries = 0

    def visit(value, depth):
        nonlocal entries
        if isinstance(value, (dict, list)):
            require(depth <= 8, 'view exceeds maximum depth')
            entries += len(value)
            require(entries <= 1024, 'view has too many entries')
            for child in (value.values() if isinstance(value, dict) else value):
                visit(child, depth + 1)
        elif type(value) is int:
            require(-(2 ** 63) <= value < 2 ** 64, 'view integer is outside 64-bit bounds')
        else:
            require(value is None or isinstance(value, (str, bool)), 'view supports JSON integers only')

    visit(view['data'], 1)
    require(len(dump(view).encode('utf-8')) <= 16384, 'view exceeds 16384 bytes')


def make_receipt(envelope: dict, turn_id: str, ordinal: int) -> dict:
    message_id = envelope['message']['id']
    fields = [envelope['ownerId'], envelope['threadId'], message_id, envelope['semanticSha256'], message_id, turn_id]
    digest = _hash(b'codex:external-input-receipt:v1\0', fields)
    digest.update(ordinal.to_bytes(8, 'big'))
    return {'schema': 'codex.external-input-receipt.v1', 'receiptId': 'eir1_' + digest.hexdigest(),
            'ownerId': envelope['ownerId'], 'threadId': envelope['threadId'], 'messageId': message_id,
            'semanticSha256': envelope['semanticSha256'], 'responseItemId': message_id,
            'turnId': turn_id, 'ordinal': ordinal}


class ExternalInputService:
    # Full envelope duplicates expire; identity, receipts and retry deduplication
    # remain for the lifetime of the thread. The authoritative inbox is intact.
    ENVELOPE_RETENTION_SECONDS = 30 * 24 * 60 * 60
    MAINTENANCE_INTERVAL_SECONDS = 60 * 60
    MAINTENANCE_BATCH = 128

    def __init__(self, server):
        self.server, self.store, self.settings = server, server.store, server.settings
        self.runtime_generation = self.settings.external_runtime_generation
        self.store.db.executescript('''
          CREATE TABLE IF NOT EXISTS external_inputs(
            thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
            message_id TEXT NOT NULL, envelope TEXT NOT NULL, receipt TEXT,
            attempt_id TEXT, output_observed INTEGER NOT NULL DEFAULT 0,
            last_status TEXT, PRIMARY KEY(thread_id,message_id));
          CREATE TABLE IF NOT EXISTS returned_carries(
            thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
            message_id TEXT NOT NULL, semantic_sha256 TEXT NOT NULL,
            PRIMARY KEY(thread_id,message_id));
          CREATE TABLE IF NOT EXISTS external_input_retries(
            thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
            retry_id TEXT NOT NULL, request TEXT NOT NULL,
            PRIMARY KEY(thread_id,retry_id));
        ''')
        with self.store.transaction():
            if 'compacted' not in {r[1] for r in self.store.db.execute('PRAGMA table_info(external_inputs)')}:
                self.store.db.execute('ALTER TABLE external_inputs ADD COLUMN compacted INTEGER NOT NULL DEFAULT 0')
            self.store.db.execute('CREATE INDEX IF NOT EXISTS external_inputs_uncompacted ON external_inputs(thread_id,message_id) WHERE compacted=0')
            self.store.db.execute('CREATE INDEX IF NOT EXISTS external_notes_turn ON notes(thread_id,turn_id,message_id)')
        self._maintenance_at = time.monotonic()
        self._recovery_cache = OrderedDict()
        self.store.db.execute('''CREATE TABLE IF NOT EXISTS external_input_recovery(
            thread_id TEXT NOT NULL, message_id TEXT NOT NULL, native_session_id TEXT NOT NULL,
            request_id TEXT NOT NULL, output_uuid TEXT NOT NULL, turn_id TEXT NOT NULL,
            PRIMARY KEY(thread_id,message_id))''')
        if self.enabled:
            self.bind({'ownerId': self.settings.external_owner_id, 'runtimeGeneration': self.runtime_generation})
            self.compact_history()
            # Notes a previous process read into a turn that then ended
            # interrupted or failed (before this was settled at turn end).
            for row in self.store.db.execute('''SELECT DISTINCT n.thread_id,n.turn_id FROM notes n
                    JOIN external_inputs e ON e.thread_id=n.thread_id AND e.message_id=n.message_id
                    JOIN turns t ON t.id=n.turn_id
                    WHERE n.state='injected' AND e.receipt IS NULL AND t.state IN ('interrupted','failed','uncertain')''').fetchall():
                self.settle_notes(row['thread_id'], row['turn_id'])

    def compact_history(self, *, now: float | None = None) -> int:
        """Bounded maintenance; never delete an identity or change its receipt.

        Terminal envelopes older than 30 days retain every field needed for
        digest conflict checks, delayed acknowledgement and stable receipts.
        Retry records and the original inbox/history are never pruned here.
        """
        cutoff = (time.time() if now is None else now) - self.ENVELOPE_RETENTION_SECONDS
        has_busy = getattr(self.server, 'busy_delivery', None) is not None
        busy_join = 'LEFT JOIN busy_inputs b ON b.thread_id=e.thread_id AND b.message_id=e.message_id' if has_busy else ''
        context_turn = 'COALESCE(b.context_turn_id,i.turn_id,n.turn_id)' if has_busy else 'COALESCE(i.turn_id,n.turn_id)'
        with self.store.transaction():
            rows = self.store.db.execute(f'''SELECT e.thread_id,e.message_id,e.envelope FROM external_inputs e
                LEFT JOIN inputs i ON i.thread_id=e.thread_id AND i.message_id=e.message_id
                LEFT JOIN notes n ON n.thread_id=e.thread_id AND n.message_id=e.message_id
                {busy_join}
                JOIN turns t ON t.id={context_turn}
                WHERE e.compacted=0 AND t.completed<=? AND t.state<>'inProgress'
                  AND COALESCE(i.state,n.state) NOT IN ('queued','pending','processing','injecting')
                LIMIT ?''', (cutoff, self.MAINTENANCE_BATCH)).fetchall()
            for row in rows:
                envelope = json.loads(row['envelope'])
                identity = {key: envelope[key] for key in ('ownerId', 'threadId', 'semanticSha256')}
                identity['message'] = {'id': envelope['message']['id']}
                self.store.db.execute('UPDATE external_inputs SET envelope=?,compacted=1 WHERE thread_id=? AND message_id=?',
                    (dump(identity), row['thread_id'], row['message_id']))
        return len(rows)

    @property
    def enabled(self) -> bool:
        return self.settings.external_owner_id is not None and self.runtime_generation is not None

    def bind(self, params: dict) -> dict:
        """Authenticated controller updates an occurrence; never changes owner or inbox."""
        require(set(params) == {'ownerId', 'runtimeGeneration'}, 'binding requires ownerId and runtimeGeneration')
        require(self.enabled and params['ownerId'] == self.settings.external_owner_id,
                'external input owner binding mismatch', -32011)
        generation = params['runtimeGeneration']
        require(type(generation) is int and 0 <= generation < 2 ** 64, 'invalid runtimeGeneration')
        require(generation >= self.runtime_generation, 'external runtime generation cannot move backwards', -32011)
        with self.store.transaction():
            previous = self.store.db.execute("SELECT value FROM meta WHERE key='external_input_binding'").fetchone()
            if previous:
                stored = json.loads(previous[0])
                require(stored['ownerId'] == params['ownerId'] and generation >= stored['runtimeGeneration'],
                        'external input binding conflicts with durable owner/generation', -32011)
            self.store.db.execute("INSERT OR REPLACE INTO meta(key,value) VALUES('external_input_binding',?)", (dump(params),))
        self.runtime_generation = generation
        return {'ownerId': self.settings.external_owner_id, 'runtimeGeneration': generation}

    def capabilities(self) -> dict:
        if not self.enabled:
            return {}
        return {'externalInputVersion': 1, 'externalInputVersions': [1, 2],
                'externalInputDeliveries': ['after_turn', 'passive'] + (['soon'] if self.settings.backend == 'subscription' else []),
                'externalInputControlVersion': 1,
                **({'ownerInputCarries': 1, 'ownerCarryReturn': 1} if self.settings.backend == 'subscription' else {})}

    def _binding(self, params: dict, *, submit=False) -> str:
        require(self.enabled, 'native external input requires a trusted host owner/runtime binding', -32011)
        version = params.get('version')
        require(type(version) is int and version in ({1, 2} if submit else {1}), 'unsupported external input version')
        owner = text(params.get('ownerId'), 'ownerId', limit=256)
        tid = text(params.get('threadId'), 'threadId', limit=256)
        generation = params.get('runtimeGeneration')
        require(type(generation) is int and 0 <= generation < 2 ** 64, 'invalid runtimeGeneration')
        require(owner == self.settings.external_owner_id and generation == self.runtime_generation,
                'external input owner/runtime binding mismatch', -32011)
        self.store.thread(tid)
        return tid

    def _response(self, tid: str) -> dict:
        return {'version': 1, 'ownerId': self.settings.external_owner_id, 'threadId': tid,
                'runtimeGeneration': self.runtime_generation}

    def _row(self, tid: str, mid: str):
        return self.store.db.execute('SELECT * FROM external_inputs WHERE thread_id=? AND message_id=?', (tid, mid)).fetchone()

    def _inbox(self, tid: str, mid: str):
        row = self.store.db.execute('SELECT *,0 AS passive FROM inputs WHERE thread_id=? AND message_id=?', (tid, mid)).fetchone()
        busy = getattr(self.server, 'busy_delivery', None)
        if row is not None and busy:
            claimed = self.store.db.execute('SELECT context_turn_id FROM busy_inputs WHERE thread_id=? AND message_id=?', (tid, mid)).fetchone()
            if claimed and claimed[0]:
                row = {**dict(row), 'turn_id': claimed[0]}
                context = self.store.db.execute('SELECT reverted FROM turns WHERE id=?', (claimed[0],)).fetchone()
                if context and context[0]:
                    row['state'] = 'reverted'
        if row is not None:
            return row
        note = self.store.db.execute('SELECT *,1 AS passive FROM notes WHERE thread_id=? AND message_id=?', (tid, mid)).fetchone()
        if note is not None:
            carrier = json.loads(note['payload']).get('metadata', {}).get('ownerCarrier')
            if carrier:
                owner = self.store.db.execute('SELECT state FROM inputs WHERE thread_id=? AND message_id=?', (tid, carrier)).fetchone()
                if owner and owner[0] in {'interrupted', 'failed', 'uncertain', 'completed'}:
                    return {**dict(note), 'state': owner[0]}
        return note

    def _status(self, tid: str, mid: str, digest: str, *, recover: bool = False) -> dict:
        result = {'messageId': mid, 'semanticSha256': digest, 'deliveryState': 'unknown', 'receipt': None,
                  'processing': {'state': 'none', 'attemptId': None, 'reason': None}}
        row = self._row(tid, mid)
        if row is None:
            # A legacy inbox entry is not a native envelope. Never reinterpret its receipt.
            if self._inbox(tid, mid) is not None:
                result['deliveryState'] = 'conflict'
            return result
        envelope = json.loads(row['envelope'])
        if envelope['ownerId'] != self.settings.external_owner_id or envelope['semanticSha256'] != digest:
            result['deliveryState'] = 'conflict'
            return result
        inbox = self._inbox(tid, mid)
        if inbox is None:
            return result
        if inbox['state'] in {'reverted', 'inherited'}:
            # A historical receipt remains an audit fact after rewind, but no longer
            # proves that the selected model context contains this input.
            result['processing'] = {'state': 'held', 'attemptId': row['attempt_id'], 'reason': 'context_missing'}
            return result
        if recover and row['receipt'] is None and inbox['state'] in {'failed', 'interrupted', 'uncertain', 'completed', 'injected'}:
            self._recover_receipts(tid, mid)
            row = self._row(tid, mid)
        result['receipt'] = json.loads(row['receipt']) if row['receipt'] else None
        result['deliveryState'] = 'context_persisted' if row['receipt'] else 'pending'
        if inbox['passive']:
            if not row['receipt'] and inbox['state'] in {'failed', 'interrupted', 'uncertain', 'completed'}:
                result['processing'] = {'state':'held', 'attemptId':row['attempt_id'], 'reason':'request_uncertain'}
            if not row['receipt'] and json.loads(inbox['payload']).get('metadata', {}).get('ownerCarrier'):
                state = inbox['state']
                result['processing'] = {'state': 'pending' if state == 'reserved' else ('claimed' if state == 'injected' else 'held'),
                                        'attemptId': row['attempt_id'], 'reason': None if state in {'reserved', 'injected'} else ('interrupted' if state == 'interrupted' else 'request_uncertain')}
            return result
        attempt = row['attempt_id']
        state = inbox['state']
        if state not in {'queued', 'pending'} and attempt is None:
            attempt = str(uuid.uuid5(uuid.NAMESPACE_URL, f"cute-claude:external:{tid}:{inbox['turn_id']}"))
            self.store.db.execute('UPDATE external_inputs SET attempt_id=? WHERE thread_id=? AND message_id=?', (attempt, tid, mid))
        processing, reason = 'pending', None
        if row['output_observed']:
            processing = 'output_observed'
        elif state in {'processing', 'injecting'}:
            processing = 'claimed'
        elif state in {'uncertain', 'failed', 'interrupted', 'completed'}:
            processing = 'held'
            reason = 'interrupted' if state == 'interrupted' else ('no_output' if state == 'completed' else 'request_uncertain')
        elif self.store.thread(tid)['halted']:
            processing, reason = 'held', 'interrupted'
        result['processing'] = {'state': processing, 'attemptId': attempt, 'reason': reason}
        return result

    def _return_carries(self, tid: str, keys: list[dict]) -> list[str]:
        """Give back the notes whose carrier steer is proven never read: they
        leave this runtime in one transaction, so no later claim can read them
        beside the copy cutex sends on its own. Anything else stays and is not
        listed. The proof outlives the note: a lost answer gets the same answer."""
        returned = []
        with self.store.transaction():
            for key in keys:
                mid = text(key['messageId'], 'messageId', limit=256)
                digest = _digest(key['semanticSha256'])
                row = self._row(tid, mid)
                if row is None:
                    proof = self.store.db.execute('SELECT semantic_sha256 FROM returned_carries WHERE thread_id=? AND message_id=?', (tid, mid)).fetchone()
                    if proof and proof[0] == digest and self._inbox(tid, mid) is None:
                        returned.append(mid)
                    continue
                if json.loads(row['envelope'])['semanticSha256'] != digest or row['receipt'] is not None:
                    continue
                note = self.store.db.execute("SELECT payload FROM notes WHERE thread_id=? AND message_id=? AND state='reserved'", (tid, mid)).fetchone()
                carrier = json.loads(note[0]).get('metadata', {}).get('ownerCarrier') if note else None
                if carrier and self.store.steer_dropped_unread(tid, carrier):
                    self._take_back(tid, mid, digest)
                    returned.append(mid)
        return returned

    def _take_back(self, tid: str, mid: str, digest: str) -> None:
        self.store.db.execute('DELETE FROM notes WHERE thread_id=? AND message_id=?', (tid, mid))
        self.store.db.execute('DELETE FROM external_inputs WHERE thread_id=? AND message_id=?', (tid, mid))
        self.store.db.execute('INSERT OR REPLACE INTO returned_carries VALUES(?,?,?)', (tid, mid, digest))

    def return_carrier_notes(self, tid: str, carrier: str) -> None:
        """The caller proved `carrier` dropped and sends it again without them."""
        rows = self.store.db.execute("""SELECT n.message_id,e.envelope FROM notes n
            JOIN external_inputs e ON e.thread_id=n.thread_id AND e.message_id=n.message_id
            WHERE n.thread_id=? AND n.state='reserved' AND e.receipt IS NULL
              AND json_extract(n.payload,'$.metadata.ownerCarrier')=?""", (tid, carrier)).fetchall()
        for row in rows:
            self._take_back(tid, row['message_id'], json.loads(row['envelope'])['semanticSha256'])

    def _recover_receipts(self, tid: str, wanted: str) -> None:
        """One bounded native scan per file/mapping version, without replay."""
        if self.settings.backend != 'subscription':
            return
        if tid in self.store.history_mutations:
            return
        inspect = getattr(self.server.engine, 'inspect_session', None)
        if inspect is None:
            return
        session = inspect(tid)
        if not session:
            return
        try:
            from .session_ops import NativeSessionApi
            from .receipt_recovery import recover
            path = NativeSessionApi(self.server.engine).transcript(session['session_id'])
            if path is None:
                return
            metadata = path.stat()
            mapped = {r['native_uuid']: r['turn_id'] for r in self.store.db.execute('''
                SELECT m.native_uuid,m.turn_id FROM sdk_turn_messages m JOIN turns t ON t.id=m.turn_id
                WHERE m.thread_id=? AND m.role='assistant' AND t.reverted=0''', (tid,))}
            key = (session['session_id'], metadata.st_size, metadata.st_mtime_ns, tuple(mapped.items()))
            cached = self._recovery_cache.get(tid)
            if cached and cached[0] == key and wanted in cached[1]:
                return
            expected, duplicates, bindings = {}, set(), {}
            rows = self.store.db.execute('''SELECT message_id,envelope FROM external_inputs
                    WHERE thread_id=? AND receipt IS NULL ORDER BY (message_id=?) DESC,message_id LIMIT 200''', (tid, wanted)).fetchall()
            for row in rows:
                mid = row['message_id']
                inbox = self._inbox(tid, mid)
                if not inbox or inbox['state'] not in {'failed', 'interrupted', 'uncertain', 'completed', 'injected'}:
                    continue
                payload = json.loads(inbox['payload'])
                envelope = json.loads(row['envelope'])
                if (envelope.get('threadId') != tid or envelope.get('ownerId') != self.settings.external_owner_id
                        or envelope.get('message', {}).get('id') != mid
                        or semantic_digest(envelope) != envelope.get('semanticSha256')):
                    continue
                canonical = self.payload(envelope)
                if any(payload.get(k) != canonical.get(k) for k in ('source', 'eventType', 'text', 'view')):
                    continue
                message = envelope['message']
                identical = self.store.db.execute('''SELECT count(*) FROM external_inputs WHERE thread_id=?
                    AND json_extract(envelope,'$.message.source.kind')=?
                    AND json_extract(envelope,'$.message.source.id')=?
                    AND json_extract(envelope,'$.message.type')=? AND json_extract(envelope,'$.message.text')=?''',
                    (tid, message['source']['kind'], message['source']['id'], message['type'], message['text'])).fetchone()[0]
                if identical != 1:
                    continue
                user_uuids = {r[0] for r in self.store.db.execute('''SELECT native_uuid FROM sdk_turn_messages
                    WHERE thread_id=? AND turn_id=? AND role='user' ''', (tid, inbox['turn_id']))}
                context_turn = inbox['turn_id'] if inbox['passive'] and inbox['state'] == 'injected' else None
                if getattr(self.server, 'busy_delivery', None):
                    busy = self.store.db.execute('''SELECT context_turn_id,offered FROM busy_inputs
                        WHERE thread_id=? AND message_id=?''', (tid, mid)).fetchone()
                    if busy is not None:
                        if not busy['offered']:
                            continue
                        context_turn = busy['context_turn_id']
                        user_uuids = set()
                bindings[mid] = {'user_uuids':user_uuids, 'context_turn':context_turn}
                frame = {'source':payload['source'], 'eventType':payload['eventType'], 'text':payload['text']}
                ref = asks.ask_ref(payload)
                if ref is not None:
                    frame['ask'] = ref
                encoded = dump(frame)
                if encoded in duplicates:
                    continue
                matches = [other for other, value in expected.items() if value == frame]
                if matches:
                    for other in matches:
                        del expected[other]
                    duplicates.add(encoded)
                else:
                    expected[mid] = frame
            proofs = recover(path, session['session_id'], expected, mapped, bindings) if expected else {}
            after = path.stat()
            if (after.st_size, after.st_mtime_ns) != (metadata.st_size, metadata.st_mtime_ns):
                return
            for mid, (turn_id, request_id, output_uuid) in proofs.items():
                with self.store.transaction():
                    self._persisted(tid, mid, turn_id, native_proof=True)
                    if self._row(tid, mid)['receipt'] is not None:
                        self.store.db.execute('''INSERT OR IGNORE INTO external_input_recovery
                            VALUES(?,?,?,?,?,?)''', (tid, mid, session['session_id'], request_id, output_uuid, turn_id))
            self._recovery_cache[tid] = (key, {row['message_id'] for row in rows})
            self._recovery_cache.move_to_end(tid)
            while len(self._recovery_cache) > 16:
                self._recovery_cache.popitem(last=False)
        except (OSError, ValueError, KeyError, TypeError, RpcError):
            # Missing/truncated history is unproven, never a success or resend.
            return

    async def _changed(self, tid: str, mid: str) -> None:
        row = self._row(tid, mid)
        if row is None:
            return
        envelope = json.loads(row['envelope'])
        status = dump(self._status(tid, mid, envelope['semanticSha256']))
        if status != row['last_status']:
            self.store.db.execute('UPDATE external_inputs SET last_status=? WHERE thread_id=? AND message_id=?', (status, tid, mid))
            await self.server.engine.emit(tid, 'thread/externalInput/statusChanged', {'threadId': tid, 'messageId': mid})

    def _persisted(self, tid: str, mid: str, turn_id: str, *, native_proof: bool = False) -> dict | None:
        row = self._row(tid, mid)
        if row is None or row['receipt'] is not None:
            return
        if not native_proof and getattr(self.server, 'busy_delivery', None):
            busy = self.store.db.execute('SELECT context_turn_id,offered FROM busy_inputs WHERE thread_id=? AND message_id=?', (tid, mid)).fetchone()
            if busy and busy[0] and not busy[1]:
                return
        # Receipt and its stable ordinal are allocated only on context acknowledgement.
        with self.store.transaction():
            ordinal = self.store.db.execute('SELECT count(*) FROM external_inputs WHERE thread_id=? AND receipt IS NOT NULL', (tid,)).fetchone()[0] + 1
            receipt = make_receipt(json.loads(row['envelope']), turn_id, ordinal)
            self.store.db.execute('UPDATE external_inputs SET receipt=? WHERE thread_id=? AND message_id=?', (dump(receipt), tid, mid))
            # The receipt and its grant window commit together, including
            # transcript recovery. A crash cannot lose the first-read clock.
            grants = getattr(self.server, 'safety_grants', None)
            return grants.record_notice_read(tid, mid) if grants else None

    # Timeline items of a turn's inputs; any other item is the model's own
    # (its prompt, with the notes read at its start, reached the model).
    _INPUT_ITEMS = ('userMessage', 'functionCallOutput')

    def settle_notes(self, tid: str, turn_id: str) -> tuple[list[str], list[str]]:
        """Native notes read into `turn_id`, which ended interrupted or failed
        (on the subscription backend only a successful result acknowledges them).

        If the model answered in that turn, the notes reached its context:
        they get their receipt now. If it never answered, they go back to
        pending (their items leave that turn) and the next turn or a wake
        reads them. Either way none stays read but unconfirmed. Returns
        (acknowledged, released) message ids."""
        if not self.enabled or self.settings.backend != 'subscription':
            return [], []
        rows = self.store.db.execute('''SELECT n.seq,n.message_id FROM notes n
            JOIN external_inputs e ON e.thread_id=n.thread_id AND e.message_id=n.message_id
            WHERE n.thread_id=? AND n.turn_id=? AND n.state='injected' AND e.receipt IS NULL
              AND json_extract(n.payload,'$.metadata.ownerCarrier') IS NULL ORDER BY n.seq''',
            (tid, turn_id)).fetchall()
        if not rows:
            return [], []
        answered = self.store.db.execute(
            "SELECT 1 FROM items WHERE turn_id=? AND json_extract(payload,'$.type') NOT IN (?,?) LIMIT 1",
            (turn_id, *self._INPUT_ITEMS)).fetchone() is not None
        if answered:
            for row in rows:
                self._persisted(tid, row['message_id'], turn_id)
            return [row['message_id'] for row in rows], []
        with self.store.transaction():
            for row in rows:
                self.store.db.execute("UPDATE notes SET state='pending',turn_id=NULL WHERE seq=?", (row['seq'],))
                self.store.db.execute('DELETE FROM items WHERE turn_id=? AND id=?', (turn_id, row['message_id']))
        return [], [row['message_id'] for row in rows]

    async def observe_event(self, tid: str, method: str, params: dict) -> None:
        """Parent calls this from Engine.emit; recursive native notifications are ignored."""
        watched = {'turn/started', 'turn/completed', 'claude/input/contextPersisted', 'claude/input/statusChanged',
                   'item/completed'}
        if method not in watched:
            return
        item = params.get('item') or {}
        output = method == 'item/completed' and item.get('type') in {'agentMessage', 'reasoning', 'commandExecution', 'fileChange', 'dynamicToolCall'}
        if method == 'item/completed' and not output:
            return
        turn = params.get('turn') or {}
        turn_id = params.get('turnId') or turn.get('id')
        if method == 'turn/completed' and turn_id and turn.get('status') in {'interrupted', 'failed', 'uncertain'}:
            self.settle_notes(tid, turn_id)
        # Resolve the small turn/message candidate set before probing the
        # permanent receipt ledger. Never scan it for each streamed token.
        candidates = '''SELECT message_id FROM inputs WHERE thread_id=? AND turn_id=?
            UNION SELECT message_id FROM notes WHERE thread_id=? AND turn_id=?'''
        arguments = [tid, turn_id, tid, turn_id]
        if getattr(self.server, 'busy_delivery', None):
            candidates += ' UNION SELECT message_id FROM busy_inputs WHERE thread_id=? AND context_turn_id=?'
            arguments.extend([tid, turn_id])
        if params.get('messageId') is not None:
            candidates += ' UNION SELECT ?'
            arguments.append(params['messageId'])
        if method == 'turn/completed':
            candidates += " UNION SELECT message_id FROM inputs WHERE thread_id=? AND state='queued'"
            arguments.append(tid)
        rows = self.store.db.execute('SELECT message_id FROM external_inputs WHERE thread_id=? AND message_id IN (' + candidates + ')',
                                    [tid, *arguments]).fetchall()
        acknowledged = method == 'claude/input/contextPersisted'
        if acknowledged:
            # A native attachment proves only its explicitly covered input.
            # Other offers in this turn can still be waiting for persistence.
            rows = [row for row in rows if row['message_id'] == params.get('messageId')]
        for row in rows:
            mid = row['message_id']
            inbox = self._inbox(tid, mid)
            if inbox is None:
                continue
            grant_read = None
            if turn_id and inbox['turn_id'] == turn_id:
                if acknowledged or (self.settings.backend != 'subscription' and inbox['state'] not in {'queued', 'pending'}):
                    grant_read = self._persisted(tid, mid, turn_id)
                if output and not inbox['passive']:
                    self.store.db.execute('UPDATE external_inputs SET output_observed=1 WHERE thread_id=? AND message_id=?', (tid, mid))
            if grant_read is not None:
                await self.server.engine.emit(tid, 'thread/safetyCheck/updated', {'threadId': tid, 'block': grant_read})
            await self._changed(tid, mid)
        if method == 'turn/completed' and time.monotonic() - self._maintenance_at >= self.MAINTENANCE_INTERVAL_SECONDS:
            self.compact_history()
            self._maintenance_at = time.monotonic()

    async def dispatch(self, peer, method: str, params: dict):
        require(method in METHODS, 'unsupported native external input method')
        action = method.rsplit('/', 1)[-1]
        extra = {'submit': {'message', 'view', 'semanticSha256', 'admission'}, 'status': {'messages'}, 'returnCarries': {'messages'},
                 'retry': {'messageId', 'semanticSha256', 'expectedAttemptId', 'retryId'},
                 'resume': set(), 'interrupt': {'turnId'}}[action]
        require(not set(params) - BINDING_FIELDS - extra, 'unsupported external input fields')
        tid = self._binding(params, submit=action == 'submit')
        if action != 'status':
            require(tid not in self.store.history_mutations, 'session history mutation is in progress', -32009)
        if action == 'submit':
            return await self._submit(peer, tid, params)
        if action == 'status':
            keys = params.get('messages')
            require(isinstance(keys, list) and 1 <= len(keys) <= 100, 'status requires 1..100 message keys')
            statuses = []
            for key in keys:
                require(isinstance(key, dict) and set(key) == {'messageId', 'semanticSha256'}, 'invalid message key')
                statuses.append(self._status(tid, text(key['messageId'], 'messageId', limit=256), _digest(key['semanticSha256']), recover=True))
            peer.subscriptions.add(tid)
            return {**self._response(tid), 'statuses': statuses}, None
        if action == 'returnCarries':
            keys = params.get('messages')
            require(isinstance(keys, list) and 1 <= len(keys) <= 100, 'returnCarries requires 1..100 message keys')
            for key in keys:
                require(isinstance(key, dict) and set(key) == {'messageId', 'semanticSha256'}, 'invalid message key')
            return {**self._response(tid), 'returned': self._return_carries(tid, keys)}, None
        if action == 'retry':
            return self._retry(tid, params)
        require(not self.store.thread(tid)['archived'], 'thread is archived', -32009)
        if action == 'resume':
            reopened = bool(self.store.thread(tid)['halted'])
            self.store.db.execute('UPDATE threads SET halted=0 WHERE id=?', (tid,))
            return {**self._response(tid), 'reopened': reopened}, lambda: self.server.engine.wake(tid)
        turn_id = text(params.get('turnId'), 'turnId', limit=256)
        was_halted = bool(self.store.thread(tid)['halted'])
        await self.server.engine.interrupt(tid, turn_id)
        # Keep the previous gate; interrupt never requeues the interrupted message.
        self.store.db.execute('UPDATE threads SET halted=? WHERE id=?', (int(was_halted), tid))
        return {}, (None if was_halted else lambda: self.server.engine.wake(tid))

    def validate_envelope(self, tid: str, params: dict) -> dict:
        require(isinstance(params, dict) and not set(params) - BINDING_FIELDS - {'message', 'view', 'semanticSha256'},
                'invalid canonical external envelope')
        require(self._binding(params, submit=True) == tid, 'carried thread binding mismatch', -32011)
        message = params.get('message')
        require(isinstance(message, dict) and set(message) == {'id', 'source', 'type', 'delivery', 'text'}, 'invalid external input message')
        mid = text(message['id'], 'message.id', limit=256)
        text(message['type'], 'message.type', limit=256)
        text(message['text'], 'message.text')
        source = message['source']
        require(isinstance(source, dict) and set(source) == {'kind', 'id'} and isinstance(source['kind'], str)
                and source['kind'] in {'agent', 'service'}, 'invalid external input source')
        text(source['id'], 'source.id', limit=256)
        supported = {'after_turn', 'passive'} | ({'soon'} if self.settings.backend == 'subscription' else set())
        require(isinstance(message['delivery'], str) and message['delivery'] in supported,
                'unsupported external delivery for this backend')
        view = params.get('view')
        _view(view)
        require(params['version'] == 2 or view is None, 'v1 external input cannot carry a view')
        digest = _digest(params.get('semanticSha256'))
        envelope = {key: params[key] for key in BINDING_FIELDS | {'message', 'semanticSha256'}}
        if view is not None:
            envelope['view'] = view
        require(semantic_digest(envelope) == digest, 'external input semantic digest mismatch')
        return envelope

    @staticmethod
    def payload(envelope: dict) -> dict:
        message, view = envelope['message'], envelope.get('view')
        metadata = {'nativeExternalInput': True}
        if isinstance(view, dict) and view.get('schema') == 'cutex.agent-message.v1':
            name = view['data'].get('senderName')
            if isinstance(name, str) and name.strip():
                metadata['sourceName'] = name[:256]
        if message['delivery'] in {'passive', 'soon'}:
            metadata['deliveryMode'] = message['delivery']
        payload = {'source': message['source'], 'eventType': message['type'], 'text': message['text'], 'metadata': metadata}
        if view is not None:
            payload['view'] = view
        return payload

    def owner_carries(self, tid: str, values) -> list[dict]:
        require(self.settings.backend == 'subscription' and self.enabled, 'owner carries require bound subscription runtime', -32011)
        require(isinstance(values, list) and len(values) <= 100, 'ownerCarries must be an array of at most 100 envelopes')
        result = [self.validate_envelope(tid, value) for value in values]
        require(len({e['message']['id'] for e in result}) == len(result), 'duplicate carried identity')
        require(sum(len(dump(e).encode('utf-8')) for e in result) <= 1024 * 1024, 'ownerCarries exceeds 1 MiB')
        return result

    def reserve_owner_carries(self, tid: str, carrier: str, values) -> None:
        # Caller encloses owner enqueue and reservations in one SQLite transaction.
        for envelope in values:
            mid = envelope['message']['id']
            old = self._row(tid, mid)
            note = self.store.db.execute('SELECT payload FROM notes WHERE thread_id=? AND message_id=?', (tid, mid)).fetchone()
            if old or self._inbox(tid, mid):
                require(old is not None and note is not None
                        and json.loads(old['envelope'])['semanticSha256'] == envelope['semanticSha256']
                        and json.loads(note[0]).get('metadata', {}).get('ownerCarrier') == carrier,
                        'carried identity already belongs to another submission', -32010)
                continue  # Retrying never rearms a claimed/terminal carrier.
            payload = self.payload(envelope)
            payload['metadata']['ownerCarrier'] = carrier
            self.store.enqueue_note(tid, mid, payload, self.settings.max_pending)
            self.store.db.execute("UPDATE notes SET state='reserved' WHERE thread_id=? AND message_id=?", (tid, mid))
            self.store.db.execute('INSERT INTO external_inputs(thread_id,message_id,envelope) VALUES(?,?,?)', (tid, mid, dump(envelope)))
            self.store.db.execute('DELETE FROM returned_carries WHERE thread_id=? AND message_id=?', (tid, mid))

    async def _submit(self, peer, tid: str, params: dict):
        envelope = self.validate_envelope(tid, {k: v for k, v in params.items() if k != 'admission'})
        message = envelope['message']
        mid, digest = message['id'], envelope['semanticSha256']
        admission = params.get('admission', 'queue')
        require(isinstance(admission, str) and admission in {'queue', 'exclusive_turn'}, 'unsupported external input admission')
        require(admission != 'exclusive_turn' or message['delivery'] == 'after_turn', 'exclusive turn requires after_turn delivery')
        existing = self._row(tid, mid)
        if existing is not None or self._inbox(tid, mid) is not None:
            status = self._status(tid, mid, digest)
            result = {**self._response(tid), 'statuses': [status]}
            if admission == 'exclusive_turn' and status['deliveryState'] != 'conflict':
                result['turnId'] = self._inbox(tid, mid)['turn_id']
            peer.subscriptions.add(tid)
            return result, None
        if admission == 'exclusive_turn':
            reason = asks.busy_reason(self.server, tid)
            if reason:
                raise asks.refuse_busy(reason)
        payload = self.payload(envelope)
        with self.store.transaction():
            enqueue = self.store.enqueue_note if message['delivery'] == 'passive' else self.store.enqueue
            receipt = enqueue(tid, mid, payload, self.settings.max_pending)
            self.store.db.execute('DELETE FROM returned_carries WHERE thread_id=? AND message_id=?', (tid, mid))
            if message['delivery'] == 'soon':
                self.server.busy_delivery.register(tid, mid, receipt)
            self.store.db.execute('INSERT INTO external_inputs(thread_id,message_id,envelope) VALUES(?,?,?)', (tid, mid, dump(envelope)))
        peer.subscriptions.add(tid)
        result = {**self._response(tid), 'statuses': [self._status(tid, mid, digest)]}
        if admission == 'exclusive_turn':
            result['turnId'] = receipt['turnId']

        async def after():
            await self._changed(tid, mid)
            if message['delivery'] != 'passive':
                self.server.engine.wake(tid)

        return result, after

    def _retry(self, tid: str, params: dict):
        mid = text(params.get('messageId'), 'messageId', limit=256)
        digest = _digest(params.get('semanticSha256'))
        retry_id = text(params.get('retryId'), 'retryId', limit=256)
        expected = params.get('expectedAttemptId')
        if expected is not None:
            try:
                require(isinstance(expected, str) and uuid.UUID(expected).int != 0, 'invalid expectedAttemptId')
            except ValueError:
                raise RpcError(-32602, 'invalid expectedAttemptId') from None
        request = dump({'messageId': mid, 'semanticSha256': digest, 'retryId': retry_id, 'expectedAttemptId': expected})
        prior = self.store.db.execute('SELECT request FROM external_input_retries WHERE thread_id=? AND retry_id=?', (tid, retry_id)).fetchone()
        response = {**self._response(tid), 'messageId': mid, 'semanticSha256': digest,
                    'expectedAttemptId': expected, 'retryId': retry_id, 'disposition': 'released'}
        if prior:
            require(prior[0] == request, 'retryId reused with different content', -32010)
            return response, None
        require(not self.store.thread(tid)['archived'], 'thread is archived', -32009)
        row = self._row(tid, mid)
        inbox = self._inbox(tid, mid)
        require(row is not None and inbox is not None, 'external input not found', -32004)
        envelope = json.loads(row['envelope'])
        require(envelope['ownerId'] == self.settings.external_owner_id and envelope['semanticSha256'] == digest,
                'external input identity conflict', -32010)
        require(not inbox['passive'] and inbox['state'] == 'queued' and row['attempt_id'] is None and expected is None,
                'only unclaimed durable inputs may be retried; uncertain or executed input is never replayed', -32009)
        require(not self.store.thread(tid)['halted'], 'interruption gate is closed; resume before retry', -32009)
        self.store.db.execute('INSERT INTO external_input_retries VALUES(?,?,?)', (tid, retry_id, request))
        return response, lambda: self.server.engine.wake(tid)
