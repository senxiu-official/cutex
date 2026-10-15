from __future__ import annotations

import asyncio
import copy
from dataclasses import replace

import pytest_asyncio

from cute_claude.config import Settings
from cute_claude.engine import Engine
from cute_claude.protocol import Server
from cute_claude.provider import MockProvider
from cute_claude.store import Store
from cute_claude.tools import ToolRegistry


class Harness:
    def __init__(self, settings, provider=None):
        self.settings = settings
        self.store = Store(settings.state_dir)
        self.provider = provider or MockProvider()
        self.tools = ToolRegistry(settings)
        self.engine = Engine(settings, self.store, self.provider, self.tools)
        self.server = Server(settings, self.store, self.engine)
        self.out = []
        self.next_id = 0
        self.peer = self.server.connect(self.send)

    async def send(self, message):
        self.out.append(copy.deepcopy(message))

    async def init(self, extensions=True):
        await self.call('initialize', {'capabilities': {'claudeExtensions': extensions}})
        await self.server.handle(self.peer, {'method': 'initialized'})

    async def call(self, method, params=None, *, error=False):
        self.next_id += 1
        rid = self.next_id
        await self.server.handle(self.peer, {'id': rid, 'method': method, 'params': params or {}})
        result = next(m for m in reversed(self.out) if m.get('id') == rid)
        if error:
            return result['error']
        assert 'result' in result, result
        return result['result']

    async def thread(self):
        return (await self.call('thread/start'))['thread']['id']

    async def turn(self, tid, value):
        return (await self.call('turn/start', {'threadId': tid, 'input': [{'type': 'text', 'text': value}]}))['turn']['id']

    async def drain(self):
        while self.engine.workers:
            await asyncio.wait_for(asyncio.gather(*list(self.engine.workers.values())), 5)

    async def close(self):
        await self.engine.close()
        self.store.close()


@pytest_asyncio.fixture
async def h(tmp_path):
    workspace = tmp_path / 'workspace'
    workspace.mkdir()
    (workspace / 'hello.txt').write_text('hello from fixture')
    harness = Harness(Settings(workspace, tmp_path / 'state'))
    await harness.init()
    yield harness
    await harness.close()
