"""Confirm offered inputs from newly appended native context attachments only.

No SDK output or hook return is a persistence receipt. Readers exist only while
offers are pending; unavailable, replaced or oversized transcripts leave the
ordinary terminal acknowledgement as the fallback.

Bounds: 256 KiB per read, 16 MiB per line, 256 pending offers (five-minute
expiry), and 1024 parent identities with at most 256 tool IDs each. Terminal
flush snapshots EOF and drains at most 32 MiB of that existing tail, yielding
between chunks. Later appends and incomplete lines wait for another read;
close discards unconfirmed evidence. Limits never imply successful delivery.
"""
from __future__ import annotations

import asyncio
from dataclasses import dataclass, field
import hashlib
import json
import os
import time
import uuid

from .session_ops import NativeSessionApi

POLL_SECONDS = 0.05
MAX_CHUNK_BYTES = 256 * 1024
# A valid batch can contain 128 owner inputs of 64 KiB each, plus framing.
MAX_LINE_BYTES = 16 * 1024 * 1024
MAX_FLUSH_BYTES = 32 * 1024 * 1024
MAX_OFFERS = 256
MAX_PARENTS = 1024
MAX_PARENT_CALLS = 256
MAX_PENDING_SECONDS = 300


@dataclass(eq=False)
class _Offer:
    call_id: str
    hook_name: str
    mids: tuple[str, ...]
    context: str
    boundary: int
    expires: float


@dataclass(eq=False)
class _Reader:
    tid: str
    bridge: object
    owner: object
    driver: object
    binding: tuple
    sid: str
    connection: object
    generation: object
    api: NativeSessionApi
    path: object = None
    identity: object = None
    anchor: bytes = b''
    offset: int = 0
    line_start: int = 0
    buffer: bytes = b''
    skip_line: bool = False
    offers: list = field(default_factory=list)
    parents: dict = field(default_factory=dict)
    lock: asyncio.Lock = field(default_factory=asyncio.Lock)
    flush_lock: asyncio.Lock = field(default_factory=asyncio.Lock)
    flushing: bool = False
    task: object = None
    stopped: bool = False


class ContextReceipts:
    def __init__(self, server, acknowledge_async):
        self.server = server
        self.acknowledge = acknowledge_async
        self.readers: dict[str, _Reader] = {}
        self._tasks: dict[asyncio.Task, str] = {}

    def _valid(self, reader):
        driver = self.server.engine.driver
        return (not reader.stopped and driver is reader.driver
                and driver.owners.get(reader.tid) is reader.owner
                and reader.owner.connection_id == reader.connection
                and reader.owner.native_connection_generation == reader.generation
                and reader.owner.session_id == reader.sid
                and reader.owner.bridge is reader.bridge
                and reader.bridge.sid == reader.sid
                and reader.bridge.binding == reader.binding)

    def _stop(self, reader):
        reader.stopped = True
        reader.offers.clear()
        reader.buffer = b''
        reader.parents.clear()
        if self.readers.get(reader.tid) is reader:
            self.readers.pop(reader.tid, None)
        if reader.task is not None and reader.task is not asyncio.current_task():
            reader.task.cancel()

    @staticmethod
    def _anchor(stream, offset):
        stream.seek(max(0, offset - 64))
        return hashlib.sha256(stream.read(min(64, offset))).digest()

    def offer(self, tid, bridge, call_id, message_ids, context, *, hook_name):
        """Capture EOF and ownership synchronously before the hook returns."""
        try:
            binding = bridge.binding
            mids = tuple(dict.fromkeys(message_ids))
            if (not isinstance(binding, tuple) or len(binding) != 2
                    or not all(isinstance(x, str) and x for x in binding)
                    or not isinstance(call_id, str) or not call_id
                    or not isinstance(hook_name, str) or not hook_name.startswith('PostToolUse:')
                    or not hook_name[len('PostToolUse:'):].strip()
                    or not isinstance(context, str) or not context
                    or not mids or not all(isinstance(x, str) and x for x in mids)):
                return
            driver = self.server.engine.driver
            owner = driver.owners[tid]
            sid = bridge.sid
            uuid.UUID(sid)
            reader = self.readers.get(tid)
            if reader and (not self._valid(reader) or reader.bridge is not bridge
                           or reader.binding != binding):
                self._stop(reader)
                if reader.task:
                    reader.task.cancel()
                reader = None
            api = reader.api if reader else NativeSessionApi(self.server.engine)
            path = api.transcript(sid)
            boundary, identity, skip, anchor = 0, None, False, b''
            if path is not None:
                with path.open('rb') as stream:
                    stat = os.fstat(stream.fileno())
                    boundary = stat.st_size
                    identity = (stat.st_dev, stat.st_ino)
                    anchor = self._anchor(stream, boundary)
                    if boundary:
                        stream.seek(boundary - 1)
                        skip = stream.read(1) != b'\n'
            if reader is None:
                reader = _Reader(tid, bridge, owner, driver, binding, sid,
                                 owner.connection_id, owner.native_connection_generation, api,
                                 path=path, identity=identity, anchor=anchor, offset=boundary,
                                 line_start=boundary, skip_line=skip)
                if not self._valid(reader):
                    return
            elif (reader.path is not None and path != reader.path
                  or reader.identity is not None and identity != reader.identity
                  or boundary < reader.offset):
                self._stop(reader)
                return
            if len(reader.offers) >= MAX_OFFERS:
                return
            reader.offers.append(_Offer(call_id, hook_name, mids, context, boundary,
                                        time.monotonic() + MAX_PENDING_SECONDS))
            self.readers[tid] = reader
            if reader.task is None:
                reader.task = asyncio.create_task(self._watch(reader), name=f'claude-context-receipts:{tid}')
                self._tasks[reader.task] = tid
                reader.task.add_done_callback(self._task_done)
        except Exception:
            # Observation must never break the hook or manufacture a receipt.
            if tid in self.readers:
                self._stop(self.readers[tid])

    def _task_done(self, task):
        self._tasks.pop(task, None)
        if not task.cancelled():
            task.exception()

    async def _watch(self, reader):
        try:
            while reader.offers and self._valid(reader):
                if not reader.flushing:
                    await self._read(reader)
                if reader.offers and self._valid(reader):
                    await asyncio.sleep(POLL_SECONDS)
        except asyncio.CancelledError:
            raise
        except Exception:
            pass  # Terminal acknowledgement remains available.
        finally:
            self._stop(reader)

    @staticmethod
    def _uuid(value):
        try:
            return isinstance(value, str) and bool(uuid.UUID(value))
        except (ValueError, AttributeError):
            return False

    @staticmethod
    def _mainchain(record, reader):
        return (isinstance(record, dict) and record.get('sessionId') == reader.sid
                and record.get('isSidechain') is False
                and not record.get('agentId') and not record.get('parent_tool_use_id')
                and ContextReceipts._uuid(record.get('uuid')))

    @staticmethod
    def _matches(record, reader, offer, start):
        if not ContextReceipts._mainchain(record, reader) or start < offer.boundary:
            return False
        parent = reader.parents.get(record.get('parentUuid'))
        if parent is None or parent[0] < offer.boundary or offer.call_id not in parent[1]:
            return False
        attachment = record.get('attachment')
        return (record.get('type') == 'attachment'
                and isinstance(attachment, dict)
                and attachment.get('type') == 'hook_additional_context'
                and attachment.get('hookEvent') == 'PostToolUse'
                and attachment.get('hookName') == offer.hook_name
                and attachment.get('toolUseID') == offer.call_id
                and isinstance(attachment.get('content'), list)
                and all(isinstance(x, str) for x in attachment['content'])
                and offer.context in attachment['content'])

    async def _read(self, reader, *, stop_at=None):
        async with reader.lock:
            if not self._valid(reader):
                self._stop(reader)
                return
            reader.offers[:] = [o for o in reader.offers if o.expires > time.monotonic()]
            if not reader.offers:
                return
            path = reader.api.transcript(reader.sid)
            if path is None:
                if reader.path is not None:
                    self._stop(reader)
                return
            if reader.path is not None and path != reader.path:
                self._stop(reader)
                return
            with path.open('rb') as stream:
                stat = os.fstat(stream.fileno())
                identity = (stat.st_dev, stat.st_ino)
                if (reader.identity is not None and identity != reader.identity
                        or stat.st_size < reader.offset
                        or reader.anchor and self._anchor(stream, reader.offset) != reader.anchor):
                    self._stop(reader)
                    return
                reader.path, reader.identity = path, identity
                stream.seek(reader.offset)
                limit = MAX_CHUNK_BYTES if stop_at is None else min(MAX_CHUNK_BYTES, max(0, stop_at - reader.offset))
                chunk = stream.read(limit)
                reader.anchor = self._anchor(stream, reader.offset + len(chunk))
            reader.offset += len(chunk)
            reader.buffer += chunk
            while b'\n' in reader.buffer:
                line, reader.buffer = reader.buffer.split(b'\n', 1)
                start = reader.line_start
                reader.line_start += len(line) + 1
                if len(line) > MAX_LINE_BYTES:
                    self._stop(reader)
                    return
                if reader.skip_line:
                    reader.skip_line = False
                    continue
                try:
                    record = json.loads(line.decode('utf-8'))
                except (ValueError, UnicodeError):
                    continue
                if self._mainchain(record, reader) and record.get('type') == 'user':
                    message = record.get('message')
                    content = (message.get('content') if isinstance(message, dict)
                               and message.get('role') == 'user' else None)
                    if isinstance(content, list):
                        calls = frozenset(b['tool_use_id'] for b in content
                                          if isinstance(b, dict) and b.get('type') == 'tool_result'
                                          and isinstance(b.get('tool_use_id'), str))
                        if len(calls) > MAX_PARENT_CALLS:
                            self._stop(reader)
                            return
                        native_uuid = record['uuid']
                        if calls and native_uuid not in reader.parents:
                            if len(reader.parents) >= MAX_PARENTS:
                                reader.parents.pop(next(iter(reader.parents)))
                            reader.parents[native_uuid] = (start, calls)
                for offer in list(reader.offers):
                    if not self._matches(record, reader, offer, start):
                        continue
                    if not self._valid(reader):
                        self._stop(reader)
                        return
                    reader.offers.remove(offer)
                    await self.acknowledge(reader.tid, reader.binding[0], list(offer.mids))
                    if not self._valid(reader):
                        self._stop(reader)
                        return
            if len(reader.buffer) > MAX_LINE_BYTES:
                self._stop(reader)

    async def flush(self, tid, turn_id):
        """Drain a bounded EOF snapshot before the terminal callback ends binding.

        Do not chase concurrent appends or let the watcher read beyond this
        snapshot while flushing. Each chunk revalidates native ownership.
        """
        reader = self.readers.get(tid)
        if reader is not None and reader.binding[0] == turn_id:
            async with reader.flush_lock:
                reader.flushing = True
                try:
                    async with reader.lock:
                        if not self._valid(reader):
                            self._stop(reader)
                            return
                        path = reader.api.transcript(reader.sid)
                        if path is None:
                            if reader.path is not None:
                                self._stop(reader)
                            return
                        with path.open('rb') as stream:
                            eof = os.fstat(stream.fileno()).st_size
                        stop_at = min(eof, reader.offset + MAX_FLUSH_BYTES)
                    # Read once even at EOF to validate truncation/expiry.
                    while True:
                        previous = reader.offset
                        await self._read(reader, stop_at=stop_at)
                        if (not reader.offers or not self._valid(reader)
                                or reader.offset >= stop_at or reader.offset <= previous):
                            break
                        await asyncio.sleep(0)
                except Exception:
                    self._stop(reader)
                finally:
                    reader.flushing = False
                    if not reader.offers:
                        self._stop(reader)

    async def close(self, tid=None):
        readers = list(self.readers.values()) if tid is None else [self.readers[tid]] if tid in self.readers else []
        for reader in readers:
            self._stop(reader)
        tasks = [task for task, thread in list(self._tasks.items())
                 if (tid is None or tid == thread) and task is not asyncio.current_task()]
        for task in tasks:
            task.cancel()
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)
