"""Use --plugin examples.python_tools:register from the repository root.

A production CUTEX tool can be registered here without MCP or model-specific RPC.
"""
from cute_claude.tools import ToolContext


def register(registry):
    async def add(arguments: dict, context: ToolContext):
        return {'value': arguments['a'] + arguments['b'], 'threadId': context.thread_id}
    registry.register('add', 'Add two numbers with an ordinary Python function.', {
        'type': 'object', 'properties': {'a': {'type': 'number'}, 'b': {'type': 'number'}},
        'required': ['a', 'b'], 'additionalProperties': False}, add)
