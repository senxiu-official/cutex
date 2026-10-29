"""Background subagents (claude-r23, `input.backgroundSubagents`); fake SDK, no model calls.

The replays use frames recorded from real Claude Code 2.1.301 / claude-agent-sdk
0.2.166 (fixtures/background_subagent_probe.json, from the cutex-desktop probe of
2026-10-21): a background subagent keeps running after its turn's Result, its
hooks carry its agent id, it asks can_use_tool for Write / MCP tools in default
mode, and its task ends with task_updated + task_notification (summary = its reply).
"""
from __future__ import annotations

import asyncio
import json
from pathlib import Path
from types import SimpleNamespace

from cute_claude import runtime_tasks
from cute_claude.subscription import (BACKGROUND_NEEDS_APPROVAL, BACKGROUND_NO_AGENTS, BACKGROUND_NO_HOST_TOOLS,
                                      BACKGROUND_NO_QUESTIONS)
from cute_claude.subagent_transcripts import SubagentTranscripts
from cute_claude.store import identifier
from test_persistent import ph, until  # noqa: F401  (fixture)
from test_questions_plan import begin

PROBE = json.loads((Path(__file__).parent / 'fixtures/background_subagent_probe.json').read_text())


def task_events(ph, tid):
    return [e['params']['task'] for e in ph.store.events(tid, 0, 10000) if e['method'] == 'claude/task/updated']


def notices(ph, tid):
    return [t['notifyParent'] for t in task_events(ph, tid) if t.get('notifyParent')]


def with_external_probe_server(ph):
    # The probe's subagent called an MCP tool of an SDK server named `probe`, and ran
    # Bash and Write (this harness turns write / exec off by default).
    object.__setattr__(ph.settings, 'mcp_servers', {'probe': {'type': 'stdio', 'command': 'probe'}})
    object.__setattr__(ph.settings, 'allow_write', True)
    object.__setattr__(ph.settings, 'allow_exec', True)


class Replay:
    """Feeds one probe recording through the real bridge, hooks, can_use_tool and owner."""

    def __init__(self, ph, tid, turn, bridge, script):
        self.ph, self.tid, self.turn, self.bridge = ph, tid, turn, bridge
        self.owner = ph.engine.driver.owners[tid]
        self.options = ph.sdk.clients[0].options
        self.script = script
        self.sid = self.probe_sid()
        self.decisions: dict[tuple[str, str], dict] = {}
        self.permissions: dict[str, object] = {}
        self.results = 0
        self.index = 0

    def probe_sid(self):
        return next(e['data']['session_id'] for e in self.script if e['kind'] == 'msg' and e['data'].get('session_id'))

    def fix(self, value):
        """The recorded native session id becomes this owner's."""
        return json.loads(json.dumps(value).replace(self.sid, self.owner.session_id))

    async def until(self, predicate=None):
        """Run events until `predicate(event)` (exclusive), or to the end."""
        while self.index < len(self.script):
            event = self.script[self.index]
            if predicate is not None and predicate(event):
                return event
            self.index += 1
            await self.step(event)
        return None

    async def step(self, event):
        kind, data = event['kind'], self.fix(event['data'])
        if kind == 'msg':
            if data.get('type') == 'result':
                self.results += 1
                if self.results == 1 and self.bridge.binding is not None:
                    # The origin turn's Result: the owner closes tool admission and ends the turn.
                    self.bridge.accepting = False
                    await self.bridge.end_turn()
                    self.ph.store.finish(self.tid, self.turn, 'completed', None)
                    return
            await self.owner._event(data)
        elif kind.startswith('hook:'):
            name = kind[5:]
            call = data.get('tool_use_id')
            if name == 'PreToolUse':
                self.decisions[(name, call)] = await self.bridge.pre_tool(data, call, {})
            elif name == 'PostToolUse':
                hook = self.options.hooks['PostToolUse'][0].hooks[0]
                self.decisions[(name, call)] = await hook(data, call, {})
            elif name == 'SubagentStart':
                await self.bridge.subagent_start(data, call, {})
            elif name == 'SubagentStop':
                await self.bridge.subagent_stop(data, call, {})
        elif kind == 'can_use_tool':
            context = SimpleNamespace(tool_use_id=data['tool_use_id'], agent_id=data['agent_id'], title=None,
                                      decision_reason=None, suggestions=[])
            self.permissions[data['name']] = await self.options.can_use_tool(data['name'], data['args'], context)

    def denied(self, call):
        decision = self.decisions[('PreToolUse', call)]
        return decision.get('hookSpecificOutput', {}).get('permissionDecision') == 'deny'


async def replay(ph, script_name, *, mode=None):
    with_external_probe_server(ph)
    tid, turn, bridge = await begin(ph)
    if mode:
        ph.store.db.execute('INSERT OR REPLACE INTO thread_tool_approvals VALUES(?,?)', (tid, mode))
    return Replay(ph, tid, turn, bridge, PROBE[script_name])


async def test_probe_background_subagent_runs_after_its_turn_and_notifies_the_parent(ph):
    run = await replay(ph, 'default', mode='default')
    await run.until(lambda e: e['kind'] == 'msg' and e['data'].get('type') == 'result')
    await run.step(run.script[run.index])  # The origin turn's Result.
    run.index += 1
    # A note left after the turn ended waits for the main agent, not for the child's hooks.
    ph.store.enqueue_note(run.tid, 'note-1', {'source': {'kind': 'agent', 'id': 'cutex.a'}, 'eventType': 'agent_message',
                                            'text': 'A NOTE', 'metadata': {'deliveryMode': 'passive'}}, 10)
    await run.until()
    agent_call = 'toolu_01HfZyjifjDUHnbjVdQtNn7c'
    task_id = 'aa335f43da4fb68e1'
    # The spawn (run_in_background: true) is admitted and the child adopted by its task-start frame.
    assert run.decisions[('PreToolUse', agent_call)] == {}
    # After the turn's Result every child tool reached PreToolUse and was admitted by agent id.
    child_pre = [call for (hook, call) in run.decisions if hook == 'PreToolUse' and call != agent_call]
    assert len(child_pre) == 7 and not any(run.denied(call) for call in child_pre)
    # Default mode: Claude Code asked for Write and the MCP call; nobody can answer a background child.
    for name in ('Write', 'mcp__probe__echo'):
        assert run.permissions[name].behavior == 'deny' and run.permissions[name].message == BACKGROUND_NEEDS_APPROVAL
    assert ph.store.db.execute('SELECT count(*) FROM approvals').fetchone()[0] == 0 if ph.store.db.execute(
        "SELECT 1 FROM sqlite_master WHERE name='approvals'").fetchone() else True
    # Its PostToolUse hooks never read notes or busy inputs (that is the main agent's step).
    child_post = [d for (hook, call), d in run.decisions.items() if hook == 'PostToolUse' and call != agent_call]
    assert child_post and all('additionalContext' not in json.dumps(d) for d in child_post)
    assert ph.store.pending_notes(run.tid) == 1
    # Nothing of the child is in the parent turn's timeline.
    parent = ph.store.items(run.turn)
    assert [i['type'] for i in parent] == ['userMessage', 'dynamicToolCall', 'subAgentActivity', 'subAgentActivity']
    assert 'toolu_01D6s1WYdvnyzhwfetoqmagM' not in json.dumps(parent)
    # The transcript stayed open past the turn, has the child's work, and closed on the task's end.
    ref = f'claude:{run.owner.session_id}:task:{task_id}'
    thread = (await ph.call('thread/read', {'threadId': ref, 'includeTurns': True}))['thread']
    assert thread['status'] == {'type': 'idle'}
    turn_wire = thread['turns'][0]
    assert turn_wire['status'] == 'completed' and turn_wire['durationMs'] is not None
    kinds = [i['type'] for i in turn_wire['items']]
    assert kinds.count('commandExecution') >= 4 and 'fileChange' in kinds and 'agentMessage' in kinds
    # One notice for the parent, with the subagent's reply (bounded) and its transcript.
    [notice] = notices(ph, run.tid)
    assert notice['noticeId'] == task_id and notice['status'] == 'completed' and notice['taskType'] == 'local_agent'
    assert notice['summary'].startswith('a.txt contains the text "alpha file"') and notice['transcript'] == ref
    assert notice['originTurnId'] == run.turn and notice['toolUseId'] == agent_call
    # The summary only travels in the notice, never in other task rows.
    assert all('summary' not in t for t in task_events(ph, run.tid))
    assert task_id not in run.bridge.background


async def test_background_usage_is_added_to_the_origin_turn(ph):
    run = await replay(ph, 'default', mode='default')
    await run.until()
    usage = json.loads(ph.store.db.execute('SELECT usage FROM turns WHERE id=?', (run.turn,)).fetchone()[0])
    [entry] = [c for c in usage['modelRequests'] if c['kind'] == 'background-task']
    assert entry['taskId'] == 'aa335f43da4fb68e1'
    # Summed once per API message id (Claude Code repeats a message's usage on each block).
    child = {}
    for event in PROBE['default']:
        data = event['data']
        if event['kind'] == 'msg' and data.get('type') == 'assistant' and data.get('parent_tool_use_id'):
            old = child.get(data['message_id'], {})
            child[data['message_id']] = {k: max(v, old.get(k, 0)) for k, v in data['usage'].items()}
    assert entry['usage']['output_tokens'] == sum(u['output_tokens'] for u in child.values())
    assert entry['usage']['cache_read_input_tokens'] == sum(u['cache_read_input_tokens'] for u in child.values())
    updates = [e['params'] for e in ph.store.events(run.tid, 0, 10000) if e['method'] == 'thread/tokenUsage/updated']
    assert updates[-1]['turnId'] == run.turn and updates[-1]['tokenUsage']['total']['outputTokens'] >= entry['usage']['output_tokens']


async def test_usage_of_a_task_ending_inside_its_turn_waits_for_the_result(ph):
    tid, turn, bridge = await begin(ph)
    await ph.engine.add_task_usage(tid, turn, 'task-x', {'output_tokens': 5})
    assert ph.engine.pending_task_usage[turn][0]['taskId'] == 'task-x'
    ph.store.finish(tid, turn, 'interrupted', None)
    await ph.engine._flush_task_usage(tid, turn)
    usage = json.loads(ph.store.db.execute('SELECT usage FROM turns WHERE id=?', (turn,)).fetchone()[0])
    assert usage['modelRequests'][-1]['usage'] == {'output_tokens': 5}
    # A late item for an unknown turn is ignored.
    await ph.engine.add_task_usage(tid, 'no-such-turn', 'task-y', {'output_tokens': 1})


async def test_full_access_safety_check_is_a_block_on_the_origin_turn(ph):
    run = await replay(ph, 'default', mode='bypassPermissions')
    await run.until()
    write = run.permissions['Write']
    assert write.behavior == 'deny' and 'may approve it once' in write.message
    blocks = ph.store.db.execute('SELECT turn_id,item_id,tool_name FROM safety_blocks').fetchall()
    assert [tuple(b) for b in blocks if b[2] == 'Write'] == [(run.turn, 'toolu_01RTMZB9od3uy2gmQmJx4RTE', 'Write')]
    blocked = [e['params'] for e in ph.store.events(run.tid, 0, 10000) if e['method'] == 'thread/safetyCheck/blocked']
    assert blocked and blocked[-1]['turnId'] == run.turn


async def test_background_child_denials(ph):
    run = await replay(ph, 'default', mode='default')
    await run.until(lambda e: e['kind'] == 'msg' and e['data'].get('type') == 'result')
    await run.until(lambda e: e['kind'] == 'hook:PreToolUse' and e['data'].get('agent_id'))
    agent = 'aa335f43da4fb68e1'
    sid = run.owner.session_id
    assert agent in run.bridge.background
    base = {'session_id': sid, 'agent_id': agent, 'hook_event_name': 'PreToolUse'}

    async def reason(name, tool_input=None, **extra):
        decision = await run.bridge.pre_tool({**base, **extra, 'tool_name': name, 'tool_input': tool_input or {}}, identifier(), {})
        return decision.get('hookSpecificOutput', {}).get('permissionDecisionReason')

    assert await reason('mcp__cutex__agent_tell', {'arguments': {}}) == BACKGROUND_NO_HOST_TOOLS
    assert await reason('Agent', {'prompt': 'x', 'run_in_background': False}) == BACKGROUND_NO_AGENTS
    assert await reason('Task', {'prompt': 'x'}) == BACKGROUND_NO_AGENTS
    assert await reason('AskUserQuestion', {'questions': []}) == BACKGROUND_NO_QUESTIONS
    assert await reason('SendMessage') == 'Tool is not enabled by this runtime configuration'
    assert await reason('mcp__unknown__x') == 'Tool is not enabled by this runtime configuration'
    assert 'identity mismatch' in await reason('Read', session_id='other-session')
    assert await reason('Read', {'file_path': '/w/a'}) is None
    object.__setattr__(ph.settings, 'allow_exec', False)
    assert await reason('Bash', {'command': 'ls'}) == 'Tool is not enabled by this runtime configuration'
    object.__setattr__(ph.settings, 'allow_exec', True)
    # Plan mode stays read-only.
    await ph.engine.permissions.set_plan(run.tid, True)
    assert await reason('Write', {'file_path': '/w/a', 'content': ''}) == 'Plan mode allows read-only subagent tools'
    assert await reason('Grep', {'pattern': 'x'}) is None
    await ph.engine.permissions.set_plan(run.tid, False)
    # can_use_tool: questions and cutex tools are refused even under full access.
    ph.store.db.execute('INSERT OR REPLACE INTO thread_tool_approvals VALUES(?,?)', (run.tid, 'bypassPermissions'))
    context = SimpleNamespace(tool_use_id='q-1', agent_id=agent, title=None, decision_reason=None, suggestions=[])
    asked = await run.options.can_use_tool('AskUserQuestion', {'questions': []}, context)
    assert asked.behavior == 'deny' and asked.message == BACKGROUND_NO_QUESTIONS
    # A tool call id of the background child cannot be claimed by another agent.
    call = identifier()
    assert await run.bridge.pre_tool({**base, 'tool_name': 'Read', 'tool_input': {}}, call, {}) == {}
    other = await run.bridge.pre_tool({**base, 'agent_id': 'someone-else', 'tool_name': 'Read', 'tool_input': {}}, call, {})
    assert other['hookSpecificOutput']['permissionDecision'] == 'deny'


async def test_stop_ends_a_running_background_subagent(ph):
    run = await replay(ph, 'stop', mode='bypassPermissions')
    await run.until(lambda e: e['kind'] == 'action')
    task_id = 'a52f24f27d7f52e86'
    assert (await ph.call('claude/tasks/stop', {'threadId': run.tid, 'taskId': task_id}))['stopRequested'] is True
    assert ('task_stop', task_id) in ph.sdk.clients[0].controls
    await run.until()
    [notice] = notices(ph, run.tid)
    assert notice['status'] == 'stopped' and notice['noticeId'] == task_id
    row = ph.engine.subagents.resolve(f'claude:{run.owner.session_id}:task:{task_id}')
    assert row['status'] == 'interrupted'
    error = await ph.call('claude/tasks/stop', {'threadId': run.tid, 'taskId': task_id}, error=True)
    assert 'not a known running task' in error['message']


async def test_interrupting_or_ending_the_parent_turn_keeps_the_child(ph):
    run = await replay(ph, 'default', mode='bypassPermissions')
    await run.until(lambda e: e['kind'] == 'hook:SubagentStart')
    agent = 'aa335f43da4fb68e1'
    assert agent in run.bridge.background
    # turn/interrupt revokes the turn's tool context only.
    run.bridge.revoke()
    assert agent in run.bridge.background and not ph.sdk.clients[0].controls
    await run.until()
    assert notices(ph, run.tid)[0]['status'] == 'completed'


async def test_a_later_turn_runs_while_the_child_works(ph):
    run = await replay(ph, 'followup', mode='bypassPermissions')
    await run.until(lambda e: e['kind'] == 'action')
    # The owner's next turn starts while the child still runs (probe: answered at once).
    second = identifier()
    ph.store.db.execute("INSERT INTO turns(id,thread_id,state,started) VALUES(?,?,'inProgress',0)", (second, run.tid))
    run.bridge.begin_turn(second)
    ph.store.enqueue_note(run.tid, 'note-2', {'source': {'kind': 'agent', 'id': 'cutex.a'}, 'eventType': 'agent_message',
                                            'text': 'A NOTE', 'metadata': {'deliveryMode': 'passive'}}, 10)
    run.results = 0  # The second Result ends the second turn.
    run.turn = second
    await run.until()
    child_pre = [call for (hook, call) in run.decisions if hook == 'PreToolUse' and call != 'toolu_0164zAqjhMuDJKhYxgvta65M']
    assert child_pre and not any(run.denied(call) for call in child_pre)
    # The child's hooks during the second turn never took the note meant for the main agent.
    assert ph.store.pending_notes(run.tid) == 1
    assert notices(ph, run.tid)[0]['status'] == 'completed'


async def test_lost_runs_are_reported_after_an_adapter_restart(ph):
    run = await replay(ph, 'default', mode='bypassPermissions')
    await run.until(lambda e: e['kind'] == 'msg' and e['data'].get('type') == 'result')
    await run.until(lambda e: e['kind'] == 'msg' and e['data'].get('type') == 'result')  # turn ended, child running
    # A new adapter process: the open background run is lost; its notice waits for a resume.
    ph.engine.subagents = SubagentTranscripts(ph.engine)
    [(method, params)] = ph.store.recovered_events[run.tid]
    assert method == 'claude/task/updated'
    assert params['task']['notifyParent']['status'] == 'lost' and params['task']['taskId'] == 'aa335f43da4fb68e1'
    seen = len(ph.out)
    await ph.call('thread/resume', {'threadId': run.tid})
    replayed = [m for m in ph.out[seen:] if m.get('method') == 'claude/task/updated']
    assert replayed and replayed[0]['params']['task']['notifyParent']['noticeId'] == 'aa335f43da4fb68e1'


async def test_lost_in_process_when_the_claude_child_ends(ph):
    run = await replay(ph, 'default', mode='bypassPermissions')
    await run.until(lambda e: e['kind'] == 'hook:SubagentStart')
    await run.owner.tasks.lost('The Claude Code process ended and was restarted while it ran.')
    [notice] = notices(ph, run.tid)
    assert notice['status'] == 'lost' and 'restarted' in notice['reason']
    # Also kept for an observer that resumes the thread later (cutex queues it once).
    [(method, params)] = ph.store.recovered_events[run.tid]
    assert method == 'claude/task/updated' and params['task']['notifyParent']['noticeId'] == 'aa335f43da4fb68e1'
    row = ph.engine.subagents.resolve(f'claude:{run.owner.session_id}:task:aa335f43da4fb68e1')
    assert row['status'] == 'interrupted'
    await run.owner.tasks.lost('again')
    assert len(notices(ph, run.tid)) == 1


async def test_notice_without_notification_after_grace_and_once_only(ph, monkeypatch):
    monkeypatch.setattr(runtime_tasks, 'NOTICE_GRACE_SECONDS', 0.05)
    run = await replay(ph, 'default', mode='bypassPermissions')
    await run.until(lambda e: e['kind'] == 'hook:SubagentStop')
    await run.step(run.script[run.index])  # SubagentStop: completed, no summary yet
    await until(lambda: notices(ph, run.tid))
    [notice] = notices(ph, run.tid)
    assert notice['status'] == 'completed' and notice['summary'] is None
    run.index += 1
    await run.until()  # The late task_notification adds no second notice.
    assert len(notices(ph, run.tid)) == 1


async def test_background_shell_notices_are_rate_limited_and_coalesced(ph, monkeypatch):
    monkeypatch.setattr(runtime_tasks, 'SHELL_NOTICE_SECONDS', 0.2)
    tid, turn, bridge = await begin(ph)
    tasks = ph.engine.driver.owners[tid].tasks
    for n in range(3):
        await tasks.shell_started(f'b{n}', turn, f'call-{n}', {'command': f'sleep {n}', 'run_in_background': True})
    # A subagent's own foreground command and a main-agent foreground one never notify.
    await tasks.observe({'type': 'system', 'subtype': 'task_started', 'task_id': 'owned', 'task_type': 'local_bash',
                         'owned_by_subagent': True, 'is_backgrounded': False}, None)
    await tasks.shell_started('fg', turn, 'call-fg', {'command': 'ls'})
    for task in ['b0', 'b1', 'b2', 'owned', 'fg']:
        await tasks.observe({'type': 'system', 'subtype': 'task_notification', 'task_id': task, 'status': 'completed',
                             'summary': f'done {task}'}, None)
    first = notices(ph, tid)
    assert [n['noticeId'] for n in first] == ['b0']
    await until(lambda: len(notices(ph, tid)) == 2)
    batch = notices(ph, tid)[1]
    assert batch['noticeId'] == 'shells:b1' and [t['taskId'] for t in batch['tasks']] == ['b1', 'b2']
    assert batch['description'] == '2 background commands'


async def test_capability_and_features(ph):
    caps = await ph.call('claude/capabilities')
    assert caps['input']['backgroundSubagents'] is True and caps['features']['backgroundSubagents'] is True
