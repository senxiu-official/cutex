"""Small host-owned tool registry. Ordinary Python handlers and fixed HTTP endpoints.

Path checks are guardrails for a trusted local workspace, NOT an OS sandbox.
Exec, when explicitly enabled, has the OS permissions of the server process.
"""
from __future__ import annotations

import asyncio
import base64
import copy
import hashlib
import importlib
import itertools
import json
import os
import re
import signal
import stat
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Awaitable, Callable
from urllib.parse import urlsplit

import httpx
from jsonschema import Draft202012Validator

from .config import Settings
from .store import dump


@dataclass(frozen=True)
class ToolContext:
    thread_id: str
    turn_id: str
    call_id: str
    workspace: Path


@dataclass
class ToolResult:
    text: str
    success: bool = True
    # Optional validated MCP blocks. Text-only tools retain their old contract.
    content: list[dict] | None = None

    @classmethod
    def from_mcp(cls, blocks: list[dict], success: bool = True) -> ToolResult:
        if not isinstance(blocks, list) or not 1 <= len(blocks) <= 8:
            raise ValueError('Invalid MCP content count')
        texts, images = [], 0
        for block in blocks:
            if not isinstance(block, dict):
                raise ValueError('Invalid MCP content')
            if block.get('type') == 'text':
                if set(block) != {'type', 'text'} or not isinstance(block['text'], str):
                    raise ValueError('Invalid MCP text')
                texts.append(block['text'])
            elif block.get('type') == 'image':
                if set(block) != {'type', 'data', 'mimeType'} or block['mimeType'] not in {'image/png', 'image/jpeg'}:
                    raise ValueError('Invalid MCP image')
                data = block['data']
                if not isinstance(data, str) or not data or len(data) > 6 * 1024 * 1024:
                    raise ValueError('MCP image budget')
                decoded = base64.b64decode(data, validate=True)
                if len(decoded) > 4_718_592:
                    raise ValueError('MCP image bytes')
                # The trusted daemon does complete decoding; adapter rechecks framing.
                if not (decoded.startswith(b'\x89PNG\r\n\x1a\n') if block['mimeType'] == 'image/png'
                        else decoded.startswith(b'\xff\xd8') and decoded.endswith(b'\xff\xd9')):
                    raise ValueError('MCP image type mismatch')
                images += 1
            else:
                raise ValueError('Unsupported MCP content')
        text = '\n'.join(texts)
        envelope_bytes = len(dump({'content': blocks, 'isError': not success}).encode('utf-8'))
        if images > 1 or len(text.encode('utf-8')) > 65536 or envelope_bytes > 8 * 1024 * 1024:
            raise ValueError('MCP output budget')
        if images == 0 and envelope_bytes > 65536:
            raise ValueError('MCP text envelope budget')
        return cls(text, success, copy.deepcopy(blocks))

    def mcp_content(self) -> list[dict]:
        return copy.deepcopy(self.content) if self.content is not None else [{'type': 'text', 'text': self.text}]

    def model_content(self) -> str | list[dict]:
        if self.content is None:
            return self.text
        return [({'type': 'image', 'source': {'type': 'base64', 'media_type': block['mimeType'], 'data': block['data']}}
                 if block['type'] == 'image' else copy.deepcopy(block)) for block in self.content]

    def timeline_content(self) -> list[dict]:
        return [({'type': 'inputImage', 'imageUrl': f"data:{block['mimeType']};base64,{block['data']}"}
                 if block['type'] == 'image' else {'type': 'inputText', 'text': block['text']}) for block in self.mcp_content()]


Handler = Callable[[dict, ToolContext], Awaitable[Any]]


@dataclass
class Tool:
    name: str
    description: str
    schema: dict
    handler: Handler
    mcp_server: str | None = None
    read_only: bool = False
    # Seconds; None = Settings.tool_timeout. Cutex tools that may hold on the
    # Agent Bus (approval-wait, delegation_wait) need longer than the default.
    timeout: float | None = None


class ToolRegistry:
    def __init__(self, settings: Settings, *, include_builtins: bool = True):
        self.settings = settings
        self.tools: dict[str, Tool] = {}
        self._clients: list[httpx.AsyncClient] = []
        if include_builtins:
            self._builtins()

    def register(self, name: str, description: str, schema: dict, handler: Handler, *, read_only: bool = False) -> None:
        if not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", name) or name in self.tools:
            raise ValueError(f"invalid or duplicate tool name: {name}")
        Draft202012Validator.check_schema(schema)
        if schema.get("type") != "object":
            raise ValueError("tool input schema must have type=object")
        self.tools[name] = Tool(name, description, schema, handler, read_only=read_only)

    def definitions(self) -> list[dict]:
        return [{"name": t.name, "description": t.description, "input_schema": t.schema} for t in self.tools.values()]

    async def close(self) -> None:
        for client in self._clients:
            await client.aclose()

    async def execute(self, name: str, arguments: dict, context: ToolContext) -> ToolResult:
        tool = self.tools.get(name)
        if tool is None:
            return ToolResult("Unknown or disabled tool", False)
        errors = list(Draft202012Validator(tool.schema).iter_errors(arguments))
        if errors:
            # Validation messages may include caller data; keep result structural.
            return ToolResult("Invalid tool arguments: schema validation failed", False)
        try:
            async with asyncio.timeout(tool.timeout or self.settings.tool_timeout):
                value = await tool.handler(arguments, context)
            result = value if isinstance(value, ToolResult) else ToolResult(value if isinstance(value, str) else dump(value))
        except asyncio.CancelledError:
            raise
        except TimeoutError:
            return ToolResult("Tool timeout: side effects may be uncertain; do not retry automatically.", False)
        except Exception as exc:
            # User data, API tokens, and request headers must not become diagnostic output.
            return ToolResult(f"Tool failed ({type(exc).__name__}). Check server-side configuration or arguments.", False)
        raw = result.text.encode("utf-8")
        if result.content is not None:
            # Never text-truncate image data. Validate even direct host-tool results.
            try:
                return ToolResult.from_mcp(result.content, result.success)
            except (ValueError, TypeError):
                return ToolResult('Tool returned invalid or oversized image content.', False)
        if len(raw) > self.settings.max_tool_bytes:
            result.text = raw[:self.settings.max_tool_bytes].decode("utf-8", "ignore") + "\n[tool output truncated]"
        return result

    def _path(self, raw: str) -> Path:
        if not isinstance(raw, str) or "\x00" in raw:
            raise ValueError("invalid path")
        path = (self.settings.workspace / raw).resolve()
        if not path.is_relative_to(self.settings.workspace):
            raise ValueError("path leaves workspace")
        # Protect runtime state even when the operator puts it under the workspace.
        if path == self.settings.state_dir or path.is_relative_to(self.settings.state_dir):
            raise ValueError("runtime state is not exposed as a model tool")
        return path

    def _builtins(self) -> None:
        def obj(properties: dict, required: list[str]) -> dict:
            return {"type": "object", "properties": properties, "required": required, "additionalProperties": False}
        string = {"type": "string"}

        async def read(args: dict, ctx: ToolContext) -> dict:
            path = self._path(args["path"])
            info = path.stat()
            if not stat.S_ISREG(info.st_mode) or info.st_size > self.settings.max_tool_bytes:
                raise ValueError("read_file requires a bounded regular UTF-8 file")
            data = path.read_bytes()
            if len(data) > self.settings.max_tool_bytes:
                raise ValueError("file grew beyond read limit")
            return {"path": str(path.relative_to(self.settings.workspace)), "text": data.decode("utf-8"), "sha256": hashlib.sha256(data).hexdigest()}

        async def ls(args: dict, ctx: ToolContext) -> dict:
            path = self._path(args.get("path", "."))
            with os.scandir(path) as entries:
                selected = list(itertools.islice(entries, 501))
            visible = selected[:500]
            return {"entries": [{"name": p.name, "isDirectory": p.is_dir(follow_symlinks=False), "isSymlink": p.is_symlink()} for p in sorted(visible, key=lambda p: p.name)], "truncated": len(selected) > 500}

        async def write(args: dict, ctx: ToolContext) -> dict:
            path = self._path(args["path"])
            data = args["content"].encode("utf-8")
            if len(data) > self.settings.max_tool_bytes:
                raise ValueError("write exceeds byte budget")
            mode = 0o644
            if path.exists():
                if not path.is_file() or path.stat().st_size > self.settings.max_tool_bytes:
                    raise ValueError("write target must be a bounded regular file")
                old = path.read_bytes()
                if args.get("expected_sha256") != hashlib.sha256(old).hexdigest():
                    raise ValueError("existing file requires matching expected_sha256 from read_file")
                mode = stat.S_IMODE(path.stat().st_mode)
            elif args.get("expected_sha256"):
                raise ValueError("expected an existing file")
            path.parent.mkdir(parents=True, exist_ok=True)
            fd, tmp = tempfile.mkstemp(prefix=".cute-claude-", dir=path.parent)
            try:
                with os.fdopen(fd, "wb") as f:
                    f.write(data)
                    f.flush()
                    os.fsync(f.fileno())
                os.chmod(tmp, mode)
                os.replace(tmp, path)
            finally:
                if os.path.exists(tmp):
                    os.unlink(tmp)
            return {"path": str(path.relative_to(self.settings.workspace)), "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)}

        async def execute(args: dict, ctx: ToolContext) -> dict:
            argv = args["argv"]
            proc = await asyncio.create_subprocess_exec(*argv, cwd=ctx.workspace,
                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
                    start_new_session=(os.name != "nt"))
            async def bounded_read(stream) -> tuple[str, bool]:
                parts, retained, truncated = [], 0, False
                while chunk := await stream.read(8192):
                    room = max(0, self.settings.max_tool_bytes // 2 - retained)
                    if room:
                        parts.append(chunk[:room])
                    retained += min(room, len(chunk))
                    truncated |= len(chunk) > room
                return b"".join(parts).decode("utf-8", "replace"), truncated
            tasks = [asyncio.create_task(bounded_read(proc.stdout)), asyncio.create_task(bounded_read(proc.stderr))]
            try:
                await proc.wait()
                stdout, stderr = await asyncio.gather(*tasks)
            except BaseException:
                try:
                    if os.name != "nt":
                        os.killpg(proc.pid, signal.SIGKILL)
                    elif proc.returncode is None:
                        proc.kill()
                except ProcessLookupError:
                    pass
                await proc.wait()
                for task in tasks:
                    task.cancel()
                await asyncio.gather(*tasks, return_exceptions=True)
                raise
            return {"exitCode": proc.returncode, "stdout": stdout[0], "stderr": stderr[0], "truncated": stdout[1] or stderr[1]}

        self.register("read_file", "Read a UTF-8 file up to the configured byte limit; returns its SHA-256.", obj({"path": string}, ["path"]), read, read_only=True)
        self.register("list_directory", "List up to 500 entries in a workspace directory.", obj({"path": string}, []), ls, read_only=True)
        if self.settings.allow_write:
            self.register("write_file", "Create a file, or replace it atomically. For existing files, pass expected_sha256 returned by read_file. Not an OS sandbox.", obj({"path": string, "content": string, "expected_sha256": string}, ["path", "content"]), write)
        if self.settings.allow_exec:
            self.register("exec", "Execute an argv vector on the host, without a shell wrapper. Has full OS user permissions; root path is only the starting directory.", obj({"argv": {"type": "array", "minItems": 1, "maxItems": 128, "items": string}}, ["argv"]), execute)

    def load_plugin(self, spec: str) -> None:
        module, separator, function = spec.partition(":")
        if not separator:
            raise ValueError("plugin must be module:function")
        # Only operator-selected command-line config can load executable Python code.
        getattr(importlib.import_module(module), function)(self)

    def load_http_tools(self, path: Path) -> None:
        entries = json.loads(path.read_text())
        if not isinstance(entries, list):
            raise ValueError("HTTP tools config must be an array")
        for entry in entries:
            url = entry["url"]
            parsed = urlsplit(url)
            if parsed.username or parsed.password or parsed.fragment:
                raise ValueError("HTTP tool URL may not contain credentials or fragments")
            if parsed.scheme != "https" and not (parsed.scheme == "http" and parsed.hostname in {"127.0.0.1", "localhost", "::1"}):
                raise ValueError("HTTP tools require HTTPS or loopback HTTP")
            token_name = entry.get("token_env")
            token = os.environ.get(token_name, "") if token_name else ""
            if token_name and not token:
                raise ValueError(f"missing tool credential environment variable: {token_name}")
            client = httpx.AsyncClient(timeout=self.settings.tool_timeout, follow_redirects=False)
            self._clients.append(client)
            name = entry["name"]
            async def handler(args: dict, context: ToolContext, *, _url=url, _token=token, _client=client, _name=name):
                headers = {"Authorization": f"Bearer {_token}"} if _token else {}
                payload = {"name": _name, "arguments": args,
                           "context": {"threadId": context.thread_id, "turnId": context.turn_id, "callId": context.call_id}}
                # Fixed host-selected endpoint; a model cannot change the destination.
                async with _client.stream("POST", _url, json=payload, headers=headers) as response:
                    response.raise_for_status()
                    chunks, size = [], 0
                    async for chunk in response.aiter_bytes():
                        size += len(chunk)
                        if size > self.settings.max_tool_bytes:
                            raise ValueError("HTTP tool output exceeds limit")
                        chunks.append(chunk)
                    return b"".join(chunks).decode("utf-8")
            self.register(name, entry["description"], entry["input_schema"], handler)
