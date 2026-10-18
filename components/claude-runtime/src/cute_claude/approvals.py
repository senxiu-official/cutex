"""Persist Claude native permission modes and bridge SDK human requests to clients.

The SDK owns permission decisions; this module renders only requests which
reach its callback. It does not grant disabled tools or OS sandbox access.

A request goes to every interactive client subscribed to the thread (the
cute-codex TUI, and cutex Management which relays it to the desktop); the
first valid answer wins and ``serverRequest/resolved`` tells the others.
"""
from __future__ import annotations
import asyncio
import json
import shlex
import time
from dataclasses import dataclass, field
from .store import identifier
from .errors import require
from .questions import questions_wire, read_answers

MODES = {'default', 'acceptEdits', 'auto', 'bypassPermissions'}

UNATTENDED_QUESTION = ('No human is available to answer in this turn (it was started by {who}). '
                       'Decide yourself with reasonable assumptions and state them, '
                       'or ask the requesting agent in your reply.')


def unattended_question(source: dict | None, mode: str) -> str | None:
    """Tool result for AskUserQuestion in a turn the owner did not start, or
    None when the question should go to the clients (owner turn, unknown
    origin, or mode "wait")."""
    if mode != 'decide' or not source or source.get('kind') in (None, 'user'):
        return None
    kind, ident, name = source.get('kind'), source.get('id'), source.get('name')
    label = f'{name} ({ident})' if name and ident and name != ident else (name or ident or 'unknown')
    if kind == 'agent':
        who = f'a message from agent {label} via the Agent Bus'
    elif kind == 'service':
        who = f'input from service {label}'
    else:
        who = f'{kind} input from {label}'
    return UNATTENDED_QUESTION.format(who=who)


@dataclass
class Pending:
    peers: set
    thread_id: str
    future: asyncio.Future
    kind: str = 'elicitation'  # elicitation | command | fileChange | question
    questions: list | None = None
    rules: list | None = None  # offered Claude permission rules (command / fileChange)


# "Always allow in this project" (cutex `claude-permission-rules`): the
# request offers Claude permission rules (`claudePermissionRules`), the answer
# `{"decision": {"acceptWithClaudeRules": {"rules": [...]}}}` names the ones
# to keep. cutex persists them per workspace; the adapter adds them to the
# native session at once (`updated_permissions`, destination session).
RULE_DECISION = 'acceptWithClaudeRules'


def decision_of(result, offered: list | None = None):
    """Codex v2 approval decision; object forms (policy amendments) accept once.

    ``acceptWithClaudeRules`` returns ``('acceptWithClaudeRules', rules)`` for
    a non-empty subset of the ``offered`` rules, else it accepts once."""
    decision = result.get('decision') if isinstance(result, dict) else None
    if isinstance(decision, dict) and len(decision) == 1 and next(iter(decision)) in {
            'acceptWithExecpolicyAmendment', 'applyNetworkPolicyAmendment'}:
        return 'accept'
    if isinstance(decision, dict) and len(decision) == 1 and RULE_DECISION in decision:
        body = decision[RULE_DECISION]
        rules = body.get('rules') if isinstance(body, dict) else None
        if (isinstance(rules, list) and rules and offered
                and all(isinstance(rule, str) and rule in offered for rule in rules)):
            return (RULE_DECISION, tuple(dict.fromkeys(rules)))
        return 'accept'
    return decision if decision in {'accept', 'acceptForSession', 'decline', 'cancel'} else None


def is_allowed(decision) -> bool:
    return decision in {'accept', 'acceptForSession'} or (isinstance(decision, tuple) and decision[0] == RULE_DECISION)


class Approvals:
    def __init__(self, server):
        self.server = server
        self.pending: dict[str, Pending] = {}
        server.store.db.execute('CREATE TABLE IF NOT EXISTS thread_tool_approvals(thread_id TEXT PRIMARY KEY REFERENCES threads(id), mode TEXT NOT NULL)')
        server.store.db.execute("UPDATE thread_tool_approvals SET mode=CASE mode WHEN 'ask' THEN 'default' WHEN 'off' THEN 'bypassPermissions' ELSE mode END")
        server.store.db.execute('CREATE TABLE IF NOT EXISTS thread_plan_mode(thread_id TEXT PRIMARY KEY REFERENCES threads(id), enabled INTEGER NOT NULL)')
        server.engine.permissions = self

    def mode(self, tid):
        row = self.server.store.db.execute('SELECT mode FROM thread_tool_approvals WHERE thread_id=?', (tid,)).fetchone()
        return row[0] if row else 'bypassPermissions'

    def planning(self, tid):
        row = self.server.store.db.execute('SELECT enabled FROM thread_plan_mode WHERE thread_id=?', (tid,)).fetchone()
        return bool(row and row[0])

    def effective_mode(self, tid):
        return 'plan' if self.planning(tid) else self.mode(tid)

    async def set_plan(self, tid, enabled):
        # Like Claude Code (shift+tab), plan mode can change while a turn runs:
        # the native session applies it from its next tool call on.
        self.server.store.thread(tid)
        if self.planning(tid) == enabled:
            return
        owner = getattr(self.server.engine.driver, 'owners', {}).get(tid)
        if owner is not None:
            await owner.set_permission_mode('plan' if enabled else self.mode(tid))
        self.server.store.db.execute('INSERT OR REPLACE INTO thread_plan_mode VALUES(?,?)', (tid, int(enabled)))

    async def configure(self, tid, mode):
        self.server.store.thread(tid)
        require(mode in MODES, 'mode must be default, acceptEdits, auto or bypassPermissions')
        # A running turn may change mode (Claude Code's shift+tab): the SDK's
        # set_permission_mode applies it to the live session at once.
        owner = getattr(self.server.engine.driver, 'owners', {}).get(tid) if hasattr(self.server.engine, 'driver') else None
        if owner is not None:
            await owner.set_permission_mode('plan' if self.planning(tid) else mode)
        self.server.store.db.execute('INSERT OR REPLACE INTO thread_tool_approvals VALUES(?,?)', (tid,mode))
        return {'threadId':tid, 'mode':mode, 'scope':'sdk-tools'}

    def targets(self, tid):
        return {p for p in self.server.peers if p.ready and p.interactive and tid in p.subscriptions}

    def disconnect(self, peer):
        for p in self.pending.values():
            p.peers.discard(peer)
            if not p.peers and not p.future.done():
                p.future.set_result(None)

    def respond(self, peer, message):
        rid = message.get('id')
        p = self.pending.get(rid) if isinstance(rid,str) else None
        if p is None or peer not in p.peers or p.future.done():
            return
        result = None if 'error' in message else message.get('result')
        if p.kind == 'question':
            p.future.set_result(read_answers(result, p.questions) if result is not None else None)
        elif p.kind in {'command', 'fileChange'}:
            p.future.set_result(decision_of(result, p.rules))
        else:
            content = result.get('content') if isinstance(result,dict) else None
            accepted = (isinstance(result,dict) and result.get('action') == 'accept' and
                        isinstance(content,dict) and content.get('approved') is True)
            p.future.set_result('accept' if accepted else ('cancel' if isinstance(result, dict) and result.get('action') == 'cancel' else 'decline'))

    async def _request(self, tid, method, params, kind, *, flag='waitingOnApproval', questions=None, rules=None):
        """Send one server request to every interactive client; None if nobody answers."""
        peers = self.targets(tid)
        if not peers:
            return None
        rid = f'claude-{"question" if kind == "question" else "approval"}-' + identifier()
        future = asyncio.get_running_loop().create_future()
        self.pending[rid] = Pending(set(peers), tid, future, kind, questions, rules)
        try:
            await self.server.engine.emit(tid, 'thread/status/changed', {
                'threadId':tid,'status':{'type':'active','activeFlags':[flag]}})
            for peer in list(peers):
                try:
                    await peer.send({'id': rid, 'method': method, 'params': params})
                except (ConnectionError, OSError):
                    self.pending[rid].peers.discard(peer)
            if not self.pending[rid].peers and not future.done():
                future.set_result(None)
            return await future
        finally:
            self.pending.pop(rid, None)
            future.cancel()
            await self.server.broadcast(tid,'serverRequest/resolved',{'threadId':tid,'requestId':rid})
            if not any(p.thread_id == tid for p in self.pending.values()):
                await self.server.engine.emit(tid,'thread/status/changed',{
                    'threadId':tid,'status':{'type':'active','activeFlags':[]}})

    async def request_command(self, tid, turn_id, item_id, command, reason=None, rules=None):
        """`item/commandExecution/requestApproval` for a native tool call; returns the decision.
        ``rules``: Claude permission rules offered for "always allow in this project"."""
        decisions = ['accept', 'acceptForSession']
        if rules:
            decisions.append({RULE_DECISION: {'rules': list(rules)}})
        params = {
            'kind': 'command', 'threadId': tid, 'turnId': turn_id, 'itemId': item_id,
            'startedAtMs': int(time.time()*1000), 'approvalId': None, 'environmentId': None,
            'command': command, 'cwd': str(self.server.settings.workspace),
            'reason': reason or 'Claude requests permission to use this tool.', 'commandActions': None,
            'availableDecisions': decisions + ['decline', 'cancel']}
        if rules:
            params['claudePermissionRules'] = list(rules)
        return await self._request(tid, 'item/commandExecution/requestApproval', params, 'command', rules=rules)

    async def request_file_change(self, tid, turn_id, item_id, reason=None, rules=None):
        """`item/fileChange/requestApproval` for the fileChange item already started."""
        params = {
            'threadId': tid, 'turnId': turn_id, 'itemId': item_id, 'startedAtMs': int(time.time()*1000),
            'reason': reason or 'Claude requests permission to edit this file.', 'grantRoot': None}
        if rules:
            params['claudePermissionRules'] = list(rules)
        return await self._request(tid, 'item/fileChange/requestApproval', params, 'fileChange', rules=rules)

    async def approve(self, tid, turn_id, name, arguments, call_id=None):
        """A host (cutex) tool call the official SDK asks a human about (mode
        ``default``): an ordinary approval card naming the tool and its
        arguments, answered with the approval buttons. Returns the decision
        (``accept`` / ``acceptForSession`` / ``decline`` / ``cancel``) or None
        when nobody answered. Services subscribing to runtime events must not
        consume prompts.

        Until claude-r18 this was an MCP elicitation form whose "Allow this
        invocation" checkbox defaulted to false: accepting the card without
        ticking it declined the call ("Not approved", r243 acceptance)."""
        preview = json.dumps(arguments, ensure_ascii=False, indent=2)
        # Never approve an invocation whose full arguments cannot be reviewed.
        if len(preview.encode('utf-8')) > 65536:
            return None
        if name in {'exec', 'Bash'}:
            command = arguments.get('command','') if name == 'Bash' else shlex.join(arguments.get('argv', []))
            reason = 'Claude requests permission to run this command.'
        else:
            command = f'{name} {preview}'
            reason = f'Claude requests permission to use the cutex tool {name}.'
        return await self.request_command(tid, turn_id, call_id or identifier(), command, reason)

    async def decline_unattended(self, tid, turn_id, arguments, call_id) -> str | None:
        """AskUserQuestion in a turn started by another agent or a service:
        record the attempt and return the tool result for the model, without
        asking anyone. None: ask the clients as usual."""
        notice = unattended_question(self.server.store.turn_source(turn_id),
                                     self.server.settings.agent_turn_questions)
        if notice is None:
            return None
        item = {'type':'dynamicToolCall', 'id':call_id, 'namespace':'claude', 'tool':'AskUserQuestion',
                'arguments':arguments, 'status':'inProgress', 'contentItems':None, 'success':None, 'durationMs':None}
        self.server.store.put_item(turn_id, item)
        await self.server.engine.emit(tid, 'item/started', {'threadId':tid,'turnId':turn_id,'item':item})
        item.update(status='failed', success=False, durationMs=0,
                    contentItems=[{'type':'inputText','text':notice}])
        self.server.store.put_item(turn_id, item)
        await self.server.engine.emit(tid, 'item/completed', {'threadId':tid,'turnId':turn_id,'item':item})
        return notice

    async def ask(self, tid, turn_id, arguments, call_id):
        questions = questions_wire(arguments)
        if not self.targets(tid):
            return None
        item = {'type':'dynamicToolCall', 'id':call_id, 'namespace':'claude', 'tool':'AskUserQuestion',
                'arguments':arguments, 'status':'inProgress', 'contentItems':None, 'success':None, 'durationMs':None}
        started = time.monotonic()
        answers = None
        try:
            self.server.store.put_item(turn_id, item)
            await self.server.engine.emit(tid, 'item/started', {'threadId':tid,'turnId':turn_id,'item':item})
            answers = await self._request(tid, 'item/tool/requestUserInput', {
                'threadId':tid,'turnId':turn_id,'itemId':call_id,'questions':questions,'isBlocking':True},
                'question', flag='waitingOnUserInput', questions=arguments['questions'])
            return answers
        finally:
            item.update(status='completed' if answers is not None else 'failed', success=answers is not None,
                        contentItems=[{'type':'inputText','text':json.dumps(answers, ensure_ascii=False) if answers is not None else 'No answer received.'}],
                        durationMs=int((time.monotonic()-started)*1000))
            self.server.store.put_item(turn_id, item)
            await self.server.engine.emit(tid, 'item/completed', {'threadId':tid,'turnId':turn_id,'item':item})
