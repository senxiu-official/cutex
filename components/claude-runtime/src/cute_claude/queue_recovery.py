"""Retain proved empty transport inputs; keep effect recovery separately guarded."""
import json
import random
import time

LIMIT = 3
WINDOW = 300.0
BASE_DELAY = 10.0
KEY = 'claudeQueueRecovery:'

def time_now():
    return time.time()

async def wait_backoff(event, delay):
    import asyncio
    try:
        await asyncio.wait_for(event.wait(), delay)
    except TimeoutError:
        pass


def transport_failure(error):
    if getattr(error, 'transport_failure', False):
        return True
    diagnostic = getattr(error, 'diagnostic', None)
    if getattr(error, 'native_code', None) not in {'server_error', 'unknown'}:
        return False
    return diagnostic is not None and (diagnostic.category in {'ECONNRESET', 'ENOTFOUND', 'ETIMEDOUT'}
        or diagnostic.category == 'overloaded' or diagnostic.http_status == 408
        or diagnostic.http_status is not None and 500 <= diagnostic.http_status <= 599)


def read(store, tid):
    row = store.db.execute('SELECT value FROM meta WHERE key=?', (KEY + tid,)).fetchone()
    return json.loads(row[0]) if row else None


def reset(store, tid):
    store.db.execute('DELETE FROM meta WHERE key=?', (KEY + tid,))


def plan(store, tid, error, *, tool_in_flight, now=None, empty=False):
    if tool_in_flight or not transport_failure(error):
        return None
    now = time_now() if now is None else now
    old = read(store, tid)
    if empty:
        probes = (old or {}).get('probeCount', 0) + 1
        started = (old or {}).get('outageStarted', (old or {}).get('windowStarted', now))
        started = started if type(started) in (int,float) and 0 <= started <= now else now
        base = (random.uniform(900,1800) if now-started >= 3600 else
                BASE_DELAY * (1, 3, 6)[min(probes - 1, 2)] if probes <= 3 else BASE_DELAY * random.uniform(12,30))
        retry_after = getattr(getattr(error, 'diagnostic', None), 'retry_after', None) or 0
        return {'count': 0, 'probeCount': probes, 'outageStarted': started, 'windowStarted': now, 'held': False,
                'notBefore': now + max(base, retry_after)}
    start = old['windowStarted'] if old and 0 <= now - old['windowStarted'] <= WINDOW else now
    count = old['count'] + 1 if old and start == old['windowStarted'] else 1
    retry_after = getattr(getattr(error, 'diagnostic', None), 'retry_after', None) or 0
    held = count >= LIMIT or retry_after > 60
    return {'count': count, 'windowStarted': start, 'held': held,
            'notBefore': now + max(BASE_DELAY * (1, 3, 6)[min(count - 1, 2)], retry_after)}


def remaining(store, tid):
    state = read(store, tid)
    return max(0.0, state['notBefore'] - time_now()) if state and not state['held'] else 0.0


class QueueRetryHeld(Exception):
    """A queued retry did not enter query; retain it for authorized Resume."""


def held_plan(store, tid):
    state = read(store, tid) or {'count': 0, 'windowStarted': time_now()}
    return {**state, 'held': True, 'notBefore': time_now()}



def owner_stamp(owner):
    if owner is None:
        return None
    pid, code = owner.process_observation()
    return [owner.connection_id, owner.native_connection_generation,
            owner.recovery_arrivals, owner.recovery_revision, id(owner.client),
            pid, code, owner.model_selection_id]


def writer(engine):
    if not hasattr(engine, '_queue_probe_writer'):
        import uuid
        engine._queue_probe_writer = str(uuid.uuid4())
    return engine._queue_probe_writer


def empty_receipt(engine, tid, turn_id, owner, *, terminal, error_uuid):
    sid = engine.session(tid)['session_id']
    row = engine.store.db.execute('SELECT message_id,seq,semantic_hash FROM inputs WHERE thread_id=? AND turn_id=?', (tid, turn_id)).fetchone()
    return {'input': list(row) if row else None, 'writer': writer(engine), 'threadId': tid, 'turnId': turn_id,
            'sessionId': sid, 'state': 'terminal-empty' if terminal else 'not-submitted',
            'owner': owner_stamp(owner) if terminal else None, 'errorUuid': error_uuid}


def receipt_matches(engine, tid, turn_id, receipt, owner):
    if not receipt or receipt.get('writer') != writer(engine) or receipt.get('threadId') != tid or receipt.get('turnId') != turn_id:
        return False
    row = engine.store.db.execute('SELECT message_id,seq,semantic_hash FROM inputs WHERE thread_id=? AND turn_id=?', (tid, turn_id)).fetchone()
    if row is None or receipt.get('input') != list(row):
        return False
    if receipt.get('sessionId') != engine.session(tid)['session_id']:
        return False
    if receipt.get('state') == 'not-submitted':
        row = engine.store.db.execute('SELECT value FROM meta WHERE key=?', ('claudeQueueRetry:' + turn_id,)).fetchone()
        return row is not None and json.loads(row[0]).get('dispatch') == 'not-submitted'
    try:
        return receipt.get('state') == 'terminal-empty' and receipt.get('owner') == owner_stamp(owner)
    except OSError:
        return False


def dispatch_probe(store, turn_id, state):
    row = store.db.execute('SELECT value FROM meta WHERE key=?', ('claudeQueueRetry:' + turn_id,)).fetchone()
    if row:
        retry = json.loads(row[0])
        retry['dispatch'] = state
        store.db.execute('UPDATE meta SET value=? WHERE key=?',
                         (json.dumps(retry), 'claudeQueueRetry:' + turn_id))


# Pinned CLI metadata attachments: catalog/context snapshots, never executions.
_METADATA = {
    'environment': {'snapshot':dict},
    'model': {'identity':dict, 'text':str},
    'deferred_tools_delta': {k:list for k in ('addedNames','addedLines','removedNames','wireHiddenNames','readdedNames','pendingMcpServers','needsAuthMcpServers','failedMcpServers')},
    'agent_listing_delta': {**{k:list for k in ('addedTypes','addedLines','builtInTypes','builtInPluginTypes','removedTypes')},'isInitial':bool,'showConcurrencyNote':bool},
    'skill_listing': {'content':str,'skillCount':int,'isInitial':bool,'names':list,'digests':list},
    'total_tokens_reminder': {'text':str},
    'session_context': {'context':dict},
    'date': {'date':str},
    'prompt_snapshot': {'systemPrompt':list,'tools':list,'cliPrefix':str,'reminderFold':bool,'echoWireToolInputs':bool,'contextRendering':str},
}


def empty_metadata(row):
    allowed = {'attachment','cwd','entrypoint','gitBranch','isSidechain','parentUuid','rendered','renderedRole','sessionId','timestamp','type','userType','uuid','version'}
    data = row.get('attachment')
    if not set(row) <= allowed or not isinstance(data,dict):
        return False
    if not isinstance(data.get('type'), str):
        return False
    schema = _METADATA.get(data['type'])
    optional = {'tools','cliPrefix'} if data.get('type')=='prompt_snapshot' else set()
    return (schema is not None and set(data) <= {'type',*schema}
            and set(schema)-optional <= set(data)
            and all(type(value) is schema[key] for key,value in data.items() if key!='type'))


def empty_history(owner, sid, error_uuid, anchor=None):
    """Exact empty current-input chain, not the tool-checkpoint empty-set test."""
    from pathlib import Path
    from .checkpoint_recovery import valid_uuid, digest, MAX_TRANSCRIPT_BYTES
    if not valid_uuid(error_uuid):
        return None
    paths = list((Path(owner.driver.config_dir) / 'projects').glob(f'*/{sid}.jsonl'))
    if len(paths) != 1 or paths[0].is_symlink():
        return None
    with paths[0].open('rb') as stream:
        size = stream.seek(0, 2)
        offset = max(0, size - MAX_TRANSCRIPT_BYTES)
        stream.seek(offset)
        raw = stream.read(MAX_TRANSCRIPT_BYTES + 1)
    if offset:
        raw = raw.partition(b'\n')[2]
    if not raw.endswith(b'\n'):
        return None
    entries, tip = {}, None
    for line in raw.splitlines():
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except (ValueError, TypeError):
            return None
        if not isinstance(row, dict):
            return None
        if not row.get('uuid'):
            continue
        if row.get('sessionId') != sid or not valid_uuid(row['uuid']) or row.get('parentUuid') is not None and not valid_uuid(row['parentUuid']):
            return None
        if row['uuid'] in entries and entries[row['uuid']] != row:
            return None
        entries[row['uuid']] = row
        tip = row['uuid']
    if tip != error_uuid:
        return None
    chain, seen, cursor = [], set(), error_uuid
    while cursor in entries:
        row = entries[cursor]
        if cursor in seen or row.get('isSidechain'):
            return None
        seen.add(cursor)
        if row.get('type') == 'attachment':
            if not empty_metadata(row):
                return None
            chain.append([cursor, digest(row)])
            cursor = row.get('parentUuid')
            continue
        if row.get('type') not in {'user','assistant'}:
            return None
        message = row.get('message')
        if not isinstance(message, dict):
            return None
        blocks = message.get('content', [])
        if isinstance(blocks, str):
            blocks = [{'type':'text', 'text':blocks}]
        if not isinstance(blocks, list) or any(not isinstance(b, dict) or b.get('type') not in {'text','image','document'} for b in blocks):
            return None
        if row['type'] == 'assistant' and row.get('isApiErrorMessage') is not True:
            return None
        chain.append([cursor, digest(row)])
        if row['type'] == 'user' and (anchor is None or cursor == anchor):
            return {'anchor': cursor, 'tip': error_uuid, 'sha256': digest(chain)}
        cursor = row.get('parentUuid')
    return None


def restart_candidate(store, tid, turn_id, retry):
    if not isinstance(retry, dict) or not isinstance(retry.get('receipt'), dict):
        return False
    receipt = retry['receipt']
    try:
        recovery = read(store, tid)
    except (ValueError, TypeError):
        return False
    row = store.db.execute('SELECT message_id,seq,semantic_hash FROM inputs WHERE thread_id=? AND turn_id=?', (tid,turn_id)).fetchone()
    if (not row or receipt.get('input') != list(row) or receipt.get('threadId') != tid or receipt.get('turnId') != turn_id
            or not isinstance(recovery, dict) or recovery.get('held') or type(recovery.get('count')) is not int or recovery['count'] != 0
            or type(recovery.get('probeCount')) is not int or recovery['probeCount'] < 1 or store.db.execute('SELECT 1 FROM tool_calls WHERE turn_id=? LIMIT 1', (turn_id,)).fetchone()):
        return False
    if receipt.get('state') == 'not-submitted':
        return retry.get('dispatch') == 'not-submitted' and not retry.get('continuation')
    return (receipt.get('state') == 'terminal-empty' and retry.get('dispatch') in {'terminal-empty','not-submitted'}
            and retry.get('continuation') is True and bool(receipt.get('history')))


async def verified_receipt(engine, tid, turn_id, receipt, owner):
    import asyncio
    if not receipt_matches(engine, tid, turn_id, receipt, owner):
        return False
    try:
        history = receipt.get('history')
        if history and await asyncio.to_thread(empty_history, owner, receipt['sessionId'], receipt['errorUuid'], history['anchor']) != history:
            return False
        return receipt_matches(engine, tid, turn_id, receipt, owner)
    except (OSError, ValueError, TypeError, KeyError):
        return False
