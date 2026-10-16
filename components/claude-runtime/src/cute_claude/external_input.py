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

from . import asks
from .errors import RpcError, require, text
from .store import dump

METHODS = frozenset('thread/externalInput/' + action for action in ('submit', 'status', 'retry', 'resume', 'interrupt'))
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
        if self.enabled:
            self.bind({'ownerId': self.settings.external_owner_id, 'runtimeGeneration': self.runtime_generation})
            self.compact_history()

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
                'externalInputControlVersion': 1}

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
        return row or self.store.db.execute('SELECT *,1 AS passive FROM notes WHERE thread_id=? AND message_id=?', (tid, mid)).fetchone()

    def _status(self, tid: str, mid: str, digest: str) -> dict:
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
        result['receipt'] = json.loads(row['receipt']) if row['receipt'] else None
        result['deliveryState'] = 'context_persisted' if row['receipt'] else 'pending'
        if inbox['passive']:
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

    async def _changed(self, tid: str, mid: str) -> None:
        row = self._row(tid, mid)
        if row is None:
            return
        envelope = json.loads(row['envelope'])
        status = dump(self._status(tid, mid, envelope['semanticSha256']))
        if status != row['last_status']:
            self.store.db.execute('UPDATE external_inputs SET last_status=? WHERE thread_id=? AND message_id=?', (status, tid, mid))
            await self.server.engine.emit(tid, 'thread/externalInput/statusChanged', {'threadId': tid, 'messageId': mid})

    def _persisted(self, tid: str, mid: str, turn_id: str) -> None:
        row = self._row(tid, mid)
        if row is None or row['receipt'] is not None:
            return
        if getattr(self.server, 'busy_delivery', None):
            busy = self.store.db.execute('SELECT context_turn_id,offered FROM busy_inputs WHERE thread_id=? AND message_id=?', (tid, mid)).fetchone()
            if busy and busy[0] and not busy[1]:
                return
        # Receipt and its stable ordinal are allocated only on context acknowledgement.
        with self.store.transaction():
            ordinal = self.store.db.execute('SELECT count(*) FROM external_inputs WHERE thread_id=? AND receipt IS NOT NULL', (tid,)).fetchone()[0] + 1
            receipt = make_receipt(json.loads(row['envelope']), turn_id, ordinal)
            self.store.db.execute('UPDATE external_inputs SET receipt=? WHERE thread_id=? AND message_id=?', (dump(receipt), tid, mid))

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
        for row in rows:
            mid = row['message_id']
            inbox = self._inbox(tid, mid)
            if inbox is None:
                continue
            if turn_id and inbox['turn_id'] == turn_id:
                if acknowledged or (self.settings.backend != 'subscription' and inbox['state'] not in {'queued', 'pending'}):
                    self._persisted(tid, mid, turn_id)
                if output and not inbox['passive']:
                    self.store.db.execute('UPDATE external_inputs SET output_observed=1 WHERE thread_id=? AND message_id=?', (tid, mid))
            await self._changed(tid, mid)
        if method == 'turn/completed' and time.monotonic() - self._maintenance_at >= self.MAINTENANCE_INTERVAL_SECONDS:
            self.compact_history()
            self._maintenance_at = time.monotonic()

    async def dispatch(self, peer, method: str, params: dict):
        require(method in METHODS, 'unsupported native external input method')
        action = method.rsplit('/', 1)[-1]
        extra = {'submit': {'message', 'view', 'semanticSha256', 'admission'}, 'status': {'messages'},
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
                statuses.append(self._status(tid, text(key['messageId'], 'messageId', limit=256), _digest(key['semanticSha256'])))
            peer.subscriptions.add(tid)
            return {**self._response(tid), 'statuses': statuses}, None
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

    async def _submit(self, peer, tid: str, params: dict):
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
        metadata = {'nativeExternalInput': True}
        if isinstance(view, dict) and view.get('schema') == 'cutex.agent-message.v1':
            source_name = view['data'].get('senderName')
            if isinstance(source_name, str) and source_name.strip():
                metadata['sourceName'] = source_name[:256]
        if message['delivery'] == 'passive':
            metadata['deliveryMode'] = 'passive'
        if message['delivery'] == 'soon':
            metadata['deliveryMode'] = 'soon'
        payload = {'source': source, 'eventType': message['type'], 'text': message['text'], 'metadata': metadata}
        if view is not None:
            payload['view'] = view
        with self.store.transaction():
            enqueue = self.store.enqueue_note if message['delivery'] == 'passive' else self.store.enqueue
            receipt = enqueue(tid, mid, payload, self.settings.max_pending)
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
