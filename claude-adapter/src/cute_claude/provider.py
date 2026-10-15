"""Direct Anthropic Messages/SSE client. No Claude CLI or Agent SDK dependency."""
from __future__ import annotations

import asyncio
import copy
import json
from dataclasses import dataclass
from typing import Awaitable, Callable, Protocol
from urllib.parse import urlsplit

import httpx

from .config import Settings
from .store import dump

Delta = Callable[[int, str], Awaitable[None]]


class ProviderError(RuntimeError):
    pass


@dataclass
class Completion:
    content: list[dict]
    stop_reason: str
    usage: dict
    request_id: str | None = None


class Provider(Protocol):
    async def sample(self, messages: list[dict], tools: list[dict], model: str, on_text: Delta, system: str | None = None) -> Completion: ...
    async def close(self) -> None: ...


async def sse_events(response: httpx.Response):
    """Support comments, multiline data, CRLF, final unterminated event, and size bounds."""
    data: list[str] = []
    size = total = 0
    async for line in response.aiter_lines():
        total += len(line.encode())
        if total > 16_000_000:
            raise ProviderError("Provider stream exceeded 16 MB budget")
        if not line:
            if data:
                try:
                    event = json.loads("\n".join(data))
                except json.JSONDecodeError as exc:
                    raise ProviderError("Malformed provider SSE JSON") from exc
                if not isinstance(event, dict):
                    raise ProviderError("Provider SSE event is not an object")
                yield event
            data, size = [], 0
        elif line.startswith("data:"):
            value = line[5:].removeprefix(" ")
            data.append(value)
            size += len(value.encode())
            if size > 2_000_000:
                raise ProviderError("Provider SSE event exceeded 2 MB budget")
    if data:
        try:
            event = json.loads("\n".join(data))
        except json.JSONDecodeError as exc:
            raise ProviderError("Malformed final SSE JSON") from exc
        if not isinstance(event, dict):
            raise ProviderError("Provider SSE event is not an object")
        yield event


class AnthropicMessages:
    def __init__(self, settings: Settings, api_key: str, client: httpx.AsyncClient | None = None):
        if not api_key:
            raise ValueError("ANTHROPIC_API_KEY is required (no subscription-token/CLI credential loading)")
        parts = urlsplit(settings.api_base)
        if parts.username or parts.password or parts.query or parts.fragment:
            raise ValueError("api_base must not contain credentials, a query or a fragment")
        if parts.scheme != "https" and not (parts.scheme == "http" and parts.hostname in {"127.0.0.1", "localhost", "::1"}):
            raise ValueError("api_base requires HTTPS (HTTP is allowed only on loopback)")
        base = settings.api_base.rstrip("/")
        self.url = base + ("/messages" if base.endswith("/v1") else "/v1/messages")
        self.settings, self.key = settings, api_key
        self._owns_client = client is None
        self.client = client or httpx.AsyncClient(timeout=settings.api_timeout, follow_redirects=False)

    async def close(self) -> None:
        if self._owns_client:
            await self.client.aclose()

    async def sample(self, messages: list[dict], tools: list[dict], model: str, on_text: Delta, system: str | None = None) -> Completion:
        body = {"model": model, "max_tokens": self.settings.max_tokens, "stream": True,
                "system": system or self.settings.system, "messages": messages}
        if tools:
            body["tools"] = tools
        headers = {"x-api-key": self.key, "anthropic-version": "2023-06-01", "content-type": "application/json"}
        blocks: dict[int, dict] = {}
        fragments: dict[int, list[str]] = {}
        closed: set[int] = set()
        usage: dict = {}
        stop: str | None = None
        started = stopped = False
        async with asyncio.timeout(self.settings.api_timeout):
            async with self.client.stream("POST", self.url, json=body, headers=headers) as response:
                request_id = response.headers.get("request-id")
                if response.status_code != 200:
                    # Do not leak response bodies or credentials into the transcript/logs.
                    raise ProviderError(f"Anthropic HTTP {response.status_code}; request_id={request_id or 'unavailable'}. No automatic retry.")
                async for event in sse_events(response):
                    kind = event.get("type")
                    if kind == "error":
                        raise ProviderError("Anthropic reported an SSE error; request outcome may be uncertain. No automatic retry.")
                    if kind == "message_start":
                        if started:
                            raise ProviderError("Duplicate message_start")
                        started = True
                        usage.update(event.get("message", {}).get("usage", {}))
                    elif kind == "content_block_start":
                        index, block = event.get("index"), copy.deepcopy(event.get("content_block", {}))
                        if not started or not isinstance(index, int) or isinstance(index, bool) or index != len(blocks) or index in blocks or len(blocks) >= 64:
                            raise ProviderError("Invalid content block sequence")
                        if block.get("type") not in {"text", "tool_use", "thinking", "redacted_thinking"}:
                            raise ProviderError("Unsupported provider content block; no lossy history conversion is performed")
                        blocks[index] = block
                        if block["type"] == "text" and block.get("text"):
                            await on_text(index, block["text"])
                    elif kind == "content_block_delta":
                        index, delta = event.get("index"), event.get("delta", {})
                        if index not in blocks or index in closed:
                            raise ProviderError("Delta for missing or closed content block")
                        block, dtype = blocks[index], delta.get("type")
                        if dtype == "text_delta" and block["type"] == "text":
                            chunk = delta.get("text", "")
                            if not isinstance(chunk, str):
                                raise ProviderError("Invalid text delta")
                            block["text"] = block.get("text", "") + chunk
                            await on_text(index, chunk)
                        elif dtype == "input_json_delta" and block["type"] == "tool_use":
                            fragments.setdefault(index, []).append(delta.get("partial_json", ""))
                        elif dtype == "thinking_delta" and block["type"] == "thinking":
                            block["thinking"] = block.get("thinking", "") + delta.get("thinking", "")
                        elif dtype == "signature_delta" and block["type"] == "thinking":
                            block["signature"] = block.get("signature", "") + delta.get("signature", "")
                        else:
                            raise ProviderError("Unsupported content delta; refusing a corrupted replay")
                    elif kind == "content_block_stop":
                        index = event.get("index")
                        if index not in blocks or index in closed:
                            raise ProviderError("Invalid content_block_stop")
                        if index in fragments:
                            try:
                                value = json.loads("".join(fragments[index]) or "{}")
                            except (json.JSONDecodeError, TypeError) as exc:
                                raise ProviderError("Truncated/invalid tool arguments") from exc
                            if not isinstance(value, dict):
                                raise ProviderError("Tool arguments must be an object")
                            blocks[index]["input"] = value
                        closed.add(index)
                    elif kind == "message_delta":
                        stop = event.get("delta", {}).get("stop_reason") or stop
                        usage.update(event.get("usage", {}))  # Cumulative; never sum delta snapshots.
                    elif kind == "message_stop":
                        stopped = True
                        break
                    # Unknown top-level events and pings are harmless; block semantics aren't.
        if not started or not stopped or not stop or set(blocks) != closed:
            raise ProviderError("Provider stream ended before complete message_stop; no automatic retry")
        content = [blocks[k] for k in sorted(blocks)]
        calls = [b for b in content if b["type"] == "tool_use"]
        ids = [b.get("id") for b in calls]
        if any(not isinstance(x, str) or not x for x in ids) or len(set(ids)) != len(ids):
            raise ProviderError("Invalid/duplicate tool use IDs")
        for call in calls:
            if not isinstance(call.get("input"), dict) or not isinstance(call.get("name"), str):
                raise ProviderError("Incomplete tool_use block")
        return Completion(content, stop, usage, request_id)


class MockProvider:
    """Deterministic offline provider; model output is never represented as live Claude."""
    def __init__(self, delay: float = 0.001):
        self.delay = delay
        self.requests: list[dict] = []

    async def close(self) -> None:
        pass

    async def sample(self, messages: list[dict], tools: list[dict], model: str, on_text: Delta, system: str | None = None) -> Completion:
        self.requests.append(copy.deepcopy({"messages": messages, "tools": tools, "model": model}))
        content = messages[-1]["content"]
        if any(b.get("type") == "tool_result" for b in content):
            answer = "[MOCK] Tool result received: " + " ".join(b.get("content", "") for b in content)[:2000]
            calls = []
        else:
            prompt = " ".join(b.get("text", "") for b in content)
            if prompt.startswith("sleep:"):
                await asyncio.sleep(float(prompt.split(":", 1)[1]))
            if prompt.startswith("fail:"):
                raise ProviderError("[MOCK] deliberate provider failure")
            calls = []
            if prompt.startswith("tool:"):
                name, raw = prompt[5:].split(" ", 1)
                calls = [{"type": "tool_use", "id": f"mock_tool_{len(self.requests)}", "name": name, "input": json.loads(raw)}]
            elif prompt.startswith("tools:"):
                calls = [{"type": "tool_use", "id": f"mock_tool_{len(self.requests)}_{i}", **c} for i, c in enumerate(json.loads(prompt[6:]))]
            answer = "[MOCK] Calling tool." if calls else "[MOCK] " + prompt[:2000]
        for start in range(0, len(answer), 24):
            await asyncio.sleep(self.delay)
            await on_text(0, answer[start:start+24])
        return Completion([{"type": "text", "text": answer}] + calls,
                          "tool_use" if calls else "end_turn", {"input_tokens": 10, "output_tokens": 8,
                          "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0})
