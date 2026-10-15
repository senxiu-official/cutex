"""JSONL stdio and authenticated WebSocket transports (including Unix sockets)."""
from __future__ import annotations

import asyncio
import contextlib
import json
import os
import secrets
import stat
import sys
from http import HTTPStatus
from pathlib import Path
from urllib.parse import urlsplit

from websockets.asyncio.server import serve, unix_serve
from websockets.exceptions import ConnectionClosed

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .protocol import Server
from .store import dump
from .errors import RpcError

MAX_FRAME = 32 * 1024 * 1024  # incoming requests (turn/start may carry base64 images)
MAX_RESPONSE = 64 * 1024 * 1024
MAX_OUTBOX_BYTES = MAX_RESPONSE


def decode(raw: str | bytes) -> object:
    if len(raw) > MAX_FRAME:
        raise ValueError("frame exceeds byte limit")
    def invalid_constant(value: str):
        raise ValueError(f"non-JSON numeric constant: {value}")
    return json.loads(raw, parse_constant=invalid_constant)


class Outbox:
    def __init__(self, writer):
        self.writer = writer
        self.queue = asyncio.Queue(maxsize=256)
        self.pending_bytes = 0
        self.task = asyncio.create_task(self._write())

    async def send(self, message: dict) -> None:
        if self.task.done():
            raise ConnectionError("transport writer closed")
        raw = dump(message)
        size = len(raw.encode())
        if size > MAX_RESPONSE:
            raise RpcError(-32001, f"Response is {size} bytes, exceeding the {MAX_RESPONSE}-byte history response limit")
        if self.pending_bytes + size > MAX_OUTBOX_BYTES:
            self.task.cancel()
            raise ConnectionError("slow client exceeded outbound byte budget; reconnect")
        try:
            self.queue.put_nowait((raw, size))
            self.pending_bytes += size
        except asyncio.QueueFull:
            self.task.cancel()
            raise ConnectionError("slow client disconnected; reconnect and read thread state") from None

    async def _write(self) -> None:
        while True:
            raw, size = await self.queue.get()
            try:
                await self.writer(raw)
            finally:
                self.pending_bytes -= size
                self.queue.task_done()

    async def close(self) -> None:
        if not self.task.done():
            with contextlib.suppress(TimeoutError):
                await asyncio.wait_for(self.queue.join(), 1)
            self.task.cancel()
        await asyncio.gather(self.task, return_exceptions=True)


async def stdio(server: Server) -> None:
    async def write(raw: str) -> None:
        def emit():
            sys.stdout.write(raw + "\n")
            sys.stdout.flush()
        await asyncio.to_thread(emit)
    outbox = Outbox(write)
    peer = server.connect(outbox.send)
    try:
        while True:
            raw = await asyncio.to_thread(sys.stdin.buffer.readline, MAX_FRAME + 1)
            if not raw:
                break
            if len(raw) > MAX_FRAME:
                await outbox.send({"id": None, "error": {"code": -32700, "message": "frame too large; closing input"}})
                break
            try:
                message = decode(raw)
            except (ValueError, UnicodeDecodeError):
                await outbox.send({"id": None, "error": {"code": -32700, "message": "invalid JSON"}})
                continue
            await server.handle(peer, message)
    finally:
        server.disconnect(peer)
        await outbox.close()


def rpc_token(directory: Path, env_name: str) -> str:
    value = os.environ.get(env_name)
    if value:
        if len(value) < 24:
            raise ValueError("RPC token must have at least 24 characters")
        return value
    path = directory / "rpc-token"
    try:
        fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    except FileExistsError:
        info = path.lstat()
        if not stat.S_ISREG(info.st_mode) or path.is_symlink():
            raise ValueError("invalid token file")
        if os.name != "nt" and (info.st_mode & 0o077):
            raise ValueError("token file must be private (chmod 600)")
        value = path.read_text().strip()
    else:
        value = secrets.token_urlsafe(32)
        with os.fdopen(fd, "w") as f:
            f.write(value + "\n")
            f.flush()
            os.fsync(f.fileno())
    if len(value) < 24:
        raise ValueError("invalid persisted RPC token")
    print(f"RPC token file: {path} (contents are not logged)", file=sys.stderr)
    return value


async def websocket(server: Server, listen: str, token: str, stop: asyncio.Event, *, ready_file: Path | None = None) -> None:
    def publish(endpoint: str) -> None:
        if ready_file is None:
            return
        # The controller owns the private parent. Atomic replacement permits
        # restart discovery without publishing a half-written endpoint.
        temporary = ready_file.with_name(ready_file.name + "." + secrets.token_hex(8))
        try:
            with open(os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as out:
                out.write(dump({"endpoint": endpoint, "runtimeId": server.runtime_id, "pid": os.getpid()}))
                out.flush()
                os.fsync(out.fileno())
            os.replace(temporary, ready_file)
        finally:
            with contextlib.suppress(FileNotFoundError):
                temporary.unlink()

    async def authenticate(connection, request):
        if request.headers.get("Origin") is not None:
            return connection.respond(HTTPStatus.FORBIDDEN, "Browser-origin connections are not allowed\n")
        if request.path in {"/healthz", "/readyz"}:
            return connection.respond(HTTPStatus.OK, "ready\n")
        if request.path not in {"/", "/rpc"}:
            return connection.respond(HTTPStatus.NOT_FOUND, "not found\n")
        supplied = request.headers.get("Authorization", "")
        if not secrets.compare_digest(supplied, "Bearer " + token):
            return connection.respond(HTTPStatus.UNAUTHORIZED, "bearer token required\n")
        return None

    async def connected(ws):
        outbox = Outbox(ws.send)
        peer = server.connect(outbox.send)
        def writer_stopped(task):
            if not ws.close_code:
                asyncio.create_task(ws.close())
        outbox.task.add_done_callback(writer_stopped)
        try:
            async for raw in ws:
                if not isinstance(raw, str):
                    await ws.close(code=1003, reason="text frames required")
                    break
                try:
                    message = decode(raw)
                except (ValueError, UnicodeDecodeError):
                    await outbox.send({"id": None, "error": {"code": -32700, "message": "invalid JSON"}})
                    continue
                await server.handle(peer, message)
                if server.shutdown_requested:
                    await asyncio.wait_for(outbox.queue.join(), 2)
                    stop.set()
                    break
        except (ConnectionClosed, ConnectionError):
            pass
        finally:
            server.disconnect(peer)
            await outbox.close()

    parsed = urlsplit(listen)
    options = dict(process_request=authenticate, max_size=MAX_FRAME, max_queue=16, origins=[None], close_timeout=1)
    if parsed.scheme == "ws":
        if parsed.hostname not in {"127.0.0.1", "localhost", "::1"} or parsed.username or parsed.password or parsed.query or parsed.fragment or parsed.path not in {"", "/"}:
            raise ValueError("MVP listeners must be loopback ws://HOST:PORT; use an SSH tunnel for remote access")
        if parsed.port is None:
            raise ValueError("WebSocket port is required")
        async with serve(connected, parsed.hostname, parsed.port, **options) as listener:
            host = f"[{parsed.hostname}]" if ":" in parsed.hostname else parsed.hostname
            publish(f"ws://{host}:{listener.sockets[0].getsockname()[1]}")
            print(f"Listening: {listen}", file=sys.stderr)
            await stop.wait()
    elif parsed.scheme == "unix":
        if os.name == "nt":
            raise ValueError("Unix WebSocket transport in this demo is tested on POSIX only")
        path = Path(parsed.path) if parsed.path else server.settings.state_dir / "app-server.sock"
        if not path.is_absolute() or parsed.netloc or parsed.query or parsed.fragment:
            raise ValueError("use unix:///absolute/path")
        path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        if path.parent.stat().st_mode & 0o077:
            raise ValueError("Unix socket must be under a private directory (mode 700)")
        if path.exists() or path.is_symlink():
            raise ValueError("socket path already exists; verify stale owner before removing it")
        identity = None
        try:
            async with unix_serve(connected, path=str(path), **options):
                os.chmod(path, 0o600)
                publish(listen)
                identity = (path.stat().st_dev, path.stat().st_ino)
                print(f"Listening: unix://{path}", file=sys.stderr)
                await stop.wait()
        finally:
            with contextlib.suppress(FileNotFoundError):
                info = path.lstat()
                if identity == (info.st_dev, info.st_ino) and stat.S_ISSOCK(info.st_mode):
                    path.unlink()
    else:
        raise ValueError("listen must be stdio://, ws://loopback:port or unix:///absolute/path")
