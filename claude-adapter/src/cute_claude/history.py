"""Display-history pagination. Never reconstruct or modify model-visible history."""
import base64
import json

from . import __version__
from .errors import RpcError, require
from .store import dump
from .transport import MAX_RESPONSE

PROVIDER = "cute-claude"
PAGE_BYTES = 512 * 1024
# Leave ample room for the RPC envelope. A single item is indivisible on this wire.
ITEM_BYTES = MAX_RESPONSE - 64 * 1024
# `thread/read includeTurns` (Management bootstrap, input routing) returns the
# newest turns that fit; older turns are left to thread/turns/list paging.
FULL_READ_BYTES = 16 * 1024 * 1024


def encode(scope, key, inclusive=False):
    return base64.urlsafe_b64encode(dump([1, scope, key, inclusive]).encode()).decode()


def decode(raw, scope):
    if raw is None:
        return None, False
    try:
        require(isinstance(raw, str) and len(raw) < 2048, 'invalid history cursor')
        version, saved, key, inclusive = json.loads(base64.b64decode(raw, altchars=b'-_', validate=True))
        require(version == 1 and saved == scope and isinstance(inclusive, bool), 'history cursor scope mismatch')
        return key, inclusive
    except (ValueError, TypeError, UnicodeError) as exc:
        raise RpcError(-32602, 'invalid history cursor') from exc


def size(value):
    return len(dump(value).encode())


def thread_wire(store, history, tid, *, include_turns=False, effort=None, status=None):
    """Codex v2 `Thread` of one adapter thread (live server and offline reader)."""
    t = store.thread(tid)
    properties = store.thread_properties(tid)
    first = store.db.execute("SELECT payload FROM inputs WHERE thread_id=? AND state!='reverted' ORDER BY seq LIMIT 1", (tid,)).fetchone()
    return {"id": tid, "sessionId": tid, "forkedFromId": properties.get('forkedFromId'), "parentThreadId": None,
            "preview": json.loads(first[0])["text"][:120] if first else "", "ephemeral": False,
            "section": None, "sectionEnteredAt": None, "projectId": None, "historyMode": "paginated",
            "modelProvider": PROVIDER, "model": t["model"], "reasoningEffort": effort,
            "createdAt": int(t["created"]), "updatedAt": int(t["updated"]), "recencyAt": int(t["updated"]),
            "status": status if status is not None else {"type": "notLoaded"},
            "path": None, "cwd": t["cwd"], "cliVersion": __version__, "originator": "cute-claude-app-server",
            "source": "appServer", "threadSource": None, "agentNickname": None, "agentRole": None,
            "gitInfo": properties.get('gitInfo'), "name": t["name"], "turns": history.full_read(tid) if include_turns else []}


class History:
    def __init__(self, store, *, read_only=False):
        self.store = store
        if read_only:
            return
        # Keyset queries seek by stable order; no repeated full-history parsing.
        store.db.execute('CREATE INDEX IF NOT EXISTS history_turn_page ON turns(thread_id,started,id)')
        store.db.execute('CREATE INDEX IF NOT EXISTS history_item_page ON items(turn_id,seq)')
        store.db.execute('CREATE INDEX IF NOT EXISTS history_event_page ON events(thread_id,seq)')

    def page(self, tid, params, *, items):
        self.store.thread(tid)
        direction = params.get('sortDirection') or ('asc' if items else 'desc')
        require(direction in ('asc', 'desc'), 'invalid sortDirection')
        limit = params.get('limit') if params.get('limit') is not None else 100
        require(type(limit) is int and limit > 0, 'limit must be a positive integer')
        limit = min(limit, 200)
        turn_id = params.get('turnId') if items else None
        if turn_id is not None:
            require(isinstance(turn_id, str), 'invalid turnId')
            require(self.store.db.execute('SELECT 1 FROM turns WHERE id=? AND thread_id=? AND ' + self.store.visible_turns_filter(), (turn_id, tid)).fetchone() is not None, 'turn not found', -32004)
        scope = ['items' if items else 'turns', tid, turn_id]
        key, inclusive = decode(params.get('cursor'), scope)
        op = ('>' if direction == 'asc' else '<') + ('=' if inclusive else '')
        order = 'ASC' if direction == 'asc' else 'DESC'
        args = [tid]
        if items:
            sql = 'SELECT i.seq,i.turn_id,i.payload FROM items i JOIN turns t ON t.id=i.turn_id WHERE t.thread_id=? AND ' + self.store.visible_turns_filter('t.')
            if turn_id is not None:
                sql += ' AND i.turn_id=?'; args.append(turn_id)
            if key is not None:
                require(type(key) is int and key > 0, 'invalid item cursor')
                sql += f' AND i.seq {op} ?'; args.append(key)
            sql += f' ORDER BY i.seq {order} LIMIT ?'
        else:
            sql = 'SELECT id,started FROM turns WHERE thread_id=? AND ' + self.store.visible_turns_filter()
            if key is not None:
                require(isinstance(key, list) and len(key) == 2 and type(key[0]) in (int, float) and isinstance(key[1], str), 'invalid turn cursor')
                sql += f' AND (started,id) {op} (?,?)'; args.extend(key)
            sql += f' ORDER BY started {order},id {order} LIMIT ?'
        args.append(limit + 1)
        rows = self.store.db.execute(sql, args)
        data, keys, used, more = [], [], 0, False
        view = params.get('itemsView') or 'summary'
        require(items or view in ('notLoaded', 'summary', 'full'), 'invalid itemsView')
        for row in rows:
            if len(data) == limit:
                more = True; break
            if items:
                # Do not parse/duplicate an oversized individual tool output.
                require(len(row['payload'].encode()) <= ITEM_BYTES, 'single history item exceeds transport limit; inspect the local transcript', -32001)
                value = {'turnId': row['turn_id'], 'item': json.loads(row['payload'])}
                anchor = row['seq']
            else:
                value = self.store.turn(row['id'], include_items=False)
                anchor = [row['started'], row['id']]
                if view != 'notLoaded':
                    value['items'] = self.turn_items(row['id'], view)
                    value['itemsView'] = view
            cost = size(value)
            require(cost <= ITEM_BYTES, 'single history entry exceeds transport limit; use thread/items/list', -32001)
            if data and used + cost > PAGE_BYTES:
                more = True; break
            data.append(value); keys.append(anchor); used += cost
        return {'data': data, 'nextCursor': encode(scope, keys[-1]) if more else None,
                'backwardsCursor': encode(scope, keys[0], True) if keys else None}

    def turn_items(self, turn_id, view):
        if view == 'summary':
            # First user input and last assistant message, in chronological order.
            rows = self.store.db.execute('''SELECT payload FROM items WHERE seq IN (
                SELECT min(seq) FROM items WHERE turn_id=? AND json_extract(payload,'$.type')='userMessage'
                UNION SELECT max(seq) FROM items WHERE turn_id=? AND json_extract(payload,'$.type')='agentMessage') ORDER BY seq''', (turn_id, turn_id))
        else:
            rows = self.store.db.execute('SELECT payload FROM items WHERE turn_id=? ORDER BY seq', (turn_id,))
        result, used = [], 0
        for row in rows:
            used += len(row[0].encode())
            # One turn is one page entry; page() bounds whole pages.
            require(used <= ITEM_BYTES, 'turn detail exceeds the transport limit; use itemsView=notLoaded and thread/items/list', -32001)
            result.append(json.loads(row[0]))
        return result

    def turn_bytes(self, turn_id):
        row = self.store.db.execute('SELECT COALESCE(sum(length(CAST(payload AS BLOB))),0) FROM items WHERE turn_id=?', (turn_id,)).fetchone()
        return row[0]

    def full_read(self, tid):
        """Newest turns with all items, oldest first, within FULL_READ_BYTES.

        Older turns that do not fit are omitted (the thread is `historyMode:
        paginated`; page them with thread/turns/list). The newest turn is always
        present so its status is visible; when it alone is too large its items
        are `notLoaded`.
        """
        turns, used = [], 0
        for row in self.store.db.execute('SELECT id FROM turns WHERE thread_id=? AND ' + self.store.visible_turns_filter() + ' ORDER BY started DESC,id DESC', (tid,)):
            cost = self.turn_bytes(row[0])
            turn = self.store.turn(row[0], include_items=False)
            if used + cost > FULL_READ_BYTES:
                if not turns:
                    turn['itemsView'] = 'notLoaded'
                    turns.append(turn)
                break
            turn['items'] = self.turn_items(row[0], 'full')
            turn['itemsView'] = 'full'
            used += cost
            turns.append(turn)
        turns.reverse()
        return turns
