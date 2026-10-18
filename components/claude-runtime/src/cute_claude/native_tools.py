"""Observe SDK-owned tools; never execute or replay their effects in the adapter.

Claude Code's built-in tools are projected onto the Codex v2 item types that
app-server frontends already render. The mapping is presentation only; the
official runtime still decides, runs and records every tool call:

- ``Bash`` -> ``commandExecution``; ``Read``/``Grep``/``Glob``/``LS`` ->
  ``commandExecution`` whose ``commandActions`` say read / search / listFiles
- ``Edit``/``MultiEdit``/``Write``/``NotebookEdit`` -> ``fileChange`` plus an
  aggregated per-turn ``turn/diff/updated``
- ``WebSearch`` -> ``webSearch``
- ``TodoWrite`` and the ``Task*`` checklist tools -> ``turn/plan/updated``
  (no timeline item, like Codex ``update_plan``)
- anything else -> ``dynamicToolCall`` in namespace ``claude``
"""
import asyncio
from copy import deepcopy
import difflib
import json
import os
from pathlib import Path
import re
import time

from .safety_grants import BLOCKED_EVENT, UPDATED_EVENT
from . import permission_rules
from .approvals import is_allowed

# These require lifecycle/UI contracts beyond a main-thread native tool call.
# The second group can start native turns on their own (wake-ups, schedules,
# monitors, workflows) or move the session out of its workspace; such turns
# would have no adapter turn to belong to.
# The third group is Claude Code's cross-session messaging: it reaches other
# Claude sessions of the same OS user (every cutex Claude agent shares one
# config dir, so one session registry) without cutex's project boundary,
# identity or approvals. Agents talk through the cutex tools only. The real
# switch is CLAUDE_CODE_HARBOR_KITE=0 in the subscription env (it also closes
# the inbox); this list is defence in depth. Foreground Agent/Task delegation is
# enabled; SendMessage stays disabled because it also addresses peer sessions.
UNSUPPORTED = ['EnterPlanMode', 'ExitPlanMode',
               'ScheduleWakeup', 'CronCreate', 'CronDelete', 'CronList', 'RemoteTrigger',
               'Monitor', 'Workflow', 'EnterWorktree', 'ExitWorktree',
               'SendMessage', 'ListAgents', 'SendFile']

COMMAND_TOOLS = {'Bash', 'Read', 'Grep', 'Glob', 'LS'}
EDIT_TOOLS = {'Edit', 'MultiEdit', 'Write', 'NotebookEdit'}
TODO_TOOL = 'TodoWrite'
TASK_TOOLS = {'TaskCreate', 'TaskUpdate', 'TaskList', 'TaskGet'}
PLAN_TOOLS = {TODO_TOOL} | TASK_TOOLS

OUTPUT_BYTES = 65536          # aggregatedOutput shown per command
SNAPSHOT_BYTES = 2 * 1024 * 1024  # files larger than this are not diffed
TURN_DIFF_BYTES = 4 * 1024 * 1024


def exclusions(settings):
    result = list(UNSUPPORTED)
    if not settings.allow_write:
        result += ['Write', 'Edit', 'MultiEdit', 'NotebookEdit']
    if not settings.allow_exec:
        # PowerShell: Claude Code's Windows shell tool next to Bash.
        result += ['Bash', 'PowerShell']
    # Tools the owner turned off on a host-selected MCP server (`disabled_tools`).
    for server, rule in sorted(getattr(settings, 'mcp_tool_filters', {}).items()):
        result += [f'mcp__{server}__{tool}' for tool in rule.get('disabled') or []]
    return result


CONNECTOR_PREFIX = 'claude_ai_'


def external_mcp_tool(settings, full_name):
    """(server, tool) when ``full_name`` is a tool of a host-selected external MCP
    server or (when enabled) a claude.ai connector; None for the in-process
    cutex server and anything else. Claude Code names MCP tools
    ``mcp__<server>__<tool>``; host server names are already in its normalized
    alphabet, so the longest matching configured name wins."""
    if not isinstance(full_name, str) or not full_name.startswith('mcp__'):
        return None
    rest = full_name[5:]
    best = None
    for name in getattr(settings, 'mcp_servers', {}):
        if rest.startswith(name + '__') and len(rest) > len(name) + 2 and (best is None or len(name) > len(best)):
            best = name
    if best is not None:
        return best, rest[len(best) + 2:]
    if getattr(settings, 'claude_ai_connectors', False) and rest.startswith(CONNECTOR_PREFIX) and '__' in rest[len(CONNECTOR_PREFIX):]:
        server, tool = rest.rsplit('__', 1)
        return server, tool
    return None


def external_mcp_allowed(settings, server, tool):
    """The server's `enabled_tools` / `disabled_tools` (connectors: no filters)."""
    rule = getattr(settings, 'mcp_tool_filters', {}).get(server) or {}
    enabled = rule.get('enabled')
    return (enabled is None or tool in enabled) and tool not in (rule.get('disabled') or [])


def preview(value, budget=16384):
    """Bound presentation only; full arguments/results stay in native history."""
    remaining = budget
    def visit(v, depth=0):
        nonlocal remaining
        if remaining <= 0 or depth > 6:
            return '[preview truncated]'
        if isinstance(v, str):
            raw = v.encode('utf-8'); shown = raw[:remaining].decode('utf-8', 'ignore')
            remaining -= min(len(raw), remaining)
            return shown + (' [preview truncated]' if len(raw) > len(shown.encode('utf-8')) else '')
        if isinstance(v, dict):
            return {str(k)[:80]: ('[binary data omitted]' if k in {'data','base64'} else visit(x, depth+1))
                    for k,x in list(v.items())[:32]}
        if isinstance(v, list):
            return [visit(x, depth+1) for x in v[:32]]
        return v if v is None or isinstance(v, (bool,int,float)) else str(type(v).__name__)
    return visit(value)


def bounded(text, limit=OUTPUT_BYTES):
    """Keep the head and the tail of a long output (the tail matters for commands)."""
    raw = text.encode('utf-8')
    if len(raw) <= limit:
        return text
    half = limit // 2
    return (raw[:half].decode('utf-8', 'ignore') + '\n[… output truncated …]\n'
            + raw[-half:].decode('utf-8', 'ignore'))


# Permission denials the adapter itself answers (never Claude Code's "The user doesn't want…"): say
# what happened, and blame the owner only for the owner's own answer.
STALE_PERMISSION = ('cutex runtime: this permission request no longer matches a live tool call of the running '
                    'turn (the turn ended or the runtime is restarting). Nobody refused it; the tool did not run.')
UNANSWERED_PERMISSION = ('cutex runtime: nobody answered this permission request (no owner client was connected, '
                         'or the request was withdrawn). The owner did not refuse it; the tool did not run. Tell the '
                         'owner what you need it for and wait, or continue without it.')
OWNER_DECLINED = 'The owner declined this tool call in cutex. Do not retry it without new instructions.'
OWNER_STOPPED = 'The owner declined this tool call in cutex and stopped the turn. Wait for the owner.'


def interrupted_item(item, note):
    """Close an in-progress tool item whose outcome is unknown (interrupt, crash)."""
    kind = item.get('type')
    if kind == 'commandExecution':
        item.update(status='failed', exitCode=None,
                    aggregatedOutput=(item.get('aggregatedOutput') or '') + ('\n' if item.get('aggregatedOutput') else '') + note)
    elif kind == 'fileChange':
        item['status'] = 'failed'
    elif kind == 'mcpToolCall':
        item.update(status='failed', error={'message': note})
    elif kind == 'dynamicToolCall':
        item.update(status='failed', success=False, contentItems=[{'type': 'inputText', 'text': note}])
    return item


def plan_status(value):
    return {'pending': 'pending', 'in_progress': 'inProgress', 'completed': 'completed'}.get(value, 'pending')


def field(obj, key, default=None):
    return obj.get(key, default) if isinstance(obj, dict) else getattr(obj, key, default)


def text_of(value):
    """Native tool results arrive as a string, MCP-style blocks or structured JSON."""
    if value is None:
        return ''
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        parts = []
        for block in value:
            if isinstance(block, dict) and block.get('type') == 'text' and isinstance(block.get('text'), str):
                parts.append(block['text'])
            elif isinstance(block, str):
                parts.append(block)
            else:
                parts.append(json.dumps(preview(block), ensure_ascii=False))
        return '\n'.join(parts)
    return json.dumps(preview(value), ensure_ascii=False)


def command_spec(name, arguments, workspace):
    """Display command and Codex commandActions for a command-like native tool."""
    if name == 'Bash':
        command = str(arguments.get('command', ''))
        return command, [{'type': 'unknown', 'command': command}]
    if name == 'Read':
        path = str(arguments.get('file_path', ''))
        command = f'Read {path}'
        return command, [{'type': 'read', 'command': command, 'name': Path(path).name or path, 'path': path}]
    if name == 'Grep':
        pattern, path = str(arguments.get('pattern', '')), arguments.get('path')
        command = f'Grep {json.dumps(pattern, ensure_ascii=False)}' + (f' in {path}' if path else '')
        return command, [{'type': 'search', 'command': command, 'query': pattern, 'path': path if isinstance(path, str) else None}]
    pattern = str(arguments.get('pattern', arguments.get('path', '')))
    path = arguments.get('path') if name == 'Glob' else arguments.get('path', str(workspace))
    command = f'{name} {pattern}' + (f' in {path}' if name == 'Glob' and path else '')
    return command, [{'type': 'listFiles', 'command': command, 'path': path if isinstance(path, str) else None}]


def command_output(name, response):
    """aggregatedOutput for a successful command-like tool."""
    if name == 'Bash' and isinstance(response, dict):
        parts = [response.get('stdout') or '', response.get('stderr') or '']
        text = '\n'.join(p for p in parts if isinstance(p, str) and p)
        if response.get('backgroundTaskId'):
            text += f"\n[running in background as task {response['backgroundTaskId']}]"
        return text
    if name == 'Read' and isinstance(response, dict):
        file = response.get('file')
        if isinstance(file, dict) and isinstance(file.get('content'), str):
            return file['content']
    if name in {'Grep', 'Glob', 'LS'} and isinstance(response, dict):
        if isinstance(response.get('content'), str) and response['content']:
            return response['content']
        names = response.get('filenames')
        if isinstance(names, list):
            return '\n'.join(str(n) for n in names[:2000])
    return text_of(response)


def exit_code(error_text):
    match = re.match(r'\s*Exit code (\d+)', error_text or '')
    return int(match.group(1)) if match else 1


def read_snapshot(path):
    """File text for diffs: None if absent, False if binary/oversized/unreadable."""
    try:
        info = os.stat(path)
    except FileNotFoundError:
        return None
    except OSError:
        return False
    if not os.path.isfile(path) or info.st_size > SNAPSHOT_BYTES:
        return False
    try:
        with open(path, 'rb') as handle:
            data = handle.read(SNAPSHOT_BYTES + 1)
        return data.decode('utf-8')
    except (OSError, UnicodeDecodeError):
        return False


def unified(old, new, fromfile='', tofile=''):
    """Unified diff text with LF line ends (CRLF files diff by content)."""
    return ''.join(line + '\n' for line in difflib.unified_diff(
        old.splitlines(), new.splitlines(), fromfile=fromfile, tofile=tofile, lineterm='', n=3))


def hunks(old, new):
    """Unified-diff hunks (no file headers), as Codex `fileChange` update diffs."""
    return ''.join(line + '\n' for line in unified(old, new).splitlines()[2:])


def structured_patch_diff(patch):
    if not isinstance(patch, list):
        return None
    out = []
    for hunk in patch:
        if not isinstance(hunk, dict) or not isinstance(hunk.get('lines'), list):
            return None
        out.append(f"@@ -{hunk.get('oldStart', 0)},{hunk.get('oldLines', 0)} +{hunk.get('newStart', 0)},{hunk.get('newLines', 0)} @@\n")
        out.extend(str(line) + '\n' for line in hunk['lines'])
    return ''.join(out)


def apply_edits(text, edits):
    for edit in edits:
        old, new = edit.get('old_string'), edit.get('new_string')
        if not isinstance(old, str) or not isinstance(new, str) or (old and old not in text):
            return None
        text = text.replace(old, new) if edit.get('replace_all') else text.replace(old, new, 1)
    return text


def proposed_change(name, arguments):
    """The change a not-yet-run edit tool asks for, computed against the current file."""
    if name == 'NotebookEdit':
        path = str(arguments.get('notebook_path', ''))
        source = str(arguments.get('new_source', ''))
        mode = arguments.get('edit_mode') or 'replace'
        sign = '-' if mode == 'delete' else '+'
        diff = ''.join(f'{sign}{line}\n' for line in source.splitlines())
        return {'path': path, 'kind': {'type': 'update', 'move_path': None}, 'diff': diff}
    path = str(arguments.get('file_path', ''))
    current = read_snapshot(path)
    if name == 'Write':
        content = str(arguments.get('content', ''))
        if current is None:
            return {'path': path, 'kind': {'type': 'add'}, 'diff': content}
        diff = hunks(current, content) if isinstance(current, str) else ''
        return {'path': path, 'kind': {'type': 'update', 'move_path': None}, 'diff': diff}
    edits = arguments.get('edits') if name == 'MultiEdit' else [arguments]
    edits = edits if isinstance(edits, list) else []
    updated = apply_edits(current, edits) if isinstance(current, str) else None
    if updated is not None:
        diff = hunks(current, updated)
    else:
        diff = ''.join(''.join(f'-{line}\n' for line in str(e.get('old_string', '')).splitlines())
                       + ''.join(f'+{line}\n' for line in str(e.get('new_string', '')).splitlines())
                       for e in edits if isinstance(e, dict))
    return {'path': path, 'kind': {'type': 'update', 'move_path': None}, 'diff': diff}


def applied_change(name, arguments, response, proposed):
    """The change as reported by the tool result (authoritative when present)."""
    if name in {'Edit', 'MultiEdit', 'Write'} and isinstance(response, dict):
        path = response.get('filePath') or proposed['path']
        if name == 'Write' and response.get('type') == 'create':
            content = response.get('content') if isinstance(response.get('content'), str) else str(arguments.get('content', ''))
            return {'path': path, 'kind': {'type': 'add'}, 'diff': content}
        diff = structured_patch_diff(response.get('structuredPatch'))
        if diff is not None:
            return {'path': path, 'kind': {'type': 'update', 'move_path': None}, 'diff': diff}
    return proposed


class PlanState:
    """The thread's checklist: the last TodoWrite list or the Task* task list."""

    def __init__(self, store):
        self.store = store
        store.db.execute('CREATE TABLE IF NOT EXISTS thread_plan_state(thread_id TEXT PRIMARY KEY REFERENCES threads(id), state TEXT NOT NULL)')

    def load(self, tid):
        row = self.store.db.execute('SELECT state FROM thread_plan_state WHERE thread_id=?', (tid,)).fetchone()
        state = json.loads(row[0]) if row else {}
        return {'source': state.get('source'), 'todos': state.get('todos') or [], 'tasks': state.get('tasks') or []}

    def save(self, tid, state):
        self.store.db.execute('INSERT OR REPLACE INTO thread_plan_state VALUES(?,?)', (tid, json.dumps(state, ensure_ascii=False)))

    @staticmethod
    def steps(state):
        if state['source'] == 'tasks':
            return [{'step': str(t.get('subject', ''))[:1000], 'status': plan_status(t.get('status'))} for t in state['tasks']]
        return [{'step': str(t.get('content', ''))[:1000], 'status': plan_status(t.get('status'))} for t in state['todos']]

    def apply(self, tid, name, arguments, response):
        """Fold one successful checklist tool call; returns the plan or None if unchanged."""
        state = self.load(tid)
        if name == TODO_TOOL:
            todos = response.get('newTodos') if isinstance(response, dict) else None
            if not isinstance(todos, list):
                todos = arguments.get('todos')
            if not isinstance(todos, list):
                return None
            state = {'source': 'todos', 'todos': [t for t in todos if isinstance(t, dict)][:200], 'tasks': state['tasks']}
        elif name == 'TaskCreate':
            task = response.get('task') if isinstance(response, dict) else None
            if not isinstance(task, dict) or not isinstance(task.get('id'), (str, int)):
                return None
            state['tasks'] = [t for t in state['tasks'] if t.get('id') != str(task['id'])][:199] + [
                {'id': str(task['id']), 'subject': task.get('subject') or arguments.get('subject') or '', 'status': 'pending'}]
            state['source'] = 'tasks'
        elif name == 'TaskUpdate':
            task_id = str(arguments.get('taskId', ''))
            if isinstance(response, dict) and response.get('success') is False:
                return None
            status = arguments.get('status')
            if status == 'deleted':
                state['tasks'] = [t for t in state['tasks'] if t.get('id') != task_id]
            else:
                for task in state['tasks']:
                    if task.get('id') == task_id:
                        if isinstance(status, str):
                            task['status'] = status
                        if isinstance(arguments.get('subject'), str):
                            task['subject'] = arguments['subject']
            state['source'] = 'tasks'
        elif name == 'TaskList':
            tasks = response.get('tasks') if isinstance(response, dict) else None
            if not isinstance(tasks, list):
                return None
            state['tasks'] = [{'id': str(t.get('id')), 'subject': t.get('subject') or '', 'status': t.get('status')}
                              for t in tasks if isinstance(t, dict)][:200]
            state['source'] = 'tasks'
        else:
            return None
        self.save(tid, state)
        return self.steps(state)


class TurnDiff:
    """Per-turn aggregated unified diff of the files Claude's edit tools touched."""

    def __init__(self, workspace):
        self.workspace = Path(workspace)
        self.before = {}

    def track(self, path):
        if path and path not in self.before:
            self.before[path] = read_snapshot(path)

    def label(self, path):
        try:
            return Path(path).resolve().relative_to(self.workspace.resolve()).as_posix()
        except (ValueError, OSError):
            return Path(path).as_posix().lstrip('/')

    def render(self):
        out, used = [], 0
        for path, before in self.before.items():
            after = read_snapshot(path)
            if before == after:
                continue
            name = self.label(path)
            header = f'diff --git a/{name} b/{name}\n'
            if before is False or after is False:
                chunk = header + f'Binary or large file {name} changed\n'
            else:
                old_name = '/dev/null' if before is None else f'a/{name}'
                new_name = '/dev/null' if after is None else f'b/{name}'
                chunk = header + unified(before or '', after or '', old_name, new_name)
            if used + len(chunk) > TURN_DIFF_BYTES:
                out.append(f'[turn diff truncated after {len(out)} files]\n')
                break
            out.append(chunk); used += len(chunk)
        return ''.join(out)


BYPASS_SAFETY_HINT = ('Full access does not cover this Claude Code safety check. Rewrite the command in a form '
                      'the check can verify: a literal path, or variables guarded with ${NAME:?}, and no rm '
                      'inside sh -c. If this exact command is truly necessary, do not retry it and do not work '
                      'around the check: stop, explain to the owner what the command is and why it is needed, '
                      'then end the turn. The owner has been shown this command and may approve it once.')


def bypass_safety_denial(reason):
    reason = reason.strip() if isinstance(reason, str) and reason.strip() else 'Claude Code asked for approval of this tool call.'
    return f'{reason}\n\n{BYPASS_SAFETY_HINT}'


class NativeTools:
    def __init__(self, bridge):
        self.bridge = bridge
        self.calls = {}
        self.seen = set()
        self.turn_diff = None

    def begin_turn(self):
        self.turn_diff = TurnDiff(self.bridge.engine.settings.workspace)

    def item_for(self, name, arguments, call_id):
        b = self.bridge
        if name in COMMAND_TOOLS:
            command, actions = command_spec(name, arguments, b.engine.settings.workspace)
            return {'type': 'commandExecution', 'id': call_id, 'command': command,
                    'cwd': str(b.engine.settings.workspace), 'processId': None, 'source': 'agent',
                    'status': 'inProgress', 'commandActions': actions, 'aggregatedOutput': None,
                    'exitCode': None, 'durationMs': None, 'pluginId': None, 'scriptPath': None}
        if name in EDIT_TOOLS:
            return {'type': 'fileChange', 'id': call_id, 'changes': [proposed_change(name, arguments)],
                    'status': 'inProgress'}
        external = external_mcp_tool(b.engine.settings, name)
        if external:
            return {'type': 'mcpToolCall', 'id': call_id, 'server': external[0], 'tool': external[1],
                    'arguments': preview(arguments), 'status': 'inProgress', 'result': None, 'error': None,
                    'appContext': None, 'pluginId': None, 'readOnlyHint': None, 'durationMs': None}
        if name == 'WebSearch':
            query = str(arguments.get('query', ''))
            return {'type': 'webSearch', 'id': call_id, 'query': query,
                    'action': {'type': 'search', 'query': query, 'queries': None}, 'results': None}
        return {'type': 'dynamicToolCall', 'id': call_id, 'namespace': 'claude', 'tool': name,
                'arguments': preview(arguments), 'status': 'inProgress', 'contentItems': None,
                'success': None, 'durationMs': None}

    async def start(self, name, arguments, call_id, *, observation=False):
        b = self.bridge
        external = external_mcp_tool(b.engine.settings, name)
        if not observation and (name in exclusions(b.engine.settings) or (name.startswith('mcp__') and not external)):
            return b.denied('Tool is not enabled by this runtime configuration')
        if external and not external_mcp_allowed(b.engine.settings, *external):
            return b.denied('This MCP tool is turned off for this agent')
        if not isinstance(call_id, str) or not call_id or not isinstance(arguments, dict):
            return b.denied('Native tool call omitted its identity or arguments')
        if not b.accepting or not b.binding:
            return {}
        if call_id in self.seen:
            return {}  # SDK owns retries; observation never authorizes an execution.
        self.seen.add(call_id)
        binding = b.binding
        if name in PLAN_TOOLS:
            # Checklist tools render as turn/plan/updated, not as a timeline row.
            self.calls[call_id] = (binding, name, deepcopy(arguments), None, time.monotonic())
            return {}
        if name in EDIT_TOOLS and self.turn_diff is not None:
            self.turn_diff.track(str(arguments.get('notebook_path' if name == 'NotebookEdit' else 'file_path', '')))
        item = self.item_for(name, arguments, call_id)
        self.calls[call_id] = (binding, name, deepcopy(arguments), item, time.monotonic())
        b.engine.open_tools.setdefault(binding[0], {})[call_id] = item
        b.engine.store.put_item(binding[0], item)
        await b.engine.emit(b.tid, 'item/started', {'threadId':b.tid,'turnId':binding[0],'item':item})
        # No allow decision: native permission/plan policy remains authoritative.
        return {}

    async def approve(self, name, arguments, context):
        """Ask the frontend about a native permission request. Returns a decision dict:
        ``{'allow': bool, 'session': bool, 'interrupt': bool, 'message'?: str}``.

        Under full access (``bypassPermissions``) Claude Code still calls ``can_use_tool`` for the
        safety checks it never lets permission rules skip (an ``rm`` on a possibly empty variable,
        an unparsable ``sh -c``). Those never wait for a human there: they are declined with the
        reason and a hint, so the model rewrites the command and the turn never blocks. The
        decline is recorded as a safety block (``thread/safetyCheck/blocked``) the owner may
        grant once (``safety_grants``); a live grant for exactly this call allows it."""
        b = self.bridge
        call_id = getattr(context, 'tool_use_id', None)
        call = self.calls.get(call_id)
        if not call or not b.accepting or call[:3] != (b.binding, name, arguments):
            return {'allow': False, 'session': False, 'interrupt': False, 'message': STALE_PERMISSION}
        item = call[3]
        reason = getattr(context, 'title', None) or getattr(context, 'decision_reason', None)
        reason = reason if isinstance(reason, str) and reason.strip() else None
        approvals = b.engine.permissions
        if approvals.effective_mode(b.tid) == 'bypassPermissions':
            safety = getattr(b.engine, 'safety_grants', None)
            used = safety.consume(b.tid, name, arguments, call_id) if safety is not None else None
            if used is not None:
                # The owner granted exactly this call once: it runs, the grant is gone.
                await b.engine.emit(b.tid, UPDATED_EVENT, {'threadId': b.tid, 'block': used})
                return {'allow': True, 'session': False, 'interrupt': False}
            message = bypass_safety_denial(reason)
            await self.finish(call_id, message, False, declined=True)
            if safety is not None:
                command = item['command'] if item is not None and item['type'] == 'commandExecution' else (
                    f"{name} {json.dumps(preview(arguments, 2048), ensure_ascii=False)}")
                block = safety.record(b.tid, call[0][0], call_id, name, arguments, command,
                                      reason or 'Claude Code asked for approval of this tool call.')
                await b.engine.emit(b.tid, BLOCKED_EVENT, {'threadId': b.tid, 'turnId': call[0][0],
                                                          'itemId': call_id, 'block': block})
            return {'allow': False, 'session': False, 'interrupt': False, 'message': message}
        # "Always allow in this project" (cutex keeps the chosen rules per workspace).
        rules = permission_rules.offered(name, arguments, context, b.engine.settings.workspace)
        if item is not None and item['type'] == 'fileChange':
            request = approvals.request_file_change(b.tid, call[0][0], call_id, reason, rules)
        else:
            command = item['command'] if item is not None and item['type'] == 'commandExecution' else (
                f"{name} {json.dumps(preview(arguments, 2048), ensure_ascii=False)}")
            request = approvals.request_command(b.tid, call[0][0], call_id, command, reason, rules)
        task = asyncio.create_task(request)
        b.host_tasks.add(task)
        try:
            decision = await task
        finally:
            b.host_tasks.discard(task)
        allowed = is_allowed(decision)
        if not allowed:
            await self.finish(call_id, 'Permission declined.' if decision in {'decline', 'cancel'}
                              else 'No approval received.', False, declined=True)
        kept = list(decision[1]) if isinstance(decision, tuple) else None
        # What the model reads: only an owner's answer may say the owner refused.
        message = None if allowed else (OWNER_STOPPED if decision == 'cancel' else OWNER_DECLINED
                                        if decision == 'decline' else UNANSWERED_PERMISSION)
        return {'allow': allowed, 'session': decision == 'acceptForSession', 'interrupt': decision == 'cancel',
                **({'rules': kept} if kept else {}), **({'message': message} if message else {})}

    async def finish(self, call_id, result, success, *, declined=False):
        call = self.calls.pop(call_id, None)
        b = self.bridge
        if not call or call[0] != b.binding or not b.accepting:
            return
        binding, name, arguments, item, started = call
        duration = int((time.monotonic() - started) * 1000)
        if name in {'Agent', 'Task'} and getattr(b.engine, 'subagents', None) is not None:
            await b.engine.subagents.finished(call_id, success, declined=declined)
        if name == 'Bash' and success and isinstance(result, dict):
            task_id = result.get('backgroundTaskId') or result.get('task_id') or result.get('background_task_id')
            owner = getattr(b.engine.driver, 'owners', {}).get(b.tid)
            if task_id and owner:
                await owner.tasks.shell_started(task_id, binding[0], call_id, arguments)
        if item is None:
            if success:
                plan = b.engine.plan_state.apply(b.tid, name, arguments, result)
                if plan is not None:
                    await b.engine.emit(b.tid, 'turn/plan/updated', {'threadId': b.tid, 'turnId': binding[0],
                                                                     'explanation': None, 'plan': plan})
            return
        kind = item['type']
        if kind == 'commandExecution':
            if success:
                output, code = command_output(name, result), 0
                if name == 'Bash' and isinstance(result, dict) and result.get('interrupted'):
                    code = None
            else:
                output = text_of(result)
                code = None if declined else (exit_code(output) if name == 'Bash' else 1)
            # Reads and searches only need a preview; command output keeps more.
            limit = OUTPUT_BYTES if name == 'Bash' else 16384
            item.update(status='completed' if success else ('declined' if declined else 'failed'),
                        aggregatedOutput=bounded(output, limit), exitCode=code, durationMs=duration)
        elif kind == 'fileChange':
            if success:
                item['changes'] = [applied_change(name, arguments, result, item['changes'][0])]
            item['status'] = 'completed' if success else ('declined' if declined else 'failed')
        elif kind == 'webSearch':
            if isinstance(result, dict) and isinstance(result.get('results'), list):
                item['results'] = preview(result['results'])
        elif kind == 'mcpToolCall':
            if success:
                content = result if isinstance(result, list) else [{'type': 'text', 'text': text_of(result)}]
                item.update(status='completed', result={'content': preview(content), 'structuredContent': None},
                            error=None, durationMs=duration)
            else:
                item.update(status='failed', result=None, error={'message': bounded(text_of(result), 16384)},
                            durationMs=duration)
        else:
            text = json.dumps(preview(result), ensure_ascii=False) if not isinstance(result, str) else preview(result)
            item.update(status='completed' if success else 'failed', success=success,
                        contentItems=[{'type':'inputText','text':text}], durationMs=duration)
        b.engine.store.put_item(binding[0], item)
        b.engine.open_tools.get(binding[0], {}).pop(call_id, None)
        await b.engine.emit(b.tid, 'item/completed', {'threadId':b.tid,'turnId':binding[0],'item':item})
        if kind == 'fileChange' and success and self.turn_diff is not None:
            diff = self.turn_diff.render()
            await b.engine.emit(b.tid, 'turn/diff/updated', {'threadId': b.tid, 'turnId': binding[0], 'diff': diff})

    async def post(self, data, tool_use_id, context):
        b = self.bridge
        call_id = tool_use_id or data.get('tool_use_id')
        if not b.callback_valid(data, call_id):
            return {}
        failed = data.get('hook_event_name') == 'PostToolUseFailure'
        await self.finish(call_id,
                          data.get('error') if failed else data.get('tool_response'), not failed)
        return {}

    async def message(self, message):
        # Some native validation failures never reach execution hooks. Observe
        # transcript blocks too, without performing or authorizing their effects.
        content=field(message,'content',[])
        if not isinstance(content,list): return
        for block in content:
            kind=field(block,'type',type(block).__name__)
            if kind in {'ToolUseBlock','tool_use'}:
                name=field(block,'name','')
                if name and name!='AskUserQuestion' and (not name.startswith('mcp__') or external_mcp_tool(self.bridge.engine.settings,name)):
                    await self.start(name,field(block,'input',{}),field(block,'id'),observation=True)
            elif kind in {'ToolResultBlock','tool_result'}:
                await self.finish(field(block,'tool_use_id'),field(block,'content'),not field(block,'is_error',False))
