"""Cutex's existing stdio MCP façade as host tools. No Claude-specific authority."""
import asyncio
import json
import os
from pathlib import Path

from .tools import ToolContext, ToolResult
from .browser_content import BROWSER_TOOLS


def load(registry, descriptor: Path) -> None:
    config = json.loads(descriptor.read_text())
    executable = str(Path(config['executable']).resolve(strict=True))
    binding_path = descriptor.parent / 'cutex-mcp-binding.json'
    for tool in config['tools']:
        async def invoke(arguments: dict, context: ToolContext, name=tool['name']):
            binding = json.loads(binding_path.read_text())
            if binding['threadId'] != context.thread_id:
                raise ValueError('Cutex MCP thread binding mismatch')
            env = dict(os.environ)
            env.update({
                'CUTEX_AGENT_ID': binding['runtimeAgentId'],
                'CUTEX_RUNTIME_GENERATION': str(binding['generation']),
                'CUTEX_MCP_THREAD_ID': binding['threadId'],
                'CUTEX_AGENT_BUS_URL': binding['url'],
                'CUTEX_AGENT_BUS_TOKEN': binding['token'],
            })
            proc = await asyncio.create_subprocess_exec(executable, env=env,
                stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL)
            try:
                params = {'name': name, 'arguments': arguments}
                if name in BROWSER_TOOLS:
                    params['_meta'] = {'threadId': context.thread_id,
                                       'callId': getattr(context, 'call_id', None)}
                # Non-browser tools keep their pre-browser envelope. In particular,
                # adding callId here would change derive_action_id's RPC fallback.
                request = {'jsonrpc':'2.0','id':1,'method':'tools/call', 'params':params}
                proc.stdin.write((json.dumps(request) + '\n').encode())
                await proc.stdin.drain()
                proc.stdin.close()
                # The façade bounds frames. Bound consumption as well, before JSON parsing.
                output = bytearray()
                while chunk := await proc.stdout.read(8192):
                    output.extend(chunk)
                    if len(output) > (8 * 1024 * 1024 if name in BROWSER_TOOLS else 262144):
                        raise ValueError('MCP response exceeds transport budget')
                await proc.wait()
                if proc.returncode:
                    return ToolResult('Cutex MCP unavailable; the action may be uncertain. Query its stable action/message ID before retrying.', False)
                response = json.loads(output)
                if response.get('id') != 1:
                    return ToolResult('Cutex MCP response ID mismatch; outcome unknown. Inspect the action by its stable ID.', False)
                if 'error' in response:
                    error = response['error']
                    return ToolResult(f"Cutex MCP error {error.get('code', 'unknown')}: {str(error.get('message', 'Request failed'))[:2048]}", False)
                result = response['result']
                if name in BROWSER_TOOLS:
                    return ToolResult.from_mcp(result.get('content'), not result.get('isError', False))
                text = '\n'.join(block['text'] for block in result.get('content', []) if block.get('type') == 'text')
                return ToolResult(text, not result.get('isError', False))
            finally:
                if proc.returncode is None:
                    proc.kill()
                await proc.wait()
        registry.register(tool['name'], tool['description'], tool['inputSchema'], invoke,
                          read_only=tool.get('annotations', {}).get('readOnlyHint') is True)
        registry.tools[tool['name']].mcp_server = 'cutex'
