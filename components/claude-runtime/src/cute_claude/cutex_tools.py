"""Cutex's existing stdio MCP façade as host tools. No Claude-specific authority."""
import asyncio
import json
import os
import hashlib
import uuid
from pathlib import Path

from .tools import ToolContext, ToolResult
from .browser_content import BROWSER_TOOLS


# Tools whose owner-approval gate may hold the call on the Agent Bus
# (cutex approval-wait): a managed / persistent / outside-root service and an
# agent launch on an approval-gated profile.
HOLDING_TOOLS = frozenset({'service_register', 'service_update', 'agent_create', 'member_replace'})
# delegation_wait blocks up to 120 s on the Agent Bus (cutex asks §5.8).
DELEGATION_WAIT = 'delegation_wait'
DELEGATION_WAIT_SECS = 120
# Slack for the façade, the Bus and the action itself after a hold.
HOLD_SLACK_SECS = 30

RECEIPT_TOOLS = frozenset({'agent_create', 'agent_delegate'})


def receipt_arguments(name: str, arguments: dict, context) -> dict:
    """Allocate before launch/timeout; native call identity survives facade restarts.

    Never include the payload in the key: a changed payload on the same call
    must hit the daemon's idempotency conflict instead of creating new work.
    """
    if name not in RECEIPT_TOOLS or arguments.get('action_id') is not None:
        return arguments
    call = getattr(context, 'call_id', None) or uuid.uuid4().hex
    identity = [context.thread_id, getattr(context, 'turn_id', None), call, name]
    key = hashlib.sha256(json.dumps(identity, separators=(',', ':')).encode()).hexdigest()
    return {**arguments, 'action_id': 'call-' + key[:48]}


def uncertain_result(name: str, arguments: dict, code: str) -> ToolResult:
    return ToolResult(json.dumps({'status': 'response_uncertain', 'code': code,
        'action_id': arguments['action_id'], 'tool': name, 'outcomeUnknown': True,
        'detail': 'Query this action_id before retrying; no replacement action was submitted.'}), False)


def approval_wait_secs(config: dict) -> int | None:
    """The hold cutex opted this adapter in to (descriptor `approvalWaitSecs`, cutex
    sets it for adapters declaring features.approvalWait); None = answer at once."""
    value = config.get('approvalWaitSecs')
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        return None
    return min(value, 600)


def tool_timeout(name: str, wait: int | None, default: float | None) -> float | None:
    """A per-tool timeout for cutex tools that may block on the Bus; None keeps the default."""
    floor = 0
    if name in RECEIPT_TOOLS:
        # Rust: preflight 10s + create 120s / asks 150s; leave 20s
        # for spawning, serialization and reading the structured receipt.
        floor = (150 + (wait or 0)) if name == 'agent_create' else 180
    if wait and name in HOLDING_TOOLS:
        floor = max(floor, wait + HOLD_SLACK_SECS)
    if name == DELEGATION_WAIT:
        floor = DELEGATION_WAIT_SECS + HOLD_SLACK_SECS
    if not floor:
        return None
    return max(float(floor), float(default or 0))


def load(registry, descriptor: Path) -> None:
    config = json.loads(descriptor.read_text())
    executable = str(Path(config['executable']).resolve(strict=True))
    binding_path = descriptor.parent / 'cutex-mcp-binding.json'
    wait = approval_wait_secs(config)
    default_timeout = getattr(getattr(registry, 'settings', None), 'tool_timeout', None)
    for tool in config['tools']:
        async def invoke(arguments: dict, context: ToolContext, name=tool['name']):
            arguments = receipt_arguments(name, arguments, context)
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
            env.pop('CUTEX_MCP_APPROVAL_WAIT_SECS', None)
            env.pop('CUTEX_MCP_TURN_ID', None)
            if wait:
                # approval-wait: the façade may hold a gated call for the owner and
                # names this turn, so cutex stops holding when the turn ends.
                env['CUTEX_MCP_APPROVAL_WAIT_SECS'] = str(wait)
                turn = getattr(context, 'turn_id', None)
                if isinstance(turn, str) and turn:
                    env['CUTEX_MCP_TURN_ID'] = turn
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
                    if name in RECEIPT_TOOLS:
                        return uncertain_result(name, arguments, 'mcp_process_failed')
                    return ToolResult('Cutex MCP unavailable; the action may be uncertain. Query its stable action/message ID before retrying.', False)
                response = json.loads(output)
                if response.get('id') != 1:
                    if name in RECEIPT_TOOLS:
                        return uncertain_result(name, arguments, 'mcp_response_mismatch')
                    return ToolResult('Cutex MCP response ID mismatch; outcome unknown. Inspect the action by its stable ID.', False)
                if 'error' in response:
                    if name in RECEIPT_TOOLS:
                        return uncertain_result(name, arguments, 'mcp_rpc_error')
                    error = response['error']
                    return ToolResult(f"Cutex MCP error {error.get('code', 'unknown')}: {str(error.get('message', 'Request failed'))[:2048]}", False)
                result = response['result']
                if name in BROWSER_TOOLS:
                    return ToolResult.from_mcp(result.get('content'), not result.get('isError', False))
                text = '\n'.join(block['text'] for block in result.get('content', []) if block.get('type') == 'text')
                return ToolResult(text, not result.get('isError', False))
            except (ValueError, KeyError, OSError):
                if name in RECEIPT_TOOLS:
                    return uncertain_result(name, arguments, 'mcp_response_unavailable')
                raise
            finally:
                if proc.returncode is None:
                    proc.kill()
                await proc.wait()
        registry.register(tool['name'], tool['description'], tool['inputSchema'], invoke,
                          read_only=tool.get('annotations', {}).get('readOnlyHint') is True)
        registry.tools[tool['name']].mcp_server = 'cutex'
        registry.tools[tool['name']].timeout = tool_timeout(tool['name'], wait, default_timeout)
