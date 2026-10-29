"""Recovery of the pinned SDK's private, in-memory cutex MCP transport.

This does not reconnect a daemon or restart Claude. Failed tool calls are never
replayed. The SDK 0.2.166 bridge otherwise stays dead after Server.run returns.
The narrow private integration is covered against the real SDK, without a CLI.
"""
from __future__ import annotations

import asyncio
from copy import deepcopy
import logging
import json

log = logging.getLogger(__name__)


def cancelled_host_tool_result():
    log.warning('cutex MCP host child task cancelled; isolated to one tool result (effects uncertain)')
    return {'content': [{'type': 'text', 'text': json.dumps({
        'code': 'cutex_tool_cancelled', 'retryable': False, 'outcomeUnknown': True,
        'message': 'Host tool was cancelled; effects may be uncertain. The tool was not replayed; inspect its receipt before retrying.',
    })}], 'is_error': True}


class RecoveringMcpBridge:
    def __init__(self, bridge):
        self.bridge = bridge
        self.lock = asyncio.Lock()
        self.closed = False
        self.initialize = None
        self.recoveries = 0
        self.needs_handshake = False

    def stopped(self):
        session = self.bridge._session
        return session is not None and session.finished

    async def _recover(self):
        async with self.lock:
            if self.closed or (not self.stopped() and not self.needs_handshake):
                return
            self.needs_handshake = True
            self.recoveries += 1
            # Usually recorded from the CLI's initial handshake. A server can
            # have initialized during client.connect(), before we were installed;
            # these tool-only servers need no client capabilities on that path.
            init = deepcopy(self.initialize) if self.initialize else {
                'jsonrpc': '2.0', 'method': 'initialize', 'params': {
                    'protocolVersion': '2025-11-25', 'capabilities': {},
                    'clientInfo': {'name': 'cutex-mcp-recovery', 'version': '1'},
                },
            }
            init['id'] = f'cutex-mcp-recovery-{self.recoveries}'
            log.warning('cutex SDK MCP session stopped; rebuilding in-memory session (attempt=%d)', self.recoveries)
            async with asyncio.timeout(10):
                result = await self.bridge.handle(init)
                if not isinstance(result, dict) or 'error' in result:
                    raise RuntimeError('cutex MCP recovery handshake refused')
                await self.bridge.handle({'jsonrpc': '2.0', 'method': 'notifications/initialized'})
            self.needs_handshake = False
            log.warning('cutex SDK MCP in-memory session recovered (attempt=%d)', self.recoveries)

    @staticmethod
    def failure(message, *, unknown):
        # Once submitted, even a closed response cannot prove lack of effects.
        # A model may inspect durable receipts; no automatic mutation replay.
        detail = {
            'code': 'cutex_mcp_session_interrupted' if unknown else 'cutex_mcp_recovering',
            'retryable': not unknown, 'outcomeUnknown': unknown,
            'message': ('MCP session interrupted; this tool was not replayed. Inspect its durable receipt before retrying.'
                        if unknown else 'MCP transport recovery is pending; this request was not executed and may be retried.'),
        }
        if 'id' not in message:
            return None
        if message.get('method') == 'tools/call':
            return {'jsonrpc': '2.0', 'id': message['id'], 'result': {
                'content': [{'type': 'text', 'text': json.dumps(detail)}], 'isError': True,
            }}
        return {'jsonrpc': '2.0', 'id': message['id'], 'error': {
            'code': -32603, 'message': detail['message'], 'data': detail,
        }}

    async def handle(self, message):
        if self.closed:
            raise RuntimeError('cutex MCP bridge is closed')
        if message.get('method') == 'initialize':
            async with self.lock:
                self.initialize = deepcopy(message)
                result = await self.bridge.handle(message)
                if isinstance(result, dict) and 'error' not in result:
                    self.needs_handshake = False
                return result
        try:
            await self._recover()
        except Exception as error:
            log.warning('cutex SDK MCP recovery unavailable (%s)', type(error).__name__)
            return self.failure(message, unknown=False)
        try:
            sent_session = self.bridge._session
            result = await self.bridge.handle(message)
        except Exception:
            if not self.stopped():
                raise
            result = None
        # The server can send a CONNECTION_CLOSED response just before its task
        # finishes. Yield so the SDK can publish that terminal session state.
        closed_reply = (isinstance(result, dict)
                        and result.get('error', {}).get('code') == -32000
                        and result.get('error', {}).get('message') == 'Connection closed')
        if closed_reply and sent_session is not None:
            try:
                async with asyncio.timeout(.25):
                    await sent_session._task.wait()
            except Exception:
                pass
            if self.bridge._session is sent_session:
                self.needs_handshake = True
        if closed_reply or self.stopped() or (sent_session is not None and sent_session.finished):
            try:
                await self._recover()
            except Exception as error:
                log.warning('cutex SDK MCP recovery unavailable (%s)', type(error).__name__)
            return self.failure(message, unknown=True) if result is None or closed_reply else result
        return result

    async def aclose(self):
        self.closed = True
        async with self.lock:
            await self.bridge.aclose()


class RecoveringSdkClient:
    """Delegate public SDK methods, installing recovery on each new connection."""
    def __init__(self, client):
        self.client = client

    def __getattr__(self, name):
        return getattr(self.client, name)

    async def connect(self, *args, **kwargs):
        await self.client.connect(*args, **kwargs)
        query = getattr(self.client, '_query', None)
        if query is None:  # Offline fake clients have no SDK control channel.
            return
        bridges = getattr(query, '_sdk_mcp_bridges', None)
        if bridges is None:
            if 'cutex' in getattr(query, 'sdk_mcp_servers', {}):
                raise RuntimeError('SDK cutex MCP recovery integration is unavailable')
            return
        if 'cutex' in bridges and not isinstance(bridges['cutex'], RecoveringMcpBridge):
            bridges['cutex'] = RecoveringMcpBridge(bridges['cutex'])
