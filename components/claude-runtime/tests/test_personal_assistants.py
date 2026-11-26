from dataclasses import replace

import pytest

from test_persistent import ph
from test_questions_plan import begin


async def test_personal_assistant_receives_only_its_coordination_surface(ph):
    ph.engine.settings = replace(ph.settings, personal_assistant=True)
    _, _, bridge = await begin(ph)
    options = ph.sdk.clients[0].options
    assert options.tools == []
    assert options.agents == {}
    assert options.strict_mcp_config
    assert options.setting_sources == []
    assert set(options.mcp_servers) == {"cutex"}
    for name in ["Read", "Bash", "Write", "Agent", "mcp__cutex__agent_delegate",
                 "mcp__cutex__director_request_respond", "mcp__other__pa_self",
                 "mcp__cutex__pa_unknown"]:
        decision = await bridge.pre_tool(
            {"session_id": bridge.sid, "tool_name": name, "tool_input": {}}, "test-call", {})
        assert decision["hookSpecificOutput"]["permissionDecision"] == "deny", name


def test_personal_assistant_configuration_refuses_other_tool_sources(ph):
    for expanded in ({"allow_exec": True}, {"allow_write": True},
                     {"setting_sources": ("user",)}, {"claude_ai_connectors": True}):
        settings = replace(ph.settings, personal_assistant=True, **expanded)
        with pytest.raises(ValueError, match="scoped cutex tool surface"):
            settings.validate()
