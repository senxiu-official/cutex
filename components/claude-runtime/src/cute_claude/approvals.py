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


def decision_of(result) -> str | None:
    """Codex v2 approval decision; object forms (policy amendments) accept once."""
    decision = result.get('decision') if isinstance(result, dict) else None
    if isinstance(decision, dict) and len(decision) == 1 and next(iter(decision)) in {
            'acceptWithExecpolicyAmendment', 'applyNetworkPolicyAmendment'}:
        return 'accept'
    return decision if decision in {'accept', 'acceptForSession', 'decline', 'cancel'} else None


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
        self.server.store.thread(tid)
        if self.planning(tid) == enabled:
            return
        require(not self.server.store.user_turn_busy(tid), 'Finish or interrupt the turn before changing mode', -32009)
        owner = getattr(self.server.engine.driver, 'owners', {}).get(tid)
        if owner is not None:
            await owner.set_permission_mode('plan' if enabled else self.mode(tid))
        self.server.store.db.execute('INSERT OR REPLACE INTO thread_plan_mode VALUES(?,?)', (tid, int(enabled)))

    async def configure(self, tid, mode):
        self.server.store.thread(tid)
        require(mode in MODES, 'mode must be default, acceptEdits, auto or bypassPermissions')
        require(tid not in self.server.engine.workers, 'Change approvals while the thread is idle', -32009)
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
            p.future.set_result(decision_of(result))
        else:
            content = result.get('content') if isinstance(result,dict) else None
            accepted = (isinstance(result,dict) and result.get('action') == 'accept' and
                        isinstance(content,dict) and content.get('approved') is True)
            p.future.set_result('accept' if accepted else ('cancel' if isinstance(result, dict) and result.get('action') == 'cancel' else 'decline'))

    async def _request(self, tid, method, params, kind, *, flag='waitingOnApproval', questions=None):
        """Send one server request to every interactive client; None if nobody answers."""
        peers = self.targets(tid)
        if not peers:
            return None
        rid = f'claude-{"question" if kind == "question" else "approval"}-' + identifier()
        future = asyncio.get_running_loop().create_future()
        self.pending[rid] = Pending(set(peers), tid, future, kind, questions)
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

    async def request_command(self, tid, turn_id, item_id, command, reason=None):
        """`item/commandExecution/requestApproval` for a native tool call; returns the decision."""
        return await self._request(tid, 'item/commandExecution/requestApproval', {
            'kind': 'command', 'threadId': tid, 'turnId': turn_id, 'itemId': item_id,
            'startedAtMs': int(time.time()*1000), 'approvalId': None, 'environmentId': None,
            'command': command, 'cwd': str(self.server.settings.workspace),
            'reason': reason or 'Claude requests permission to use this tool.', 'commandActions': None,
            'availableDecisions': ['accept', 'acceptForSession', 'decline', 'cancel']}, 'command')

    async def request_file_change(self, tid, turn_id, item_id, reason=None):
        """`item/fileChange/requestApproval` for the fileChange item already started."""
        return await self._request(tid, 'item/fileChange/requestApproval', {
            'threadId': tid, 'turnId': turn_id, 'itemId': item_id, 'startedAtMs': int(time.time()*1000),
            'reason': reason or 'Claude requests permission to edit this file.', 'grantRoot': None}, 'fileChange')

    async def approve(self, tid, turn_id, name, arguments, call_id=None):
        # Called only when the official SDK requests a human decision for a host
        # tool. Services subscribing to runtime events must not consume prompts.
        preview = json.dumps(arguments, ensure_ascii=False, indent=2)
        # Never approve an invocation whose full arguments cannot be reviewed.
        if len(preview.encode('utf-8')) > 65536:
            return False
        if name in {'exec', 'Bash'}:
            command = arguments.get('command','') if name == 'Bash' else shlex.join(arguments.get('argv', []))
            decision = await self.request_command(tid, turn_id, call_id or identifier(), command,
                                                  'Claude requests permission to run this command.')
            return decision in {'accept', 'acceptForSession'}
        decision = await self._request(tid, 'mcpServer/elicitation/request', {
            'threadId':tid,'turnId':turn_id,'serverName':'cutex', 'mode':'form',
            'message':f'Allow Claude to run {name} once?\n{preview}',
            'requestedSchema':{'type':'object','properties':{'approved':{
                'type':'boolean','title':'Allow this invocation','default':False}},'required':['approved']}}, 'elicitation')
        return decision == 'accept'

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
