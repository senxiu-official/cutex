"""Persistent SDK owner. No SDK object escapes to an RPC handler.

One owner task connects, sends queries/interrupts and disconnects. Its one child
reader consumes receive_messages() continuously, including while idle. A Result
ends a turn, not the connection. The Engine's worker only submits commands and
awaits receipts. Never cancel the owner to implement a routine turn interrupt.
"""
from __future__ import annotations

import asyncio
import json
import os
import sys
from contextlib import suppress
from dataclasses import dataclass
from pathlib import Path
import time
from typing import Any, Awaitable, Callable
import uuid

from .provider import ProviderError
from .subscription_auth import SubscriptionSetupError


class TurnInterrupted(ProviderError):
    pass


class OutcomeUncertain(ProviderError):
    pass


def field(obj: Any, key: str, default: Any = None) -> Any:
    return obj.get(key, default) if isinstance(obj, dict) else getattr(obj, key, default)


def message_type(obj: Any) -> str:
    return obj.get('type', '') if isinstance(obj, dict) else type(obj).__name__


def safe_error(exc: BaseException, stage: str) -> ProviderError:
    """Do not expose SDK exception bodies: they may contain prompts or secrets."""
    if isinstance(exc, ProviderError):
        return exc
    if isinstance(exc, SubscriptionSetupError):
        return ProviderError(str(exc))
    kind = type(exc).__name__
    hint = ('Check the selected CLI and isolated official login.' if stage == 'connect'
            else 'Inspect the native session and tool journal; the input is not replayed.')
    return ProviderError(f'SDK {stage} failed ({kind}). {hint}')


class NativeSessionLease:
    """Cooperative lock shared by adapters using the same account home + UUID.

    Not a lock on OAuth files; it does not prevent an unrelated native CLI user
    from manually resuming the same session. Never unlink an active lock file.
    """
    def __init__(self, config_dir: Path, sid: str):
        uuid.UUID(sid)
        directory = config_dir / '.cutex-session-locks'
        directory.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.file = open(directory / f'{sid}.lock', 'a+b')
        try:
            if __import__('os').name == 'nt':
                import msvcrt
                self.file.seek(0, 2)
                if self.file.tell() == 0:
                    self.file.write(b'0'); self.file.flush()
                self.file.seek(0)
                msvcrt.locking(self.file.fileno(), msvcrt.LK_NBLCK, 1)
            else:
                import fcntl
                fcntl.flock(self.file.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            self.file.close()
            raise ProviderError('Another adapter owns this native session in the selected Claude config directory.') from None
        # The cooperative writer lock alone cannot prove that a child of a
        # crashed adapter exited. Retain one process occurrence in this same file.
        try:
            self.file.seek(0)
            raw = self.file.read(4097)
            if len(raw) > 1:
                previous = json.loads(raw[1:])
                pid = previous['pid']
                if self.process_stamp(pid) == previous['stamp']:
                    raise ProviderError(f'Prior native Claude process {pid} is still running; close that occurrence before resuming this session.')
        except BaseException:
            self.file.close()
            raise

    @staticmethod
    def process_stamp(pid: int) -> str | None:
        if sys.platform.startswith('linux'):
            try:
                raw = Path(f'/proc/{pid}/stat').read_text()
                fields = raw[raw.rfind(')') + 2:].split()
                if fields[0] == 'Z':
                    return None
                boot = Path('/proc/sys/kernel/random/boot_id').read_text().strip()
                return boot + ':' + fields[19]
            except FileNotFoundError:
                return None
        if os.name == 'nt':
            import ctypes
            from ctypes import wintypes
            kernel = ctypes.WinDLL('kernel32', use_last_error=True)
            kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
            kernel.OpenProcess.restype = wintypes.HANDLE
            kernel.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
            kernel.GetExitCodeProcess.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
            kernel.CloseHandle.argtypes = [wintypes.HANDLE]
            handle = kernel.OpenProcess(0x1000, False, pid)
            if not handle:
                if ctypes.get_last_error() == 87:
                    return None
                raise ProviderError('Cannot inspect the prior native process occurrence.')
            try:
                code = wintypes.DWORD()
                if not kernel.GetExitCodeProcess(handle, ctypes.byref(code)):
                    raise ProviderError('Cannot inspect the prior native process state.')
                if code.value != 259:
                    return None
                times = [wintypes.FILETIME() for _ in range(4)]
                if not kernel.GetProcessTimes(handle, *(ctypes.byref(t) for t in times)):
                    raise ProviderError('Cannot inspect the native process creation time.')
                return f'{times[0].dwHighDateTime}:{times[0].dwLowDateTime}'
            finally:
                kernel.CloseHandle(handle)
        raise ProviderError('Native process occurrence checking requires Linux or Windows.')

    def record_process(self, pid: int) -> None:
        stamp = self.process_stamp(pid)
        if stamp is None:
            raise ProviderError('Native child exited before its process occurrence was saved.')
        self.file.seek(0)
        self.file.truncate()
        self.file.write(b'0' + json.dumps({'pid':pid,'stamp':stamp}).encode())
        self.file.flush()
        os.fsync(self.file.fileno())

    def close(self) -> None:
        self.file.close()


@dataclass
class Run:
    turn_id: str
    # A prompt string, or one streamed SDK user message (content blocks,
    # `client_composed`); see inputs.sdk_user_message.
    prompt: str | dict
    callback: Callable[[Any], Awaitable[None]]
    future: asyncio.Future
    interrupted: bool = False


async def one_message(message: dict):
    yield message


@dataclass
class Control:
    kind: str
    future: asyncio.Future | None = None
    turn_id: str | None = None
    model: str | None = None
    effort: str | None = None
    permission_mode: str | None = None
    arguments: dict | None = None


@dataclass
class Event:
    message: Any


@dataclass
class End:
    error: BaseException | None = None


class RuntimeOwner:
    def __init__(self, driver, engine, tid: str, session: dict):
        self.driver, self.engine, self.tid = driver, engine, tid
        self.session_id = session['session_id']
        self.resume = bool(session['attempted'])
        self.mailbox: asyncio.Queue = asyncio.Queue(maxsize=256)
        self.ready = asyncio.get_running_loop().create_future()
        # Observe otherwise-unawaited startup failures when a controller vanishes.
        self.ready.add_done_callback(self._observe)
        self.active: Run | None = None
        self.interrupt_waiters: list[asyncio.Future] = []
        self.cancelled_turns: set[str] = set()
        self.interrupt_deadline: float | None = None
        self.client = self.bridge = self.reader = self.process = self.lease = None
        self.accepting = True
        self.state = 'starting'
        self.last_error: str | None = None
        self.connection_id = str(uuid.uuid4())
        self.connected_at: int | None = None
        self.identity_observed = False
        self.model_catalog: dict[str, str] = {}
        self.model_efforts: dict[str, list[str]] = {}
        self.native_connection_generation = 1
        # A -> B -> A must not confirm A using evidence from before either change.
        self.model_selection_id = str(uuid.uuid4())
        self.process_exit_confirmed: bool | None = None
        self.reconnect_attempts = 0
        from .runtime_tasks import RuntimeTasks
        self.tasks = RuntimeTasks(engine, tid, self.session_id)
        self.task = asyncio.create_task(self._own(), name=f'claude-sdk-owner:{tid}')

    @staticmethod
    def _observe(future: asyncio.Future) -> None:
        if not future.cancelled():
            future.exception()

    def process_observation(self) -> tuple[int | None, int | None]:
        # SDK process handles may already be closed. Diagnostics must remain
        # readable after failure; an unreadable handle is not proof of exit.
        try:
            pid = getattr(self.process, 'pid', None)
        except (ValueError, OSError):
            pid = None
        try:
            code = getattr(self.process, 'returncode', None)
        except (ValueError, OSError):
            code = None
        return pid, code

    def snapshot(self) -> dict:
        pid, code = self.process_observation()
        return {'threadId': self.tid, 'sessionId': self.session_id,
                'model': self.engine.store.thread(self.tid)['model'], 'effort': self.engine.thread_effort(self.tid),
                'nativeConnectionGeneration': self.native_connection_generation,
                'connectionId': self.connection_id, 'state': self.state,
                'processId': pid if type(pid) is int else None,
                'processExitCode': code if type(code) is int else None,
                'processExitConfirmed': self.process_exit_confirmed,
                'connectedAtMs': self.connected_at, 'nativeIdentityObserved': self.identity_observed,
                'activeTurnId': self.active.turn_id if self.active else None,
                'lastError': self.last_error, 'automaticReconnect': True,
                'reconnectAttempts': self.reconnect_attempts,
                'pidObservation': 'sdk-private-diagnostic-only',
                'modelObservation': self.engine.model_status(self.tid),
                'ownerTaskAlive': not self.task.done()}

    async def status(self, state: str) -> None:
        self.state = state
        await self.engine.emit(self.tid, 'claude/runtime/statusChanged', self.snapshot())

    async def submit(self, turn_id: str, prompt: str, callback) -> None:
        await asyncio.shield(self.ready)
        if not self.accepting or self.task.done():
            raise ProviderError('Runtime is not accepting input; explicitly start it again. No input was replayed.')
        future = asyncio.get_running_loop().create_future()
        future.add_done_callback(self._observe)
        await self.mailbox.put(Run(turn_id, prompt, callback, future))
        # Cancelling an RPC/Engine waiter must not cancel the SDK owner/future.
        try:
            await asyncio.shield(future)
        except asyncio.CancelledError:
            await self.interrupt(turn_id)
            raise

    async def read_metrics(self, kind: str) -> dict:
        if not self.ready.done() or self.task.done() or not self.accepting:
            raise ProviderError('Runtime metrics unavailable while offline or starting')
        future = asyncio.get_running_loop().create_future()
        await self.mailbox.put(Control(kind, future))
        # Cancelling this read cancels only its reply, not the runtime owner.
        return await asyncio.wait_for(future, timeout=4)

    async def control(self, kind: str, **arguments):
        if kind not in {'mcp_status', 'mcp_reconnect', 'mcp_toggle', 'task_list', 'task_stop', 'rewind_files'}:
            raise ProviderError('Unknown native runtime control operation')
        await asyncio.shield(self.ready)
        if not self.accepting or self.task.done():
            raise ProviderError('Runtime is offline; start it before using native controls')
        future = asyncio.get_running_loop().create_future()
        future.add_done_callback(self._observe)
        await self.mailbox.put(Control(kind, future, arguments=arguments))
        return await asyncio.shield(future)

    async def set_permission_mode(self, mode: str) -> None:
        await asyncio.shield(self.ready)
        if not self.accepting or self.task.done():
            raise ProviderError('Runtime is offline; start it before changing permissions')
        future = asyncio.get_running_loop().create_future()
        future.add_done_callback(self._observe)
        await self.mailbox.put(Control('permissions', future, permission_mode=mode))
        await asyncio.shield(future)

    async def set_model(self, model: str) -> None:
        await asyncio.shield(self.ready)
        if not self.accepting or self.task.done():
            raise ProviderError('Runtime is offline; start it before changing model')
        future = asyncio.get_running_loop().create_future()
        future.add_done_callback(self._observe)
        await self.mailbox.put(Control('model', future, model=model))
        await asyncio.shield(future)

    async def set_model_effort(self, model: str, effort: str | None) -> None:
        await asyncio.shield(self.ready)
        if not self.accepting or self.task.done():
            raise ProviderError('Runtime is offline; start it before changing effort')
        future = asyncio.get_running_loop().create_future()
        future.add_done_callback(self._observe)
        await self.mailbox.put(Control('model_effort', future, model=model, effort=effort))
        await asyncio.shield(future)

    async def interrupt(self, turn_id: str) -> None:
        if self.task.done():
            return
        future = asyncio.get_running_loop().create_future()
        await self.mailbox.put(Control('interrupt', future, turn_id))
        await asyncio.shield(future)

    async def close(self) -> dict:
        self.accepting = False
        if not self.task.done():
            await self.mailbox.put(Control('close'))
        # Do not wrap disconnect in asyncio.wait_for or task.cancel(): the
        # official transport warns raw asyncio cancellation can skip reaping.
        await asyncio.shield(self.task)
        return self.snapshot()

    async def _read(self) -> None:
        """The only application reader for this client. No ResultMessage break."""
        try:
            async for message in self.client.receive_messages():
                await self.mailbox.put(Event(message))
        except asyncio.CancelledError:
            raise
        except BaseException as exc:
            await self.mailbox.put(End(exc))
        else:
            await self.mailbox.put(End())

    def _check_identity(self, message: Any) -> None:
        sid = field(message, 'session_id')
        if message_type(message) in {'system', 'SystemMessage'}:
            sid = field(message, 'data', {}).get('session_id', sid)
        if sid and sid != self.session_id:
            raise OutcomeUncertain('Official SDK returned a different native session ID; no identity switch or replay.')
        if sid == self.session_id:
            self.identity_observed = True
        if message_type(message) in {'ConversationResetMessage', 'conversation_reset'}:
            raise OutcomeUncertain('Native conversation reset is not supported; explicitly inspect/create another thread.')

    async def _event(self, message: Any) -> None:
        self._check_identity(message)
        typ = message_type(message)
        if await self.tasks.observe(message, self.active.turn_id if self.active else None):
            return
        # Subagent text and reasoning must never become the parent's reply.
        # Native hooks still enforce the same tool/permission policy for children.
        if field(message, 'parent_tool_use_id'):
            return
        if typ in {'SystemMessage', 'system'} and field(message, 'subtype') == 'init':
            names = field(message, 'data', {}).get('tools')
            if isinstance(names, list):
                self.engine.native_tool_inventory[self.tid] = [name[:200] for name in names[:512] if isinstance(name, str)]
        if typ in {'RateLimitEvent', 'rate_limit_event'}:
            row = self.engine.rate_limits.observe(message)
            if row is not None:
                snapshot = self.engine.rate_limits.response()
                await self.engine.emit(self.tid, 'account/rateLimits/updated', {'rateLimits': snapshot['rateLimits']})
            return
        # A compaction boundary inside a running turn (/compact or auto-compact)
        # belongs to that turn's timeline.
        if (typ in {'SystemMessage', 'system'} and field(message, 'subtype') == 'compact_boundary'
                and self.active and not self.active.interrupted):
            await self.active.callback(message)
            return
        # Background/status events are session-scoped, never guessed to belong
        # to the next user turn. Do not save their raw text or data in diagnostics.
        ordinary = {'StreamEvent', 'stream_event', 'AssistantMessage', 'assistant', 'UserMessage', 'user', 'ResultMessage', 'result'}
        if typ not in ordinary or not self.active:
            await self.engine.emit(self.tid, 'claude/runtime/event', {
                'threadId': self.tid, 'sessionId': self.session_id,
                'connectionId': self.connection_id, 'kind': typ[:80],
                'whileIdle': self.active is None, 'rawContentRetained': False,
                'status': field(message, 'status') if field(message, 'status') in
                    {'running', 'completed', 'failed', 'stopped', 'cancelled'} else None})
            return
        active = self.active
        terminal = typ in {'ResultMessage', 'result'}
        if typ in {'AssistantMessage','assistant','UserMessage','user'}:
            await self.bridge.native.message(message)
        if terminal:
            if field(message, 'session_id') != self.session_id:
                raise OutcomeUncertain('Terminal result omitted the bound native session ID.')
            # Close tool-context admission before callbacks emit/await receipts.
            # A late PostToolUse cannot claim another input at the turn boundary.
            self.bridge.accepting = False
            try:
                await active.callback(message)
            except ProviderError:
                if not active.interrupted:
                    raise
            await self.bridge.end_turn()
            self.active = None
            self.reconnect_attempts = 0
            self.interrupt_deadline = None
            await self.status('waiting')
            if not active.future.done():
                if active.interrupted:
                    active.future.set_exception(TurnInterrupted('Turn interrupted; terminal result drained, client retained.'))
                else:
                    active.future.set_result(None)
            self._complete_interrupts()
        else:
            await active.callback(message)

    def _complete_interrupts(self) -> None:
        for future in self.interrupt_waiters:
            if not future.done(): future.set_result(None)
        self.interrupt_waiters.clear()

    async def _command(self, command: Run | Control) -> bool:
        if isinstance(command, Run):
            if command.turn_id in self.cancelled_turns:
                self.cancelled_turns.discard(command.turn_id)
                command.future.set_exception(TurnInterrupted('Interrupted before query submission.'))
                return True
            if self.active or not self.accepting:
                command.future.set_exception(ProviderError('Runtime busy or closing; request was not submitted.'))
                return True
            self.engine.metrics.context.pop(self.tid, None)
            self.active = command
            self.bridge.begin_turn(command.turn_id)
            await self.status('working')
            # Native UUID was allocated durably before connect; attempted means
            # "may have reached query", not a model-persisted delivery receipt.
            self.engine.store.db.execute('UPDATE sdk_sessions SET attempted=1 WHERE thread_id=?', (self.tid,))
            try:
                async with asyncio.timeout(self.engine.settings.api_timeout):
                    prompt = command.prompt
                    await self.client.query(one_message(prompt) if isinstance(prompt, dict) else prompt)
            except BaseException as exc:
                raise OutcomeUncertain(str(safe_error(exc, 'query'))) from None
            return True
        if command.kind in {'mcp_status', 'mcp_reconnect', 'mcp_toggle', 'task_list', 'task_stop', 'rewind_files'}:
            args = command.arguments or {}
            try:
                if command.kind in {'mcp_reconnect', 'mcp_toggle', 'rewind_files'} and (
                        self.active or self.engine.store.user_turn_busy(self.tid)):
                    raise ProviderError('Finish or interrupt the turn before changing native runtime state')
                if command.kind == 'task_list':
                    result = self.tasks.snapshot()
                else:
                    async with asyncio.timeout(self.driver.control_timeout):
                        if command.kind == 'mcp_status':
                            result = await self.client.get_mcp_status()
                        elif command.kind == 'mcp_reconnect':
                            result = await self.client.reconnect_mcp_server(args['server_name'])
                        elif command.kind == 'mcp_toggle':
                            result = await self.client.toggle_mcp_server(args['server_name'], args['enabled'])
                        elif command.kind == 'task_stop':
                            self.tasks.require_running(args['task_id'])
                            result = await self.client.stop_task(args['task_id'])
                        else:
                            result = await self.client.rewind_files(args['user_message_id'])
                command.future.set_result(result)
            except Exception as exc:
                command.future.set_exception(safe_error(exc, command.kind))
            return True
        if command.kind in {'context_usage', 'plan_usage'}:
            if command.future.done():
                return True
            try:
                async with asyncio.timeout(3):
                    if command.kind == 'context_usage':
                        result = await self.client.get_context_usage()
                    else:
                        # Official TypeScript SDK's experimental get_usage wire API.
                        # Python 0.2.166 still has no public wrapper; isolate this bridge here.
                        result = await self.client._query._send_control_request({'subtype': 'get_usage'})
                if not command.future.done():
                    command.future.set_result(result)
            except Exception as exc:
                if not command.future.done():
                    command.future.set_exception(ProviderError(f'Metrics unavailable ({type(exc).__name__})'))
            return True
        if command.kind == 'model_effort':
            if self.active or not self.accepting or self.engine.store.user_turn_busy(self.tid):
                command.future.set_exception(ProviderError('Finish or interrupt the current turn before changing effort'))
                return True
            try:
                # Keep this owner and its native UUID/lease. Only replace the
                # SDK child, in its owning task, after confirmed idle teardown.
                self.bridge.revoke()
                await self.bridge.cancel_tools()
                self.reader.cancel()
                await asyncio.gather(self.reader, return_exceptions=True)
                await self.client.disconnect()
                if self.process is None or self.process.returncode is None:
                    raise ProviderError('Prior Claude child exit is unverified; effort was not changed')
                await self.bridge.end_turn()
                queued = []
                while not self.mailbox.empty():
                    item = self.mailbox.get_nowait()
                    if not isinstance(item, (Event, End)):
                        queued.append(item)
                for item in queued:
                    self.mailbox.put_nowait(item)
                self.engine.save_model_effort(self.tid, command.model, command.effort)
                session = self.engine.session(self.tid)
                self.resume = bool(session['attempted'])
                self.client, self.bridge = await self.driver.make_client(self.engine, self.tid, self.session_id, self.resume)
                self.process = None
                async with asyncio.timeout(self.driver.connect_timeout):
                    await self.client.connect()
                self.process = getattr(getattr(self.client, '_transport', None), '_process', None)
                if self.process is not None:
                    self.lease.record_process(self.process.pid)
                self.native_connection_generation += 1
                self.connected_at = int(time.time() * 1000)
                self.reader = asyncio.create_task(self._read(), name=f'claude-sdk-reader:{self.tid}')
                await self.status('waiting')
                command.future.set_result(None)
            except BaseException as exc:
                error = safe_error(exc, 'reconnect-for-effort')
                if not command.future.done():
                    command.future.set_exception(error)
                raise OutcomeUncertain(str(error)) from None
            return True
        if command.kind == 'permissions':
            if self.active or not self.accepting:
                command.future.set_exception(ProviderError('Finish or interrupt the turn before changing permissions'))
                return True
            try:
                async with asyncio.timeout(self.driver.control_timeout):
                    await self.client.set_permission_mode(command.permission_mode)
                command.future.set_result(None)
            except Exception as exc:
                command.future.set_exception(safe_error(exc, 'set_permission_mode'))
            return True
        if command.kind == 'model':
            if self.active or not self.accepting:
                command.future.set_exception(ProviderError('Finish or interrupt the current turn before changing model'))
                return True
            try:
                async with asyncio.timeout(self.driver.control_timeout):
                    await self.client.set_model(command.model)
                self.engine.store.set_thread_model(self.tid, command.model)
                self.model_selection_id = str(uuid.uuid4())
                command.future.set_result(None)
                await self.status('waiting')
            except BaseException as exc:
                error = safe_error(exc, 'set_model')
                if not command.future.done(): command.future.set_exception(error)
                raise OutcomeUncertain(str(error)) from None
            return True
        if command.kind == 'close':
            return False
        if command.kind == 'interrupt':
            if not self.active:
                # Race: Engine claimed a turn but run() is still waiting on
                # connect. The queued command must not later submit that turn.
                self.cancelled_turns.add(command.turn_id)
                if command.future and not command.future.done(): command.future.set_result(None)
                return True
            if command.turn_id != self.active.turn_id:
                if command.future and not command.future.done(): command.future.set_result(None)
                return True
            if command.future: self.interrupt_waiters.append(command.future)
            if self.active.interrupted:
                return True
            self.active.interrupted = True
            self.bridge.revoke()
            await self.status('interrupting')
            await self.bridge.cancel_tools()
            try:
                async with asyncio.timeout(self.driver.control_timeout):
                    await self.client.interrupt()
            except BaseException as exc:
                raise OutcomeUncertain(str(safe_error(exc, 'interrupt'))) from None
            self.interrupt_deadline = asyncio.get_running_loop().time() + self.driver.interrupt_timeout
            return True
        raise ProviderError('Unknown runtime control operation.')

    async def _recover(self, failure: BaseException) -> None:
        """Replace a failed child, never a submitted prompt, under the same lease.

        Only one consecutive recovery is attempted. A successful terminal result
        resets this budget; an immediately dying replacement cannot spin forever.
        The logical connection ID stays stable for Cutex's observation binding.
        """
        if not self.accepting or self.reconnect_attempts >= 1:
            raise failure
        self.reconnect_attempts += 1
        self.last_error = str(failure)[:512]
        await self.status('reconnecting')
        self.bridge.revoke()
        await self.bridge.cancel_tools()
        self.reader.cancel()
        await asyncio.gather(self.reader, return_exceptions=True)
        if self.process is None:
            self.process = getattr(getattr(self.client, '_transport', None), '_process', None)
        await self.client.disconnect()
        if self.process is None or self.process_observation()[1] is None:
            raise OutcomeUncertain('Prior native child exit is unverified; supervised reconnect refused, input not replayed.')
        self.process_exit_confirmed = True
        await self.bridge.end_turn()
        self.tasks.disconnected()
        if not self.accepting:
            raise failure
        # Reader frames from the dead generation can never finish a new turn.
        # Inputs already queued in this mailbox are failed, never replayed.
        controls = []
        while not self.mailbox.empty():
            pending = self.mailbox.get_nowait()
            if isinstance(pending, Run):
                if not pending.future.done(): pending.future.set_exception(failure)
            elif isinstance(pending, Control):
                controls.append(pending)
        for pending in controls:
            self.mailbox.put_nowait(pending)
        session = self.engine.session(self.tid)
        self.resume = bool(session['attempted'])
        self.client, self.bridge = await self.driver.make_client(self.engine, self.tid, self.session_id, self.resume)
        self.process = None
        self.process_exit_confirmed = None
        async with asyncio.timeout(self.driver.connect_timeout):
            await self.client.connect()
        self.process = getattr(getattr(self.client, '_transport', None), '_process', None)
        if self.process is None:
            raise OutcomeUncertain('Replacement child has no observable process; refusing an unleased writer.')
        self.lease.record_process(self.process.pid)
        self.process_exit_confirmed = None
        self.native_connection_generation += 1
        self.connected_at = int(time.time() * 1000)
        self.identity_observed = False
        self.reader = asyncio.create_task(self._read(), name=f'claude-sdk-reader:{self.tid}')
        active, self.active = self.active, None
        self.interrupt_deadline = None
        await self.status('waiting')
        if active and not active.future.done():
            active.future.set_exception(failure)
        self._complete_interrupts()

    async def _own(self) -> None:
        failure: BaseException | None = None
        stage = 'connect'
        connected = False
        connect_attempted = False
        try:
            self.lease = NativeSessionLease(self.driver.config_dir, self.session_id)
            self.client, self.bridge = await self.driver.make_client(self.engine, self.tid, self.session_id, self.resume)
            connect_attempted = True
            async with asyncio.timeout(self.driver.connect_timeout):
                await self.client.connect()
            connected = True
            # Public SDK initialization metadata; no prompt or extra subprocess.
            info_method = getattr(self.client, 'get_server_info', None)
            if info_method is not None:
                info = await info_method()
                models = info.get('models', []) if isinstance(info, dict) else []
                if isinstance(models, list):
                    for model in models[:128]:
                        if not isinstance(model, dict):
                            continue
                        value = model.get('value')
                        label = model.get('displayName', value)
                        if (isinstance(value, str) and 0 < len(value) <= 256
                                and isinstance(label, str) and 0 < len(label) <= 256
                                and not any(ord(c) < 32 for c in value + label)):
                            self.model_catalog[value] = label
                            levels = model.get('supportedEffortLevels')
                            if isinstance(levels, list):
                                self.model_efforts[value] = [x for x in levels if isinstance(x, str) and x in {'low', 'medium', 'high', 'xhigh', 'max'}]
            # Not a control dependency: the process object is retained only to
            # distinguish SDK disconnect from observed child exit. Version-pinned
            # internal accessor, absent -> close_unverified, never fake success.
            self.process = getattr(getattr(self.client, '_transport', None), '_process', None)
            if self.process is not None:
                self.lease.record_process(self.process.pid)
            self.connected_at = int(time.time() * 1000)
            self.reader = asyncio.create_task(self._read(), name=f'claude-sdk-reader:{self.tid}')
            await self.status('waiting')
            self.ready.set_result(None)
            stage = 'stream'
            while True:
                remaining = (max(0, self.interrupt_deadline - asyncio.get_running_loop().time())
                             if self.interrupt_deadline is not None else None)
                try:
                    async with asyncio.timeout(remaining):
                        command = await self.mailbox.get()
                except TimeoutError:
                    raise OutcomeUncertain('Interrupt result was not drained before deadline; connection closed, input not replayed.') from None
                if isinstance(command, Event):
                    await self._event(command.message)
                elif isinstance(command, End):
                    error = safe_error(command.error, 'receive') if command.error else ProviderError('SDK stream ended unexpectedly')
                    await self._recover(OutcomeUncertain(f'{error}; input not replayed.'))
                else:
                    try:
                        if not await self._command(command):
                            break
                    except OutcomeUncertain as exc:
                        if not isinstance(command, Run):
                            raise
                        await self._recover(exc)
        except asyncio.CancelledError:
            failure = OutcomeUncertain('Runtime owner was unexpectedly cancelled; inspect effects, no replay.')
        except BaseException as exc:
            failure = safe_error(exc, stage)
        finally:
            self.accepting = False
            # Cleanup failures must not skip the remaining cleanup steps or
            # strand Engine futures. Keep lifecycle calls in this owner task.
            def record_cleanup(exc: BaseException, at: str) -> None:
                nonlocal failure
                failure = failure or safe_error(exc, at)
                self.last_error = str(failure)[:512]

            if failure:
                self.last_error = str(failure)[:512]
            try:
                await self.status('closing')
            except BaseException as exc:
                record_cleanup(exc, 'publish-close')
            tools_stopped = True
            if self.bridge:
                self.bridge.revoke()
                try:
                    await self.bridge.cancel_tools()
                except BaseException as exc:
                    tools_stopped = False
                    record_cleanup(exc, 'cancel-tools')
            if self.reader:
                self.reader.cancel()
                await asyncio.gather(self.reader, return_exceptions=True)
            if self.client:
                # Capture even a partially started child before SDK clears its
                # private handle. PID observation is not a public SDK guarantee.
                if self.process is None:
                    self.process = getattr(getattr(self.client, '_transport', None), '_process', None)
                try:
                    await self.client.disconnect()
                except BaseException as exc:
                    record_cleanup(exc, 'disconnect')
            if self.process is not None:
                self.process_exit_confirmed = self.process_observation()[1] is not None
            if self.bridge:
                try:
                    await self.bridge.end_turn()
                except BaseException as exc:
                    tools_stopped = False
                    record_cleanup(exc, 'end-tools')
            no_child_attempted = not connect_attempted
            closed = (self.process_exit_confirmed is True or no_child_attempted) and tools_stopped
            if self.lease and closed:
                self.lease.close(); self.lease = None
            # Closing mid-turn without draining Result is NOT proof of a clean
            # interrupted result. Surface unknown outcome, never auto-resubmit.
            active, self.active = self.active, None
            self.state = 'closed' if closed else 'close_unverified'
            if not closed and not self.last_error:
                self.last_error = 'Child exit or host-tool cleanup could not be verified; another writer is blocked.'
            if not self.ready.done():
                self.ready.set_exception(failure or ProviderError('Runtime closed before connect completed.'))
            if active and not active.future.done():
                active.future.set_exception(failure or OutcomeUncertain('Runtime closed before terminal result; inspect effects, no replay.'))
            self._complete_interrupts()
            while not self.mailbox.empty():
                pending = self.mailbox.get_nowait()
                future = getattr(pending, 'future', None)
                if future and not future.done():
                    if isinstance(pending, Run):
                        future.set_exception(failure or TurnInterrupted('Runtime closed before query submission.'))
                    elif isinstance(pending, Control) and pending.kind not in {'close', 'interrupt'}:
                        # A queued control has never reached the SDK. In
                        # particular, a file rewind must not receive a success
                        # receipt that lets a history mutation commit afterwards.
                        future.set_exception(ProviderError(
                            f'Runtime closed before control {pending.kind!r} was executed.'))
                    else:
                        future.set_result(None)
            try:
                await self.status(self.state)
            except BaseException as exc:
                record_cleanup(exc, 'publish-closed')
