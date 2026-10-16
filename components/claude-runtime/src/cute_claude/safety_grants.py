"""Claude Code safety checks under full access: record, escalate, grant once.

Under ``bypassPermissions`` Claude Code still calls ``can_use_tool`` for the
safety checks permission rules never skip (an ``rm`` on a possibly empty
variable, an ``rm`` inside an unparsable ``sh -c``). The adapter declines them
at once (the turn never waits) and records a *safety block*: the exact tool
call and the CLI reason. ``thread/safetyCheck/blocked`` tells the clients;
cutex shows it to the owner as a "needs you" item.

The owner may grant one exact retry (``thread/safetyCheck/grant``): the next
``can_use_tool`` call on the same thread with the same tool and the same
significant input within :data:`GRANT_SECONDS` is allowed and uses the grant
up. Anything else is declined as before. Grants live in the adapter state
database, so they survive an adapter restart, and are never reusable.

Only an interactive controller (cutex Management, which relays the owner's
decision from its owner-only route, or the attached TUI) may grant or
dismiss, and it must say the owner decided (``decidedBy.kind == "owner"``).
"""
from __future__ import annotations

import hashlib
import json
import time

from .errors import RpcError, require, text
from .store import dump, identifier

GRANT_SECONDS = 30 * 60
METHODS = {'thread/safetyCheck/list', 'thread/safetyCheck/grant', 'thread/safetyCheck/dismiss'}
BLOCKED_EVENT = 'thread/safetyCheck/blocked'
UPDATED_EVENT = 'thread/safetyCheck/updated'
STATES = {'blocked', 'granted', 'used', 'dismissed', 'expired'}
# Presentation-only fields of a tool input; a retry may word them differently.
COSMETIC_KEYS = {'description'}
MAX_INPUT_BYTES = 65536


def significant_input(name: str, arguments: dict) -> dict:
    """The part of a tool input a grant binds to. ``Bash``: the command
    only (its description, timeout and background flag do not change what
    the safety check judged). Other tools: the whole input minus
    presentation fields."""
    if name == 'Bash':
        return {'command': arguments.get('command')}
    return {k: v for k, v in arguments.items() if k not in COSMETIC_KEYS}


def grant_key(name: str, arguments: dict) -> str:
    raw = dump({'tool': name, 'input': significant_input(name, arguments)})
    return hashlib.sha256(raw.encode('utf-8')).hexdigest()


def iso(seconds: float | None) -> str | None:
    if seconds is None:
        return None
    return time.strftime('%Y-%m-%dT%H:%M:%S', time.gmtime(seconds)) + f'.{int(seconds * 1000) % 1000:03d}Z'


class SafetyGrants:
    def __init__(self, server):
        self.server = server
        self.db = server.store.db
        self.db.execute('''CREATE TABLE IF NOT EXISTS safety_blocks(
            id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, turn_id TEXT, item_id TEXT,
            tool_name TEXT NOT NULL, tool_input TEXT NOT NULL, grant_key TEXT NOT NULL,
            command TEXT NOT NULL, reason TEXT NOT NULL, source TEXT,
            state TEXT NOT NULL, created REAL NOT NULL,
            decided REAL, decided_by TEXT, expires REAL, used REAL, used_call_id TEXT)''')
        self.db.execute('CREATE INDEX IF NOT EXISTS safety_blocks_grants ON safety_blocks(thread_id, grant_key, state)')

    # ---------------------------------------------------------------- wire

    def wire(self, row) -> dict:
        state = row['state']
        now = time.time()
        if state == 'granted' and row['expires'] is not None and row['expires'] <= now:
            state = 'expired'
        return {'blockId': row['id'], 'threadId': row['thread_id'], 'turnId': row['turn_id'],
                'itemId': row['item_id'], 'toolName': row['tool_name'],
                'toolInput': json.loads(row['tool_input']), 'command': row['command'],
                'reason': row['reason'], 'source': json.loads(row['source']) if row['source'] else None,
                'state': state, 'blockedAt': iso(row['created']), 'decidedAt': iso(row['decided']),
                'decidedBy': json.loads(row['decided_by']) if row['decided_by'] else None,
                'expiresAt': iso(row['expires']), 'usedAt': iso(row['used']),
                'grantSeconds': GRANT_SECONDS}

    def row(self, tid: str, block_id: str):
        row = self.db.execute('SELECT * FROM safety_blocks WHERE id=? AND thread_id=?', (block_id, tid)).fetchone()
        if row is None:
            raise RpcError(-32004, 'no such safety block on this thread', {'code': 'safety_block_not_found'})
        return row

    # ------------------------------------------------------------ runtime

    def record(self, tid: str, turn_id: str | None, item_id: str | None, name: str, arguments: dict,
               command: str, reason: str) -> dict:
        """A declined safety check: store it and return its wire form."""
        block_id = 'sb-' + identifier()
        source = self.server.store.turn_source(turn_id) if turn_id else None
        self.db.execute('INSERT INTO safety_blocks(id,thread_id,turn_id,item_id,tool_name,tool_input,grant_key,command,'
                        'reason,source,state,created) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)',
                        (block_id, tid, turn_id, item_id, name, dump(arguments), grant_key(name, arguments),
                         command, reason, dump(source) if source else None, 'blocked', time.time()))
        return self.wire(self.row(tid, block_id))

    def consume(self, tid: str, name: str, arguments: dict, call_id: str | None) -> dict | None:
        """Use up a live grant for exactly this call; the used block or None."""
        key, now = grant_key(name, arguments), time.time()
        with self.server.store.transaction():
            row = self.db.execute("SELECT * FROM safety_blocks WHERE thread_id=? AND grant_key=? AND state='granted' "
                                  'AND expires>? ORDER BY decided LIMIT 1', (tid, key, now)).fetchone()
            if row is None:
                return None
            self.db.execute("UPDATE safety_blocks SET state='used',used=?,used_call_id=? WHERE id=? AND state='granted'",
                            (now, call_id, row['id']))
        return self.wire(self.row(tid, row['id']))

    # ----------------------------------------------------------- protocol

    async def dispatch(self, peer, method: str, params: dict) -> dict:
        tid = text(params.get('threadId'), 'threadId', limit=256)
        self.server.store.thread(tid)
        if method == 'thread/safetyCheck/list':
            require(set(params) <= {'threadId', 'state'}, 'only threadId and state are accepted')
            state = params.get('state')
            require(state is None or state in STATES, 'unknown state')
            rows = self.db.execute('SELECT * FROM safety_blocks WHERE thread_id=? ORDER BY created DESC LIMIT 200', (tid,)).fetchall()
            blocks = [self.wire(r) for r in rows]
            return {'data': [b for b in blocks if state is None or b['state'] == state]}
        require(set(params) <= {'threadId', 'blockId', 'decidedBy'}, 'only threadId, blockId and decidedBy are accepted')
        block_id = text(params.get('blockId'), 'blockId', limit=128)
        decided_by = params.get('decidedBy')
        # Only a controller that relays human decisions, and only for the owner.
        if not peer.interactive or not isinstance(decided_by, dict) or decided_by.get('kind') != 'owner':
            raise RpcError(-32003, 'only the owner can decide on a safety block',
                           {'code': 'owner_required'})
        require(set(decided_by) <= {'kind', 'id'} and (decided_by.get('id') is None or isinstance(decided_by['id'], str)),
                'decidedBy must be {kind: "owner", id?}')
        with self.server.store.transaction():
            row = self.row(tid, block_id)
            if row['state'] != 'blocked':
                raise RpcError(-32009, f'safety block already {self.wire(row)["state"]}',
                               {'code': 'safety_block_closed', 'block': self.wire(row)})
            now = time.time()
            if method == 'thread/safetyCheck/grant':
                self.db.execute("UPDATE safety_blocks SET state='granted',decided=?,decided_by=?,expires=? WHERE id=?",
                                (now, dump(decided_by), now + GRANT_SECONDS, block_id))
            else:
                self.db.execute("UPDATE safety_blocks SET state='dismissed',decided=?,decided_by=? WHERE id=?",
                                (now, dump(decided_by), block_id))
        block = self.wire(self.row(tid, block_id))
        await self.server.engine.emit(tid, UPDATED_EVENT, {'threadId': tid, 'block': block})
        return {'block': block}
