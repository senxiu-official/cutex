"""Observed SDK task lifecycle, with native Codex SubAgentActivity projection.

SDK task identities are namespaced references, not independently resumable Codex
threads. Their originating adapter turn is captured once and never guessed from
whichever user turn happens to be active when a late notification arrives.
"""
from __future__ import annotations

from copy import deepcopy
import uuid

from .provider import ProviderError

TERMINAL = {'completed', 'failed', 'stopped', 'killed', 'cancelled'}
TYPES = {'TaskStartedMessage': 'task_started', 'TaskProgressMessage': 'task_progress',
         'TaskNotificationMessage': 'task_notification', 'TaskUpdatedMessage': 'task_updated'}


def field(message, key, default=None):
    value = message.get(key, default) if isinstance(message, dict) else getattr(message, key, default)
    if value is default:
        data = message.get('data', {}) if isinstance(message, dict) else getattr(message, 'data', {})
        if isinstance(data, dict):
            return data.get(key, default)
    return value


class RuntimeTasks:
    def __init__(self, engine, tid, sid):
        self.engine, self.tid, self.sid = engine, tid, sid
        self.rows = {}

    def snapshot(self):
        return {'tasks': deepcopy(list(self.rows.values())), 'source': 'observedSdkLifecycle',
                'completeInventory': False}

    def require_running(self, task_id):
        task = self.rows.get(task_id)
        if not task or task['status'] in TERMINAL or task['status'] == 'unknown':
            raise ProviderError('Task is not a known running task of this native session')

    def disconnected(self):
        for task in self.rows.values():
            if task['status'] not in TERMINAL:
                task['status'] = 'unknown'

    async def activity(self, row, kind):
        if not row.get('turnId') or row.get('taskType') not in {'local_agent', 'agent', 'subagent'}:
            return
        item = {'type': 'subAgentActivity', 'id': str(uuid.uuid4()), 'kind': kind,
                'agentThreadId': f'claude:{self.sid}:task:{row["taskId"]}',
                'agentPath': f'/claude/{row["taskId"]}'}
        self.engine.store.put_item(row['turnId'], item)
        # Native event_mapping.rs emits a completed event for *every* activity,
        # including kind=started. This is an occurrence, not a mutable task row.
        await self.engine.emit(self.tid, 'item/completed', {
            'threadId': self.tid, 'turnId': row['turnId'], 'item': item})

    async def _transcript_seen(self, agent_id, turn_id, call_id=None):
        subagents = getattr(self.engine, 'subagents', None)
        if subagents is not None and turn_id:
            await subagents.agent_seen(self.tid, self.sid, turn_id, agent_id, call_id)

    async def child_started(self, agent_id, turn_id):
        await self._child_started(agent_id, turn_id)
        # After the activity item: it names the transcript reference first.
        await self._transcript_seen(agent_id, turn_id)

    async def _child_started(self, agent_id, turn_id):
        if agent_id not in self.rows:
            self.rows[agent_id] = {'taskId': agent_id, 'turnId': turn_id,
                                  'taskType': 'local_agent', 'status': 'running'}
            await self.activity(self.rows[agent_id], 'started')
        elif self.rows[agent_id]['status'] in TERMINAL | {'unknown'}:
            # Agent(resume=...) reuses the native child identity. A new start
            # hook is affirmative evidence for the new originating turn.
            row = self.rows[agent_id]
            row.update(turnId=turn_id, status='running')
            await self.activity(row, 'interacted')

    async def child_stopped(self, agent_id):
        row = self.rows.get(agent_id)
        if row and row['status'] not in TERMINAL:
            row['status'] = 'completed'
            await self.activity(row, 'completed')

    async def shell_started(self, task_id, turn_id, call_id, arguments):
        if not isinstance(task_id, str) or not task_id or len(task_id) > 256:
            return
        row = self.rows.setdefault(task_id, {'taskId': task_id, 'turnId': turn_id, 'status': 'running'})
        row.update(taskType='local_bash', toolUseId=call_id, itemId=call_id,
                   command=str(arguments.get('command', ''))[:8192],
                   cwd=str(self.engine.settings.workspace), processId=None)
        await self.engine.emit(self.tid, 'claude/task/updated', {'threadId': self.tid, 'task': deepcopy(row)})

    async def observe(self, message, active_turn_id):
        typ = message.get('type', '') if isinstance(message, dict) else type(message).__name__
        subtype = TYPES.get(typ, field(message, 'subtype'))
        if subtype not in TYPES.values():
            return False
        task_id = field(message, 'task_id')
        if not isinstance(task_id, str) or not task_id or len(task_id) > 256:
            return False
        row = self.rows.get(task_id)
        first = row is None
        if first:
            # Only a start frame has evidence tying a task to the current turn.
            row = {'taskId': task_id, 'turnId': active_turn_id if subtype == 'task_started' else None,
                   'taskType': field(message, 'task_type'), 'status': 'running'}
            self.rows[task_id] = row
        if subtype == 'task_started':
            description = field(message, 'description')
            if isinstance(description, str):
                row['description'] = description[:2048]
            call_id = field(message, 'tool_use_id')
            if isinstance(call_id, str):
                row['toolUseId'] = call_id[:256]
                owner = getattr(self.engine.driver, 'owners', {}).get(self.tid)
                call = owner.bridge.native.calls.get(call_id) if owner and owner.bridge else None
                if call and call[1] == 'Bash':
                    await self.shell_started(task_id, call[0][0], call_id, call[2])
        previous = row['status']
        if previous == 'unknown' and subtype in {'task_started', 'task_progress'}:
            row['status'] = 'running'
        patch = field(message, 'patch', {})
        patch = patch if isinstance(patch, dict) else {}
        status = field(message, 'status') or patch.get('status')
        corrected_failure = previous == 'completed' and status in {'failed', 'stopped', 'killed', 'cancelled'}
        if status in TERMINAL | {'running', 'pending'}:
            # Never reopen a terminal task due to an out-of-order progress frame.
            # SubagentStop has no success flag; a later authoritative task
            # notification may refine its apparent completion into a failure.
            if previous not in TERMINAL or corrected_failure:
                row['status'] = status
        usage = field(message, 'usage')
        if isinstance(usage, dict):
            row['usage'] = {key: value for key, value in usage.items()
                            if key in {'total_tokens', 'tool_uses', 'duration_ms'} and type(value) is int and value >= 0}
        if subtype == 'task_started' and first:
            await self.activity(row, 'started')
        if subtype == 'task_started' and row.get('toolUseId') and row.get('taskType') in {'local_agent', 'agent', 'subagent'}:
            await self._transcript_seen(task_id, row.get('turnId'), row['toolUseId'])
        elif row['status'] in TERMINAL and (previous not in TERMINAL or corrected_failure):
            await self.activity(row, 'completed' if row['status'] == 'completed' else 'interrupted')
        elif subtype == 'task_progress' and previous not in TERMINAL:
            await self.activity(row, 'interacted')
        # Deliberately omit summary, output_file and arbitrary patch fields from
        # public diagnostics; the full text belongs to native session history.
        await self.engine.emit(self.tid, 'claude/task/updated', {'threadId': self.tid, 'task': deepcopy(row)})
        return True
