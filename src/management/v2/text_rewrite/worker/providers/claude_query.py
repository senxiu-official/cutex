import asyncio
import json
import sys
import time

from claude_agent_sdk import AssistantMessage, ClaudeAgentOptions, ResultMessage, SystemMessage, query

MODEL = "claude-haiku-5-5"
ERRORS = {
    "authentication_failed": "authentication",
    "billing_error": "quota_exhausted",
    "rate_limit": "rate_limited",
    "content_filter": "content_refused",
}


async def run():
    payload = json.load(sys.stdin)

    async def prompt():
        yield {"type": "user", "message": {"role": "user", "content": payload["draft"]}}

    options = ClaudeAgentOptions(
        cli_path=sys.argv[1], model=MODEL, effort="low", system_prompt=payload["system"],
        tools=[], allowed_tools=[], strict_mcp_config=True, mcp_servers={},
        setting_sources=[], plugins=[], max_turns=1, permission_mode="dontAsk",
        extra_args={"no-session-persistence": None, "no-chrome": None, "disable-slash-commands": None},
    )
    start = time.monotonic()
    startup_ms = None
    actual_model = None
    result = {"Err": "upstream"}
    async for message in query(prompt=prompt(), options=options):
        if isinstance(message, SystemMessage) and message.subtype == "init":
            if message.data.get("tools"):
                return {"Err": "invalid_output"}
            if startup_ms is None:
                startup_ms = int((time.monotonic() - start) * 1000)
        elif isinstance(message, AssistantMessage):
            if message.error:
                return {"Err": ERRORS.get(message.error, "upstream")}
            actual_model = message.model or actual_model
            if any(type(block).__name__ not in ("TextBlock", "ThinkingBlock") for block in message.content):
                return {"Err": "invalid_output"}
        elif isinstance(message, ResultMessage):
            if message.is_error or not isinstance(message.result, str):
                return {"Err": "upstream"}
            result = {"Ok": {"text": message.result, "model": actual_model,
                             "requested_model": MODEL, "provider": "anthropic", "startup_ms": startup_ms}}
    return result


try:
    result = asyncio.run(run())
except Exception:
    result = {"Err": "upstream"}
json.dump(result, sys.stdout)
