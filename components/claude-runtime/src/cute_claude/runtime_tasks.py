"""Observed SDK task lifecycle, with native Codex SubAgentActivity projection.

SDK task identities are namespaced references, not independently resumable Codex
threads. Their originating adapter turn is captured once and never guessed from
whichever user turn happens to be active when a late notification arrives.

Background tasks (claude-r23, ``input.backgroundSubagents``): a background
subagent (``local_agent``, ``is_backgrounded``) or a main-agent background shell
command (``local_bash``) that ends adds ``notifyParent`` to its
``claude/task/updated``; cutex turns it into one message to this agent (source
"Claude Task", deduplicated by ``noticeId``). Statuses: completed, failed,
stopped, lost (the Claude runtime restarted or closed while it ran). Shell
notices are rate limited: at most one per ``SHELL_NOTICE_SECONDS``, later ones
coalesced into the next.
"""
from __future__ import annotations

import asyncio
from copy import deepcopy
import sys
import time
import uuid

from .provider import ProviderError

TERMINAL = {'completed', 'failed', 'stopped', 'killed', 'cancelled', 'lost'}
AGENT_TYPES = {'local_agent', 'agent', 'subagent'}
SHELL_TYPES = {'local_bash', 'local_shell', 'bash'}
SUMMARY_CHARS = 4000
# A terminal task_updated / SubagentStop waits this long for the task_notification
# that carries the summary before the notice goes out without it.
NOTICE_GRACE_SECONDS = 3.0
SHELL_NOTICE_SECONDS = 60.0
SHELL_BATCH = 20


def notice_status(status):
    return {'completed': 'completed', 'failed': 'failed', 'lost': 'lost'}.get(status, 'stopped')


def notice(task, status, summary, *, transcript=None, reason=None, tasks=None, notice_id=None):
    """`notifyParent` of a background task that ended (bounded)."""
    text = summary if isinstance(summary, str) and summary.strip() else None
    result = {'noticeId': notice_id or task['taskId'], 'taskId': task['taskId'],
              'taskType': 'local_agent' if task.get('taskType') in AGENT_TYPES else 'local_bash',
              'status': notice_status(status), 'description': str(task.get('description') or task.get('command') or '')[:2048],
              'summary': text[:SUMMARY_CHARS] if text else None, 'summaryTruncated': bool(text and len(text) > SUMMARY_CHARS)}
    for key, value in (('originTurnId', task.get('turnId')), ('toolUseId', task.get('toolUseId')),
                       ('transcript', transcript), ('reason', reason)):
        if isinstance(value, str) and value:
            result[key] = value[:600]
    if tasks:
        result['tasks'] = tasks
    return result
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
        self.timers: dict[str, asyncio.TimerHandle] = {}
        # Shell notices: when the last one went out, and the ones waiting for the next.
        self.shell_sent: float | None = None
        self.shell_waiting: list[dict] = []
        self.shell_timer: asyncio.TimerHandle | None = None

    def snapshot(self):
        return {'tasks': [self._public(row) for row in self.rows.values()], 'source': 'observedSdkLifecycle',
                'completeInventory': False}

    def require_running(self, task_id):
        task = self.rows.get(task_id)
        if not task or task['status'] in TERMINAL or task['status'] == 'unknown':
            raise ProviderError('Task is not a known running task of this native session')

    def disconnected(self):
        for task in self.rows.values():
            if task['status'] not in TERMINAL | {'lost'}:
                task['status'] = 'unknown'

    async def activity(self, row, kind):
        if not row.get('turnId') or row.get('taskType') not in AGENT_TYPES:
            return
        if kind == 'interacted' and row.get('background'):
            return  # Progress of a run outliving its turn: claude/task/updated only.
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
            if row.get('background'):
                await self._ended(row)
                await self.engine.emit(self.tid, 'claude/task/updated', {'threadId': self.tid, 'task': self._public(row)})

    async def shell_started(self, task_id, turn_id, call_id, arguments):
        if not isinstance(task_id, str) or not task_id or len(task_id) > 256:
            return
        row = self.rows.setdefault(task_id, {'taskId': task_id, 'turnId': turn_id, 'status': 'running'})
        row.update(taskType='local_bash', toolUseId=call_id, itemId=call_id,
                   command=str(arguments.get('command', ''))[:8192],
                   cwd=str(self.engine.settings.workspace), processId=None)
        # A main-agent command started with run_in_background.
        if arguments.get('run_in_background') is True:
            row['background'] = True
        await self.engine.emit(self.tid, 'claude/task/updated', {'threadId': self.tid, 'task': self._public(row)})

    @staticmethod
    def _public(row):
        return {key: deepcopy(value) for key, value in row.items() if not key.startswith('_')}

    def _bridge(self):
        owner = getattr(self.engine.driver, 'owners', {}).get(self.tid)
        return owner.bridge if owner else None

    async def _ended(self, row, summary=None, *, final=False):
        """A background task reached a terminal status. Its notice goes out with the
        task_notification (summary), or after NOTICE_GRACE_SECONDS without one."""
        if not row.get('_closed'):
            row['_closed'] = True
            bridge = self._bridge()
            if bridge is not None:
                bridge.background.pop(row['taskId'], None)
            subagents = getattr(self.engine, 'subagents', None)
            if subagents is not None and row.get('taskType') in AGENT_TYPES:
                try:
                    await subagents.background_ended(self.sid, row['taskId'], notice_status(row['status']),
                                                     note=row.get('_reason'))
                except Exception as exc:  # Presentation only: never fail the reader.
                    print(f'claude runtime {self.tid}: background transcript not closed ({type(exc).__name__})', file=sys.stderr)
                await self._usage(row)
        if row.get('_notified') or row.get('ownedBySubagent'):
            return
        if not final:
            if row['taskId'] not in self.timers:
                loop = asyncio.get_running_loop()
                self.timers[row['taskId']] = loop.call_later(
                    NOTICE_GRACE_SECONDS, lambda: asyncio.ensure_future(self._late_notice(row['taskId'])))
            return
        timer = self.timers.pop(row['taskId'], None)
        if timer is not None:
            timer.cancel()
        row['_notified'] = True
        if row.get('taskType') in SHELL_TYPES:
            await self._shell_notice(row, summary)
            return
        subagents = getattr(self.engine, 'subagents', None)
        ref = f'claude:{self.sid}:task:{row["taskId"]}' if subagents is not None else None
        row['notifyParent'] = notice(row, row['status'], summary, transcript=ref, reason=row.get('_reason'))

    async def _late_notice(self, task_id):
        self.timers.pop(task_id, None)
        row = self.rows.get(task_id)
        if row is None or row.get('_notified'):
            return
        await self._ended(row, final=True)
        if row.get('notifyParent'):
            await self.engine.emit(self.tid, 'claude/task/updated', {'threadId': self.tid, 'task': self._public(row)})
            row.pop('notifyParent', None)

    async def _usage(self, row):
        """Background subagent usage is billed to its origin turn (best effort)."""
        subagents = getattr(self.engine, 'subagents', None)
        add = getattr(self.engine, 'add_task_usage', None)
        if subagents is None or add is None:
            return
        try:
            usage = subagents.take_usage(self.sid, row['taskId'])
            if usage and row.get('turnId'):
                await add(self.tid, row['turnId'], row['taskId'], usage)
        except Exception as exc:  # A late frame must never fail the reader.
            print(f'claude runtime {self.tid}: background usage not recorded ({type(exc).__name__})', file=sys.stderr)

    async def _shell_notice(self, row, summary):
        entry = {'taskId': row['taskId'], 'status': notice_status(row['status']),
                 'description': str(row.get('description') or row.get('command') or '')[:500]}
        if isinstance(summary, str) and summary.strip():
            entry['summary'] = summary[:500]
        now = time.monotonic()
        if self.shell_sent is None or now - self.shell_sent >= SHELL_NOTICE_SECONDS:
            self.shell_sent = now
            row['notifyParent'] = notice(row, row['status'], summary)
            return
        # Within the interval: coalesced into one notice when it ends.
        self.shell_waiting = (self.shell_waiting + [entry])[-SHELL_BATCH:]
        if self.shell_timer is None:
            self.shell_timer = asyncio.get_running_loop().call_later(
                SHELL_NOTICE_SECONDS - (now - self.shell_sent), lambda: asyncio.ensure_future(self._flush_shell()))

    async def _flush_shell(self):
        self.shell_timer = None
        waiting, self.shell_waiting = self.shell_waiting, []
        if not waiting:
            return
        self.shell_sent = time.monotonic()
        last = self.rows.get(waiting[-1]['taskId'], {'taskId': waiting[-1]['taskId'], 'taskType': 'local_bash'})
        statuses = {entry['status'] for entry in waiting}
        status = 'completed' if statuses == {'completed'} else ('failed' if 'failed' in statuses else 'stopped')
        description = f'{len(waiting)} background commands' if len(waiting) > 1 else waiting[0]['description']
        task = {**self._public(last), 'description': description}
        task['notifyParent'] = notice(task, status, waiting[0].get('summary') if len(waiting) == 1 else None,
                                      tasks=waiting if len(waiting) > 1 else None,
                                      notice_id=f'shells:{waiting[0]["taskId"]}' if len(waiting) > 1 else None)
        await self.engine.emit(self.tid, 'claude/task/updated', {'threadId': self.tid, 'task': task})

    async def lost(self, reason):
        """The Claude Code process ended with background tasks still running. The notice
        also waits for the next `thread/resume` (`Store.recovered_events`): cutex's
        observer may be reconnecting (a management restart); cutex queues it once."""
        for row in list(self.rows.values()):
            if row.get('background') and row['status'] not in TERMINAL and not row.get('ownedBySubagent'):
                row['status'] = 'lost'
                row['_reason'] = reason
                await self._ended(row, final=True)
                params = {'threadId': self.tid, 'task': self._public(row)}
                await self.engine.emit(self.tid, 'claude/task/updated', params)
                recovered = getattr(self.engine.store, 'recovered_events', None)
                if recovered is not None and row.get('notifyParent'):
                    recovered.setdefault(self.tid, []).append(
                        ('claude/task/updated', self.engine.store.stamp_event('claude/task/updated', deepcopy(params))))
                row.pop('notifyParent', None)
        for timer in self.timers.values():
            timer.cancel()
        self.timers.clear()

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
            if field(message, 'owned_by_subagent') is True:
                row['ownedBySubagent'] = True
            call_id = field(message, 'tool_use_id')
            if isinstance(call_id, str):
                row['toolUseId'] = call_id[:256]
                bridge = self._bridge()
                call = bridge.native.calls.get(call_id) if bridge else None
                if call and call[1] == 'Bash':
                    await self.shell_started(task_id, call[0][0], call_id, call[2])
                if bridge is not None and row.get('taskType') in AGENT_TYPES:
                    backgrounded = field(message, 'is_backgrounded')
                    origin = bridge.adopt_background(task_id, call_id, backgrounded is not False)
                    subagents = getattr(self.engine, 'subagents', None)
                    if origin is not None:
                        row['background'] = True
                        row['turnId'] = row.get('turnId') or origin[0]
                    if subagents is not None and backgrounded is False:
                        subagents.set_background(call_id, False)
            if field(message, 'is_backgrounded') is True and row.get('taskType') in SHELL_TYPES \
                    and not row.get('ownedBySubagent'):
                row['background'] = True
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
        if subtype == 'task_started' and row.get('toolUseId') and row.get('taskType') in AGENT_TYPES:
            await self._transcript_seen(task_id, row.get('turnId'), row['toolUseId'])
        elif row['status'] in TERMINAL and (previous not in TERMINAL or corrected_failure):
            await self.activity(row, 'completed' if row['status'] == 'completed' else 'interrupted')
        elif subtype == 'task_progress' and previous not in TERMINAL:
            await self.activity(row, 'interacted')
        if row.get('background') and row['status'] in TERMINAL:
            # The summary (the subagent's final reply, or the command's) only goes to
            # the parent, in notifyParent; never into the task row itself.
            notification = subtype == 'task_notification'
            await self._ended(row, field(message, 'summary') if notification else None, final=notification)
        # Deliberately omit summary, output_file and arbitrary patch fields from
        # public diagnostics; the full text belongs to native session history.
        await self.engine.emit(self.tid, 'claude/task/updated', {'threadId': self.tid, 'task': self._public(row)})
        row.pop('notifyParent', None)
        return True
