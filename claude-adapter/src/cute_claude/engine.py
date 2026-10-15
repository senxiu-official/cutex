"""One serial worker per thread. Thread resume is read-only; no hidden model calls."""
from __future__ import annotations

import asyncio
import json
import time
from typing import Awaitable, Callable

from .config import Settings
from .errors import RpcError
from .provider import Provider, ProviderError
from .store import Store, dump, identifier
from .tools import ToolContext, ToolRegistry

Notify = Callable[[str, str, dict], Awaitable[None]]


class Engine:
    def __init__(self, settings: Settings, store: Store, provider: Provider, tools: ToolRegistry):
        self.settings, self.store, self.provider, self.tools = settings, store, provider, tools
        self.workers: dict[str, asyncio.Task] = {}
        self.notify: Notify = self._noop
        self.closing = False

    @staticmethod
    async def _noop(tid: str, method: str, params: dict) -> None:
        pass

    async def emit(self, tid: str, method: str, params: dict, *, durable: bool = True) -> None:
        params = self.store.stamp_event(method, params)
        if durable:
            self.store.event(tid, method, params)
        await self.notify(tid, method, params)

    async def emit_input_presentation(self, tid: str, mid: str) -> None:
        item = self.store.input_presentation(tid, mid)
        if item is None:
            return
        method = 'thread/presentation/appended'
        position = self.store.event(tid, method, {'threadId': tid, '_inputPresentationRef': mid})
        await self.notify(tid, method, {'threadId': tid, 'position': position, 'item': item})

    async def inject_notes(self, tid: str, turn_id: str) -> str:
        """Read pending notes into the running turn `turn_id` (after a tool
        call): their timeline items are emitted now and the model text is
        returned. At most once: a note taken here is never offered again."""
        taken = self.store.take_notes(tid, turn_id)
        for item, _ in taken:
            await self.emit(tid, "item/started", {"threadId": tid, "turnId": turn_id, "item": item})
            await self.emit(tid, "item/completed", {"threadId": tid, "turnId": turn_id, "item": item})
        return "\n\n".join(frame for _, frame in taken)

    def wake(self, tid: str) -> None:
        if not self.closing and tid not in self.workers and not self.store.thread(tid)["halted"]:
            self.workers[tid] = asyncio.create_task(self._worker(tid), name=f"claude-turn:{tid}")

    async def interrupt(self, tid: str, turn_id: str) -> None:
        row = self.store.db.execute("SELECT id FROM turns WHERE thread_id=? AND state='inProgress'", (tid,)).fetchone()
        if not row or row[0] != turn_id:
            raise RpcError(-32009, "turnId does not name the active turn")
        worker = self.workers.get(tid)
        if worker:
            worker.cancel()
            await asyncio.gather(worker, return_exceptions=True)
        # Also covers cancellation before a just-created worker ran its first bytecode.
        if self.store.turn(turn_id)["status"] == "inProgress":
            self.store.finish(tid, turn_id, "interrupted")
            await self.emit(tid, "turn/completed", {"threadId": tid, "turn": self.store.turn(turn_id)})
        if self.workers.get(tid) is worker:
            self.workers.pop(tid, None)

    async def close(self) -> None:
        self.closing = True
        tasks = list(self.workers.values())
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        await self.provider.close()
        await self.tools.close()

    async def _worker(self, tid: str) -> None:
        try:
            while not self.closing:
                record = self.store.claim(tid)
                if record is None:
                    break
                await self._run_turn(tid, record)
                if self.store.thread(tid)["halted"]:
                    break
        finally:
            if self.workers.get(tid) is asyncio.current_task():
                self.workers.pop(tid, None)

    async def _run_turn(self, tid: str, record: dict) -> None:
        turn_id = record["turn_id"]
        open_items: dict[str, dict] = {}
        state, error = "completed", None
        usage_calls: list[dict] = []
        try:
            await self.emit(tid, "turn/started", {"threadId": tid, "turn": self.store.turn(turn_id, include_items=False)})
            await self.emit(tid, "thread/status/changed", {"threadId": tid, "status": {"type": "active", "activeFlags": []}})
            await self.emit_input_presentation(tid, record["message_id"])
            for item in self.store.items(turn_id):
                await self.emit(tid, "item/started", {"threadId": tid, "turnId": turn_id, "item": item})
                await self.emit(tid, "item/completed", {"threadId": tid, "turnId": turn_id, "item": item})
            for step in range(self.settings.max_steps):
                by_index: dict[int, dict] = {}
                async def on_text(index: int, chunk: str) -> None:
                    if index not in by_index:
                        item = {"type": "agentMessage", "id": identifier(), "text": "", "phase": "commentary",
                                "memoryCitation": None, "delivery": None, "questions": None}
                        by_index[index] = item
                        open_items[item["id"]] = item
                        self.store.put_item(turn_id, item)
                        await self.emit(tid, "item/started", {"threadId": tid, "turnId": turn_id, "item": item})
                    item = by_index[index]
                    item["text"] += chunk
                    # Only completed snapshots are durable. Streaming deltas do not rewrite all history.
                    await self.emit(tid, "item/agentMessage/delta", {"threadId": tid, "turnId": turn_id, "itemId": item["id"], "delta": chunk}, durable=False)

                thread = self.store.thread(tid)
                history = self.store.model_history(tid, self.settings.max_history_bytes)
                usage_calls.append({"step": step + 1, "outcome": "requested", "usage": None})
                self._usage(turn_id, usage_calls)
                completion = await self.provider.sample(history, self.tools.definitions(), thread["model"], on_text,
                        system=self.settings.system + ("\n" + thread["instructions"] if thread["instructions"] else ""))
                usage_calls[-1].update(outcome="received", usage=completion.usage, requestId=completion.request_id)
                self._usage(turn_id, usage_calls)
                if completion.stop_reason not in {"end_turn", "tool_use"}:
                    raise ProviderError(f"Unsupported or incomplete completion: stop_reason={completion.stop_reason}. No auto-continue.")
                calls = [b for b in completion.content if b["type"] == "tool_use"]
                if (completion.stop_reason == "tool_use") != bool(calls):
                    raise ProviderError("stop_reason/tool_use mismatch")
                if not completion.content:
                    raise ProviderError("Empty assistant completion")
                slot = self.store.append_assistant(tid, turn_id, completion.content)
                for index, block in enumerate(completion.content):
                    if block["type"] == "text":
                        if index not in by_index:
                            await on_text(index, block.get("text", ""))
                        item = by_index[index]
                        item["text"] = block.get("text", "")
                        item["phase"] = "commentary" if calls else "final_answer"
                        self.store.put_item(turn_id, item)
                        open_items.pop(item["id"], None)
                        await self.emit(tid, "item/completed", {"threadId": tid, "turnId": turn_id, "item": item})
                for call in calls:
                    assert slot is not None
                    self.store.tool_started(tid, turn_id, call, slot)
                    tool_item = {"type": "dynamicToolCall", "id": identifier(), "namespace": "claude",
                            "tool": call["name"], "arguments": call["input"], "status": "inProgress",
                            "contentItems": None, "success": None, "durationMs": None}
                    open_items[tool_item["id"]] = tool_item
                    self.store.put_item(turn_id, tool_item)
                    await self.emit(tid, "item/started", {"threadId": tid, "turnId": turn_id, "item": tool_item})
                    started = time.monotonic()
                    result = await self.tools.execute(call["name"], call["input"], ToolContext(tid, turn_id, call["id"], self.settings.workspace))
                    self.store.tool_finished(tid, call["id"], slot, result.model_content(), result.success)
                    tool_item.update(status="completed" if result.success else "failed", success=result.success,
                            contentItems=result.timeline_content(), durationMs=int((time.monotonic() - started) * 1000))
                    self.store.put_item(turn_id, tool_item)
                    open_items.pop(tool_item["id"], None)
                    await self.emit(tid, "item/completed", {"threadId": tid, "turnId": turn_id, "item": tool_item})
                await self._emit_usage(tid, turn_id, usage_calls)
                if not calls:
                    break
            else:
                raise ProviderError("Tool-loop step budget reached. Explicit new input is required; no hidden continuation.")
        except asyncio.CancelledError:
            state = "interrupted"
        except Exception as exc:
            state = "failed"
            # Only known sanitized errors are exposed; avoid raw network/plugin exception strings.
            message = str(exc) if isinstance(exc, (RpcError, ProviderError)) else f"Turn failed ({type(exc).__name__}); no automatic retry"
            error = {"message": message, "codexErrorInfo": "other", "additionalDetails": None}
        finally:
            for item in open_items.values():
                if item["type"] == "dynamicToolCall":
                    item.update(status="failed", success=False, contentItems=[{"type": "inputText", "text": "Interrupted; execution outcome may be uncertain. Inspect the tool journal."}])
                self.store.put_item(turn_id, item)
                await self.emit(tid, "item/completed", {"threadId": tid, "turnId": turn_id, "item": item})
            self.store.finish(tid, turn_id, state, error)
            await self.emit(tid, "turn/completed", {"threadId": tid, "turn": self.store.turn(turn_id)})
            await self.emit(tid, "thread/status/changed", {"threadId": tid, "status": {"type": "idle"}})
            await self.emit(tid, "claude/input/statusChanged", self.store.input_status(tid, record["message_id"]))

    def _usage(self, turn_id: str, calls: list[dict]) -> None:
        self.store.db.execute("UPDATE turns SET usage=? WHERE id=?", (dump({"modelRequests": calls, "mock": self.settings.mock}), turn_id))

    @staticmethod
    def _totals(calls: list[dict]) -> dict:
        usage = [c["usage"] for c in calls if isinstance(c.get("usage"), dict)]
        plain = sum(u.get("input_tokens", 0) for u in usage)
        created = sum(u.get("cache_creation_input_tokens", 0) for u in usage)
        cached = sum(u.get("cache_read_input_tokens", 0) for u in usage)
        output = sum(u.get("output_tokens", 0) for u in usage)
        return {"totalTokens": plain + created + cached + output, "inputTokens": plain + created + cached,
                "cachedInputTokens": cached, "cacheWriteInputTokens": created, "outputTokens": output,
                "reasoningOutputTokens": 0}

    async def _emit_usage(self, tid: str, turn_id: str, calls: list[dict], *,
                          last: dict | None = None, window: int | None = None) -> None:
        """`last` is the context occupancy (clients draw `last.inputTokens` against
        `modelContextWindow`), so it is the final model request of the turn, not the
        turn's billing sum; `total` stays the thread's billing-style sum."""
        all_calls: list[dict] = []
        for row in self.store.db.execute("SELECT usage FROM turns WHERE thread_id=?", (tid,)):
            all_calls.extend(json.loads(row[0]).get("modelRequests", []))
        current = self._totals([{"usage": last}] if last else calls[-1:])
        context = getattr(getattr(self, 'metrics', None), 'context', {}).get(tid)
        if context:
            # Session billing totals remain separate from current context occupancy.
            current = {**self._totals([]), 'totalTokens':context['used'], 'inputTokens':context['used']}
        await self.emit(tid, "thread/tokenUsage/updated", {"threadId": tid, "turnId": turn_id,
                "tokenUsage": {"total": self._totals(all_calls), "last": current,
                               "modelContextWindow": context['maximum'] if context else window}})
