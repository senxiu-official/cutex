"""Small demo/control client, also used for end-to-end transport tests."""
from __future__ import annotations

import asyncio
import json
from urllib.parse import urlsplit

from websockets.asyncio.client import connect, unix_connect

from .errors import RpcError
from .store import dump
from .transport import MAX_RESPONSE


class Client:
    def __init__(self, socket):
        self.socket = socket
        self.next_id = 0
        self.pending: dict[int, asyncio.Future] = {}
        self.events = asyncio.Queue()
        self.reader = asyncio.create_task(self._read())

    @classmethod
    async def open(cls, endpoint: str, token: str) -> "Client":
        headers = {"Authorization": "Bearer " + token}
        if endpoint.startswith("unix://"):
            ws = await unix_connect(urlsplit(endpoint).path, additional_headers=headers, max_size=MAX_RESPONSE)
        else:
            ws = await connect(endpoint, additional_headers=headers, max_size=MAX_RESPONSE)
        client = cls(ws)
        await client.call("initialize", {"clientInfo": {"name": "cute-claude-demo-client", "version": "0.1.0"},
                "capabilities": {"experimentalApi": True, "claudeExtensions": True}})
        await client.socket.send(dump({"method": "initialized"}))
        return client

    async def _read(self) -> None:
        try:
            async for raw in self.socket:
                message = json.loads(raw)
                if "id" in message:
                    future = self.pending.pop(message["id"], None)
                    if future and not future.done():
                        if "error" in message:
                            future.set_exception(RpcError(**message["error"]))
                        else:
                            future.set_result(message["result"])
                else:
                    await self.events.put(message)
        finally:
            for future in self.pending.values():
                if not future.done():
                    future.set_exception(ConnectionError("app-server connection closed"))
            self.pending.clear()

    async def call(self, method: str, params: dict | None = None, timeout: float = 10) -> dict:
        self.next_id += 1
        rid = self.next_id
        future = asyncio.get_running_loop().create_future()
        self.pending[rid] = future
        try:
            await self.socket.send(dump({"id": rid, "method": method, "params": params or {}}))
            return await asyncio.wait_for(future, timeout)
        finally:
            self.pending.pop(rid, None)

    async def wait_turn(self, turn_id: str, timeout: float = 20) -> dict:
        async with asyncio.timeout(timeout):
            while True:
                event = await self.events.get()
                if event["method"] == "turn/completed" and event["params"]["turn"]["id"] == turn_id:
                    return event["params"]["turn"]

    async def close(self) -> None:
        await self.socket.close()
        await asyncio.gather(self.reader, return_exceptions=True)
