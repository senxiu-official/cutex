"""Read-only transcripts of Agent/Task subagents (P2, `features.subagentTranscripts`).

Claude Code forwards a subagent's messages on the parent's stream with
``parent_tool_use_id`` = the spawning Agent/Task ``tool_use`` id (with
``forward_subagent_text`` also its text and thinking). They never become the
parent's reply (sdk_runtime / subscription drop them there); this module keeps
a bounded, presentation-only copy per subagent so a frontend can open it:

- storage: ``subagent_runs`` (one row per spawning call) and ``subagent_items``
  in the adapter state, so a transcript survives an adapter restart (both carry
  ``thread_id``, so a thread delete removes them);
- identity: the namespaced reference the timeline already shows in
  ``subAgentActivity.agentThreadId`` (``claude:<native session>:task:<id>``,
  runtime_tasks.py). The SDK task / child agent id is linked to the spawning
  call from the task-start frame, from a child tool call seen both in the
  stream and in a hook, or (unambiguous only) from SubagentStart;
- reads: ``thread/read`` on such a reference answers a Codex v2 ``Thread`` whose
  ``parentThreadId`` is the adapter thread (cutex verifies sub-agent lineage
  through it) with one turn holding the items;
- live: ``item/started`` / ``item/completed`` / ``turn/started`` /
  ``turn/completed`` with that reference as ``threadId``, sent to the parent
  thread's subscribers only (never stored as parent events, never observed by
  the parent's input bookkeeping).

Background subagents (claude-r23, ``input.backgroundSubagents``): a run spawned
with ``run_in_background`` (``background = 1``) stays open after its turn ends;
it closes on the SDK task's terminal status (``background_ended``). Its stream
messages keep arriving while no adapter turn runs and land here as well. The
model usage of its messages (by API message id) is summed per run for the
origin turn (``take_usage``). A run left open by an earlier adapter process is
reported lost to the parent on the next ``thread/resume`` (``lost_notices``).

Bounds: ``MAX_ITEMS`` items per subagent (later ones are dropped, the run is
marked truncated), ``MAX_RUNS`` subagents per thread (oldest pruned), text
blocks cut at ``TEXT_BYTES``; tool items carry the same bounded previews as the
parent timeline. Full transcripts stay in Claude Code's own session files.
"""
from __future__ import annotations

from copy import deepcopy
import json
import time
from types import SimpleNamespace
from typing import Any

from .native_tools import PLAN_TOOLS, NativeTools, bounded, interrupted_item, text_of
from .store import dump

MAX_ITEMS = 400
MAX_RUNS = 100
TEXT_BYTES = 65536
REF_PREFIX = 'claude:'
TERMINAL = {'completed', 'failed', 'interrupted'}
USAGE_KEYS = {'input_tokens', 'output_tokens', 'cache_creation_input_tokens', 'cache_read_input_tokens'}


def field(obj: Any, key: str, default: Any = None) -> Any:
    return obj.get(key, default) if isinstance(obj, dict) else getattr(obj, key, default)


def block_kind(block: Any) -> str:
    kind = field(block, 'type')
    if isinstance(kind, str):
        return {'text': 'TextBlock', 'thinking': 'ThinkingBlock', 'tool_use': 'ToolUseBlock',
                'tool_result': 'ToolResultBlock', 'redacted_thinking': 'RedactedThinkingBlock'}.get(kind, kind)
    return type(block).__name__


def reference(sid: str, key: str) -> str:
    return f'{REF_PREFIX}{sid}:task:{key}'


def parse_reference(value: Any) -> tuple[str, str] | None:
    """(native session id, task/call key) of ``claude:<sid>:task:<key>``."""
    if not isinstance(value, str) or not value.startswith(REF_PREFIX) or len(value) > 600:
        return None
    sid, sep, key = value[len(REF_PREFIX):].partition(':task:')
    return (sid, key) if sep and sid and key else None


class SubagentTranscripts:
    def __init__(self, engine):
        self.engine, self.store = engine, engine.store
        db = self.store.db
        db.execute('''CREATE TABLE IF NOT EXISTS subagent_runs(
            call_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, session_id TEXT NOT NULL,
            turn_id TEXT NOT NULL, task_id TEXT, agent_type TEXT, description TEXT, prompt TEXT,
            status TEXT NOT NULL, truncated INTEGER NOT NULL DEFAULT 0,
            started REAL NOT NULL, completed REAL)''')
        db.execute('CREATE INDEX IF NOT EXISTS subagent_runs_task ON subagent_runs(session_id,task_id)')
        db.execute('CREATE INDEX IF NOT EXISTS subagent_runs_thread ON subagent_runs(thread_id,started)')
        if 'background' not in {row[1] for row in db.execute('PRAGMA table_info(subagent_runs)')}:
            db.execute('ALTER TABLE subagent_runs ADD COLUMN background INTEGER NOT NULL DEFAULT 0')
        db.execute('''CREATE TABLE IF NOT EXISTS subagent_items(
            seq INTEGER PRIMARY KEY AUTOINCREMENT, call_id TEXT NOT NULL, item_id TEXT NOT NULL,
            thread_id TEXT NOT NULL, payload TEXT NOT NULL, UNIQUE(call_id,item_id))''')
        # A run still open from an earlier adapter process lost its child.
        with self.store.transaction():
            lost = db.execute("SELECT * FROM subagent_runs WHERE status='running'").fetchall()
            for row in lost:
                self._close_items(row['call_id'], 'Adapter restarted while the subagent was running.')
            db.execute("UPDATE subagent_runs SET status='interrupted',completed=? WHERE status='running'", (time.time(),))
        self.lost_notices([row for row in lost if row['background'] and row['task_id']])
        # In-memory correlation (per adapter process): child tool id -> spawning call id
        # (stream) and child tool id -> child agent id (hooks), plus child agent ids
        # whose spawning call is not known yet.
        self.child_calls: dict[str, str] = {}
        self.child_agents: dict[str, str] = {}
        self.unlinked: dict[str, tuple[str, str, str]] = {}  # agent id -> (tid, sid, turn id)
        # Background runs: call id -> {API message id: usage counts} (max per message:
        # Claude Code repeats a message's usage on each of its content blocks).
        self.usage: dict[str, dict[str, dict]] = {}

    def lost_notices(self, rows) -> None:
        """Background runs an earlier adapter process left open: the parent hears
        of them once a client resumes its thread (``Store.recovered_events``)."""
        from .runtime_tasks import notice
        for row in rows:
            task = {'taskId': row['task_id'], 'turnId': row['turn_id'], 'taskType': 'local_agent',
                    'status': 'lost', 'background': True, 'toolUseId': row['call_id']}
            if row['description']:
                task['description'] = row['description']
            task['notifyParent'] = notice(task, 'lost', None, transcript=self.thread_ref(row),
                                          reason='The Claude runtime restarted while it ran.')
            params = self.store.stamp_event('claude/task/updated', {'threadId': row['thread_id'], 'task': task})
            self.store.event(row['thread_id'], 'claude/task/updated', params)
            self.store.recovered_events.setdefault(row['thread_id'], []).append(('claude/task/updated', params))

    # ----- identity -------------------------------------------------------

    def _row(self, call_id: str):
        return self.store.db.execute('SELECT * FROM subagent_runs WHERE call_id=?', (call_id,)).fetchone()

    def thread_ref(self, row) -> str:
        return reference(row['session_id'], row['task_id'] or row['call_id'])

    def resolve(self, ref: Any):
        """The run behind a ``claude:<sid>:task:<key>`` reference, or None."""
        parsed = parse_reference(ref)
        if parsed is None:
            return None
        sid, key = parsed
        return self.store.db.execute('''SELECT * FROM subagent_runs WHERE session_id=? AND (task_id=? OR call_id=?)
            ORDER BY task_id=? DESC, started DESC LIMIT 1''', (sid, key, key, key)).fetchone()

    def pending_thread(self, ref: Any) -> tuple[str, str] | None:
        """(thread id, turn id) of a child agent seen by a hook but not yet linked."""
        parsed = parse_reference(ref)
        if parsed is None:
            return None
        entry = self.unlinked.get(parsed[1])
        return (entry[0], entry[2]) if entry and entry[1] == parsed[0] else None

    def is_reference(self, ref: Any) -> bool:
        return self.resolve(ref) is not None or self.pending_thread(ref) is not None

    # ----- lifecycle --------------------------------------------------------

    async def start(self, tid: str, sid: str, turn_id: str, call_id: str, arguments: dict, *, background: bool = False) -> None:
        """The main agent spawned a subagent (PreToolUse admitted the call)."""
        if not isinstance(call_id, str) or not call_id or len(call_id) > 256 or self._row(call_id) is not None:
            return
        text = lambda key, limit: str(arguments.get(key))[:limit] if isinstance(arguments.get(key), str) else None
        now = time.time()
        with self.store.transaction():
            self.store.db.execute('''INSERT INTO subagent_runs(call_id,thread_id,session_id,turn_id,agent_type,description,prompt,status,started,background)
                VALUES(?,?,?,?,?,?,?,'running',?,?)''', (call_id, tid, sid, turn_id, text('subagent_type', 200),
                                                       text('description', 500), text('prompt', TEXT_BYTES), now, int(background)))
            self._prune(tid)
        prompt = text('prompt', TEXT_BYTES)
        if prompt:
            await self._put(call_id, {'type': 'userMessage', 'id': f'{call_id}:prompt',
                                      'content': [{'type': 'text', 'text': prompt, 'text_elements': []}]}, live=False)

    def _prune(self, tid: str) -> None:
        old = self.store.db.execute('SELECT call_id FROM subagent_runs WHERE thread_id=? ORDER BY started DESC LIMIT -1 OFFSET ?',
                                    (tid, MAX_RUNS)).fetchall()
        for row in old:
            self.store.db.execute('DELETE FROM subagent_items WHERE call_id=?', (row[0],))
            self.store.db.execute('DELETE FROM subagent_runs WHERE call_id=?', (row[0],))
            self.usage.pop(row[0], None)

    async def link(self, call_id: Any, task_id: Any) -> None:
        """Ties the SDK task / child agent id to its spawning call (first link wins)."""
        if not isinstance(call_id, str) or not isinstance(task_id, str) or not task_id or len(task_id) > 256:
            return
        row = self._row(call_id)
        if row is None or (row['task_id'] and row['task_id'] != task_id):
            return
        self.unlinked.pop(task_id, None)
        if row['task_id'] == task_id:
            return
        self.store.db.execute('UPDATE subagent_runs SET task_id=? WHERE call_id=?', (task_id, call_id))
        row = self._row(call_id)
        await self._announce(row)

    async def _announce(self, row) -> None:
        """First live frames of a linked run, and its reference on the spawning tool card."""
        ref = self.thread_ref(row)
        await self._view(row, 'turn/started', {'threadId': ref, 'turn': self._turn(row, include_items=False)})
        for item in self.items(row['call_id']):
            await self._view(row, 'item/completed', {'threadId': ref, 'turnId': self._turn_id(row), 'item': item})
        if row['status'] in TERMINAL:
            await self._view(row, 'turn/completed', {'threadId': ref, 'turn': self._turn(row)})
        await self._mark_card(row)

    async def _mark_card(self, row) -> None:
        engine = self.engine
        open_tools = getattr(engine, 'open_tools', {}).get(row['turn_id'], {})
        item = open_tools.get(row['call_id'])
        stored = None
        if item is None:
            found = self.store.db.execute('SELECT payload FROM items WHERE id=? AND turn_id=?', (row['call_id'], row['turn_id'])).fetchone()
            stored = json.loads(found[0]) if found else None
        card = item if item is not None else stored
        if card is None or card.get('type') != 'dynamicToolCall':
            return
        ref = self.thread_ref(row)
        if card.get('agentThreadId') == ref:
            return
        card['agentThreadId'] = ref
        self.store.put_item(row['turn_id'], card)
        await engine.emit(row['thread_id'], 'item/completed' if card.get('status') != 'inProgress' else 'item/started',
                          {'threadId': row['thread_id'], 'turnId': row['turn_id'], 'item': deepcopy(card)})

    async def agent_seen(self, tid: str, sid: str, turn_id: str, agent_id: Any, call_id: Any = None) -> None:
        """A child agent / SDK task appeared (SubagentStart, task_started)."""
        if not isinstance(agent_id, str) or not agent_id or len(agent_id) > 256:
            return
        if call_id:
            await self.link(call_id, agent_id)
            return
        if self.store.db.execute('SELECT 1 FROM subagent_runs WHERE session_id=? AND task_id=?', (sid, agent_id)).fetchone():
            return
        self.unlinked[agent_id] = (tid, sid, turn_id)
        open_runs = self.store.db.execute('''SELECT call_id FROM subagent_runs WHERE thread_id=? AND turn_id=?
            AND status='running' AND task_id IS NULL''', (tid, turn_id)).fetchall()
        if len(open_runs) == 1 and sum(1 for entry in self.unlinked.values() if entry[0] == tid and entry[2] == turn_id) == 1:
            await self.link(open_runs[0][0], agent_id)

    async def child_tool(self, agent_id: Any, tool_call_id: Any) -> None:
        """A child agent's tool call reached PreToolUse (hooks carry agent_id)."""
        if not isinstance(agent_id, str) or not isinstance(tool_call_id, str):
            return
        parent = self.child_calls.get(tool_call_id)
        if parent is not None:
            await self.link(parent, agent_id)
        elif len(self.child_agents) < 4096:
            self.child_agents[tool_call_id] = agent_id

    async def finished(self, call_id: str, success: bool, *, declined: bool = False) -> None:
        """The spawning Agent/Task call completed (its result is the subagent's answer).

        A background spawn completes at launch ("Async agent launched"): its run
        stays open until the SDK task ends (``background_ended``)."""
        row = self._row(call_id) if isinstance(call_id, str) else None
        if row is None or row['status'] in TERMINAL or (row['background'] and success):
            return
        await self._close(row, 'completed' if success else ('interrupted' if declined else 'failed'))

    def set_background(self, call_id: Any, background: bool) -> None:
        """The task-start frame says whether the spawn really runs in the background."""
        if isinstance(call_id, str):
            self.store.db.execute("UPDATE subagent_runs SET background=? WHERE call_id=? AND status='running'",
                                  (int(background), call_id))

    async def background_ended(self, sid: str, task_id: str, status: str, note: str | None = None) -> None:
        """The SDK task of a background run reached a terminal status."""
        row = self.store.db.execute("SELECT * FROM subagent_runs WHERE session_id=? AND task_id=? AND background=1 "
                                    "AND status='running'", (sid, task_id)).fetchone()
        if row is not None:
            await self._close(row, {'completed': 'completed', 'failed': 'failed'}.get(status, 'interrupted'), note=note)

    def take_usage(self, sid: str, task_id: str) -> dict | None:
        """Summed model usage of a background run's messages (taken once)."""
        row = self.store.db.execute('SELECT call_id FROM subagent_runs WHERE session_id=? AND task_id=?',
                                    (sid, task_id)).fetchone()
        seen = self.usage.pop(row[0], None) if row else None
        if not seen:
            return None
        total: dict[str, int] = {}
        for counts in seen.values():
            for key, value in counts.items():
                total[key] = total.get(key, 0) + value
        return total

    async def turn_ended(self, tid: str, turn_id: str) -> None:
        # A linked background run outlives its turn; one never launched does not.
        rows = self.store.db.execute("""SELECT * FROM subagent_runs WHERE thread_id=? AND turn_id=? AND status='running'
            AND NOT (background=1 AND task_id IS NOT NULL)""", (tid, turn_id)).fetchall()
        for row in rows:
            await self._close(row, 'interrupted', note='The turn ended before the subagent finished.')
        for agent_id in [a for a, entry in self.unlinked.items() if entry[0] == tid and entry[2] == turn_id]:
            self.unlinked.pop(agent_id, None)
        if not rows:
            return
        self.child_calls.clear()
        self.child_agents.clear()

    async def _close(self, row, status: str, note: str | None = None) -> None:
        with self.store.transaction():
            closed = self._close_items(row['call_id'], note or 'The subagent ended before this tool call finished.')
            self.store.db.execute('UPDATE subagent_runs SET status=?,completed=? WHERE call_id=?', (status, time.time(), row['call_id']))
        row = self._row(row['call_id'])
        if row['task_id']:
            ref = self.thread_ref(row)
            for item in closed:
                await self._view(row, 'item/completed', {'threadId': ref, 'turnId': self._turn_id(row), 'item': item})
            await self._view(row, 'turn/completed', {'threadId': ref, 'turn': self._turn(row)})
        else:
            # Never linked: the tool card still opens it by the spawning call id.
            await self._mark_card(row)

    def _close_items(self, call_id: str, note: str) -> list[dict]:
        closed = []
        for item in self.items(call_id):
            if item.get('status') == 'inProgress':
                item = interrupted_item(item, note)
                self.store.db.execute('UPDATE subagent_items SET payload=? WHERE call_id=? AND item_id=?', (dump(item), call_id, item['id']))
                closed.append(item)
        return closed

    # ----- messages -----------------------------------------------------------

    async def observe(self, message: Any) -> bool:
        """A stream message with ``parent_tool_use_id``; True when it belonged to a known run."""
        call_id = field(message, 'parent_tool_use_id')
        row = self._row(call_id) if isinstance(call_id, str) and call_id else None
        if row is None:
            return False
        typ = field(message, 'type') if isinstance(message, dict) else type(message).__name__
        if typ not in {'assistant', 'AssistantMessage', 'user', 'UserMessage'}:
            return True
        content = field(message, 'content')
        if content is None and isinstance(field(message, 'message'), dict):
            content = message['message'].get('content')
        if not isinstance(content, list):
            return True
        if row['background'] and typ in {'assistant', 'AssistantMessage'}:
            self._note_usage(call_id, message)
        uuid = field(message, 'uuid')
        base = uuid if isinstance(uuid, str) and uuid else f'{call_id}:{self._count(call_id)}'
        for index, block in enumerate(content):
            kind = block_kind(block)
            if kind == 'TextBlock' and typ in {'assistant', 'AssistantMessage'}:
                text = field(block, 'text')
                if isinstance(text, str) and text.strip():
                    await self._put(call_id, {'type': 'agentMessage', 'id': f'{base}:{index}',
                                              'text': bounded(text, TEXT_BYTES), 'phase': None})
            elif kind == 'ThinkingBlock':
                text = field(block, 'thinking')
                summary = [bounded(text, TEXT_BYTES)] if isinstance(text, str) and text.strip() else []
                await self._put(call_id, {'type': 'reasoning', 'id': f'{base}:{index}', 'summary': summary, 'content': []})
            elif kind == 'ToolUseBlock':
                tool_id, name, arguments = field(block, 'id'), field(block, 'name'), field(block, 'input', {})
                if not isinstance(tool_id, str) or not tool_id or not isinstance(name, str):
                    continue
                if len(self.child_calls) < 4096:
                    self.child_calls[tool_id] = call_id
                agent = self.child_agents.pop(tool_id, None)
                if agent is not None:
                    await self.link(call_id, agent)
                if name in PLAN_TOOLS:
                    continue
                await self._put(call_id, self._tool_item(name, arguments if isinstance(arguments, dict) else {}, tool_id))
            elif kind == 'ToolResultBlock':
                tool_id = field(block, 'tool_use_id')
                if isinstance(tool_id, str):
                    await self._complete(call_id, tool_id, field(block, 'content'), not field(block, 'is_error', False))
        return True

    def _note_usage(self, call_id: str, message: Any) -> None:
        usage = field(message, 'usage')
        if usage is None and isinstance(field(message, 'message'), dict):
            usage = message['message'].get('usage')
        key = field(message, 'message_id') or field(message, 'uuid')
        if not isinstance(usage, dict) or not isinstance(key, str):
            return
        counts = {k: v for k, v in usage.items() if k in USAGE_KEYS and type(v) is int and v >= 0}
        seen = self.usage.setdefault(call_id, {})
        if counts and (key in seen or len(seen) < 4096):
            old = seen.get(key, {})
            seen[key] = {k: max(v, old.get(k, 0)) for k, v in counts.items()}

    def _tool_item(self, name: str, arguments: dict, tool_id: str) -> dict:
        # The parent timeline's projection (presentation only; nothing runs here).
        return NativeTools(SimpleNamespace(engine=self.engine)).item_for(name, arguments, tool_id)

    async def _complete(self, call_id: str, tool_id: str, content: Any, success: bool) -> None:
        found = self.store.db.execute('SELECT payload FROM subagent_items WHERE call_id=? AND item_id=?', (call_id, tool_id)).fetchone()
        if found is None:
            return
        item = json.loads(found[0])
        output = text_of(content)
        kind = item.get('type')
        status = 'completed' if success else 'failed'
        if kind == 'commandExecution':
            item.update(status=status, aggregatedOutput=bounded(output, 16384), exitCode=0 if success else 1)
        elif kind == 'fileChange':
            item['status'] = status
        elif kind == 'mcpToolCall':
            if success:
                item.update(status=status, result={'content': [{'type': 'text', 'text': bounded(output, 16384)}], 'structuredContent': None}, error=None)
            else:
                item.update(status=status, result=None, error={'message': bounded(output, 16384)})
        elif kind == 'webSearch':
            pass
        else:
            item.update(status=status, success=success, contentItems=[{'type': 'inputText', 'text': bounded(output, 16384)}])
        await self._put(call_id, item, update=True)

    def _count(self, call_id: str) -> int:
        return self.store.db.execute('SELECT count(*) FROM subagent_items WHERE call_id=?', (call_id,)).fetchone()[0]

    async def _put(self, call_id: str, item: dict, *, update: bool = False, live: bool = True) -> None:
        row = self._row(call_id)
        if row is None:
            return
        if not update:
            if self.store.db.execute('SELECT 1 FROM subagent_items WHERE call_id=? AND item_id=?', (call_id, item['id'])).fetchone():
                return
            if self._count(call_id) >= MAX_ITEMS:
                if not row['truncated']:
                    self.store.db.execute('UPDATE subagent_runs SET truncated=1 WHERE call_id=?', (call_id,))
                return
            self.store.db.execute('INSERT INTO subagent_items(call_id,item_id,thread_id,payload) VALUES(?,?,?,?)',
                                  (call_id, item['id'], row['thread_id'], dump(item)))
        else:
            self.store.db.execute('UPDATE subagent_items SET payload=? WHERE call_id=? AND item_id=?', (dump(item), call_id, item['id']))
        if live and row['task_id']:
            in_progress = item.get('status') == 'inProgress'
            await self._view(row, 'item/started' if in_progress else 'item/completed',
                             {'threadId': self.thread_ref(row), 'turnId': self._turn_id(row), 'item': deepcopy(item)})

    async def _view(self, row, method: str, params: dict) -> None:
        notify = getattr(self.engine, 'notify_view', None)
        if notify is not None:
            await notify(row['thread_id'], method, self.store.stamp_event(method, params))

    # ----- reads --------------------------------------------------------------

    def items(self, call_id: str) -> list[dict]:
        return [json.loads(r[0]) for r in self.store.db.execute('SELECT payload FROM subagent_items WHERE call_id=? ORDER BY seq', (call_id,))]

    @staticmethod
    def _turn_id(row) -> str:
        return f'subagent:{row["call_id"]}'

    def _turn(self, row, include_items: bool = True) -> dict:
        status = {'running': 'inProgress', 'completed': 'completed', 'failed': 'failed'}.get(row['status'], 'interrupted')
        completed = row['completed']
        return {'id': self._turn_id(row), 'items': self.items(row['call_id']) if include_items else [],
                'itemsView': 'full' if include_items else 'notLoaded', 'status': status, 'error': None,
                'startedAt': int(row['started']), 'completedAt': int(completed) if completed else None,
                'durationMs': int((completed - row['started']) * 1000) if completed else None}

    def thread_wire(self, ref: str, include_turns: bool = False) -> dict | None:
        """Codex v2 ``Thread`` of a subagent reference, or None when unknown."""
        row = self.resolve(ref)
        pending = None if row is not None else self.pending_thread(ref)
        if row is None and pending is None:
            return None
        if row is not None:
            tid, running = row['thread_id'], row['status'] == 'running'
            name, role = row['description'], row['agent_type']
            created, updated = int(row['started']), int(row['completed'] or row['started'])
            turns = [self._turn(row)] if include_turns else []
            truncated = bool(row['truncated'])
        else:
            tid, running, name, role = pending[0], True, None, None
            created = updated = int(time.time())
            turns, truncated = [], False
        parent = self.store.thread(tid)
        spawn = {'parent_thread_id': tid, 'depth': 1, 'agent_nickname': name, 'agent_role': role}
        return {'id': ref, 'sessionId': tid, 'forkedFromId': None, 'parentThreadId': tid,
                'preview': (name or '')[:120], 'ephemeral': False, 'section': None, 'sectionEnteredAt': None,
                'projectId': None, 'historyMode': 'full', 'modelProvider': 'anthropic', 'model': parent['model'],
                'reasoningEffort': None, 'createdAt': created, 'updatedAt': updated, 'recencyAt': updated,
                'status': {'type': 'active', 'activeFlags': []} if running else {'type': 'idle'},
                'path': None, 'cwd': parent['cwd'], 'cliVersion': None, 'originator': 'cute-claude-app-server',
                'source': {'subAgent': {'thread_spawn': spawn}}, 'threadSource': None,
                'agentNickname': name, 'agentRole': role, 'gitInfo': None, 'name': name,
                'transcriptTruncated': truncated, 'turns': turns}
