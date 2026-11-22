"""The explicit Cutex capabilities of a native child, independent of parent role."""
READ_ONLY_TOOLS = frozenset({
    'agent_list', 'agent_name_history', 'artifact_list', 'artifact_read',
    'service_list', 'delegation_read', 'owner_pins',
})
DENIED = ('Native subagents only complete their assigned subtask. This Cutex tool is not in '
          'the native subagent read-only tool set. Return the required action to your parent.')

def child_context(parent: str) -> str:
    return (f'You are a native subagent working for Cutex agent {parent}. '
            'Complete only your assigned subtask and return the result to your native parent. '
            'You have no independent Cutex session, persistent-agent identity, Director role or '
            'member-management authority. Do not create or manage Cutex agents, send messages, '
            'publish artifacts, create alarms or workflows, or change services. '
            'Use only the explicitly available read-only Cutex tools within your parent scope. '
            'Leave persistent memory updates to your parent.')

def readonly_tool(engine, name: str) -> bool:
    # The fixed allowlist is authority; annotations alone never expand it.
    return name in READ_ONLY_TOOLS and name in engine.tools.tools

def agent_definitions(sdk, engine):
    """Own the generic worker definition; do not replace specialized user roles.

    The hook enforces scope for every agent type, including roles that this SDK
    cannot filter without replacing their private/native system prompt.
    """
    parent = getattr(engine.tools, 'parent_agent_name', None) or engine.settings.external_owner_id or 'the parent agent'
    denied = [f'mcp__cutex__{name}' for name in engine.tools.tools if name not in READ_ONLY_TOOLS]
    return {'general-purpose':sdk.AgentDefinition(
        description='General-purpose native worker for researching and completing the assigned subtask.',
        prompt=child_context(parent), disallowedTools=denied)}

