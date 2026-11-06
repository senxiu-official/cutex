"""Agent pause at the next tool boundary (cutex r276, docs/agent-pause.md).

cutex closes the gate of a thread with `claude/toolGate/set {threadId, held: true}` when the agent is
paused and opens it on resume; it sends the state again after every runtime (re)connect, the gate is
not stored here. While the gate is closed every tool call waits in the PreToolUse hook before it starts:
the main agent, foreground subagents and background subagents. A call that is already running
finishes. Opening the gate lets the waiting calls run in the same turn; Stop or the end of the turn
cancels them, and the tool is not run.

Checked on 2026-11-04 against Claude Code 2.1.314 (SDK 0.2.175, private home, fake Messages API):
a hook callback without a timeout was awaited for 90 s and the tool ran only after it returned; with a
configured timeout the CLI cancels the callback at the deadline and does not run the tool ("PreToolUse
hook did not respond before its timeout ... The tool call was not executed"). The hook matcher sets
HOOK_TIMEOUT so a long pause is not cut short, and a wait answers itself before that deadline.
"""
from __future__ import annotations

import asyncio
import time
from dataclasses import dataclass

GET, SET, CHANGED = 'claude/toolGate/get', 'claude/toolGate/set', 'claude/toolGate/changed'
METHODS = {GET, SET}
# The PreToolUse matcher's timeout (seconds). A wait gives up HOLD_MARGIN earlier with its own refusal.
HOOK_TIMEOUT = 7 * 24 * 3600
HOLD_MARGIN = 600
MAX_LISTED = 64
EXPIRED = ('The agent stayed paused too long; this tool call was not run. Call it again if it is still needed.')
ENDED = 'The turn ended while the agent was paused; this tool call was not run.'


@dataclass
class Waiting:
    call_id: str
    tool: str
    agent_id: str | None
    turn_id: str | None
    since_ms: int

    def wire(self) -> dict:
        return {'callId': self.call_id, 'tool': self.tool, 'agentId': self.agent_id,
                'turnId': self.turn_id, 'sinceMs': self.since_ms}


class ToolGate:
    def __init__(self, engine, hold_limit: float = HOOK_TIMEOUT - HOLD_MARGIN):
        self.engine = engine
        self.hold_limit = hold_limit
        self.opened: dict[str, asyncio.Event] = {}  # closed threads -> set when the gate opens
        self.waiting: dict[str, dict[str, Waiting]] = {}

    def closed(self, tid: str) -> bool:
        return tid in self.opened

    def snapshot(self, tid: str) -> dict:
        rows = sorted(self.waiting.get(tid, {}).values(), key=lambda w: w.since_ms)
        return {'threadId': tid, 'held': self.closed(tid), 'waiting': [w.wire() for w in rows[:MAX_LISTED]],
                'waitingCount': len(rows)}

    async def set(self, tid: str, held: bool) -> dict:
        if held and tid not in self.opened:
            self.opened[tid] = asyncio.Event()
            await self._changed(tid, None)
        elif not held and tid in self.opened:
            self.opened.pop(tid).set()
            await self._changed(tid, None)
        return self.snapshot(tid)

    async def wait(self, tid: str, call_id: str, tool: str, agent_id: str | None, turn_id: str | None) -> bool:
        """Hold one call while the gate is closed. False if the hold ran out (the call must not run)."""
        opened = self.opened.get(tid)
        if opened is None:
            return True
        rows = self.waiting.setdefault(tid, {})
        rows[call_id] = Waiting(call_id, tool, agent_id, turn_id, int(time.time() * 1000))
        try:
            await self._changed(tid, turn_id)
            try:
                await asyncio.wait_for(opened.wait(), self.hold_limit)
            except TimeoutError:
                return False
            return True
        finally:
            # Cancellation (Stop, interrupt, child exit) also lands here.
            rows.pop(call_id, None)
            if not rows:
                self.waiting.pop(tid, None)
            await asyncio.shield(self._changed(tid, turn_id))

    async def _changed(self, tid: str, turn_id: str | None) -> None:
        await self.engine.emit(tid, CHANGED, self.snapshot(tid), durable=False)
        # The status flag only for a turn still running: a late release after Stop must not
        # report an ended thread as active again.
        running = turn_id is not None and self.engine.store.db.execute(
            "SELECT 1 FROM turns WHERE id=? AND thread_id=? AND state='inProgress'", (turn_id, tid)).fetchone()
        if running:
            flags = self.engine.active_flags(tid) if hasattr(self.engine, 'active_flags') else []
            await self.engine.emit(tid, 'thread/status/changed', {
                'threadId': tid, 'status': {'type': 'active', 'activeFlags': flags}}, durable=False)

    def flags(self, tid: str) -> list[str]:
        return ['pausedAtTool'] if self.waiting.get(tid) else []
