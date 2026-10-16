from __future__ import annotations

import argparse
import asyncio
import contextlib
import os
import signal
import sys
from pathlib import Path

from . import __version__
from .config import Settings
from .engine import Engine
from .protocol import Server
from .provider import AnthropicMessages, MockProvider
from .store import Store
from .tools import ToolRegistry
from .transport import rpc_token, stdio, websocket


def parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description="Claude app-server subset: optional subscription SDK backend or independent Messages API backend")
    p.add_argument("--version", action="version", version=__version__)
    p.add_argument("--backend", choices=["messages", "subscription"], default="messages",
                   help="subscription explicitly opts into the official SDK/closed-source Claude runtime dependency")
    p.add_argument("--claude-cli", type=Path, help="one installed unmodified official Claude binary; otherwise use PATH, never SDK-bundled fallback")
    p.add_argument("--claude-config-dir", type=Path, help="shared official login/native history directory, defaults to CLAUDE_CONFIG_DIR or ~/.cutex-claude-compatible/.claude")
    p.add_argument("--api-account-config", type=Path,
                   help="subscription backend: cutex API account (JSON naming the key file, key env, optional base URL); "
                        "the key goes only to the official runtime process, no claude.ai login is used")
    p.add_argument("--mock", action="store_true", help="offline deterministic provider, never calls Anthropic")
    p.add_argument("--model", default=os.getenv("ANTHROPIC_MODEL"), help="explicit model ID or official SDK model alias available to your account")
    p.add_argument("--effort", choices=["low", "medium", "high", "xhigh", "max"], help="Claude SDK effort; omit to use runtime default")
    p.add_argument("--thinking", choices=["adaptive", "disabled"], default="adaptive",
                   help="Claude thinking (subscription): adaptive (default; budgeted on models without adaptive thinking) or disabled")
    p.add_argument("--thinking-display", choices=["summarized", "omitted"], default="summarized",
                   help="thinking text (subscription): summarized (default; shown as reasoning items) or omitted; "
                        "config.toml model_reasoning_summary overrides it")
    p.add_argument("--file-checkpointing", choices=["enabled", "disabled"], default="disabled",
                   help="subscription: opt into native file snapshots for rewind (default disabled); "
                        "disabling stops new snapshots and preserves existing native history")
    p.add_argument("--workspace", type=Path, required=True)
    p.add_argument("--managed-skills-root", type=Path, action="append", default=[],
                   help="host-selected skill root for this workspace/account; repeatable, no user settings discovery")
    p.add_argument("--external-owner-id", help="trusted cutex owner binding for thread/externalInput methods")
    p.add_argument("--external-runtime-generation", type=int, help="trusted cutex runtime generation; requires --external-owner-id")
    p.add_argument("--state-dir", type=Path, default=Path.home() / ".local/state/cute-claude-demo")
    p.add_argument("--listen", default="stdio://")
    p.add_argument("--ready-file", type=Path, help="write bound endpoint and runtime identity after listening; contains no credentials")
    p.add_argument("--rpc-token-env", default="CUTE_CLAUDE_RPC_TOKEN")
    p.add_argument("--api-base", default="https://api.anthropic.com")
    p.add_argument("--max-tokens", type=int, default=None, help="Messages-only output token limit (default 4096)")
    p.add_argument("--max-steps", type=int, default=12)
    p.add_argument("--api-timeout", type=float, default=120)
    p.add_argument("--tool-timeout", type=float, default=30)
    p.add_argument("--allow-write", action="store_true", help="enable workspace write_file tool")
    p.add_argument("--allow-exec", action="store_true", help="DANGER: enable command execution with this OS user's full privileges")
    p.add_argument("--system-file", type=Path, help="additional trusted host instructions")
    p.add_argument("--system-append-file", type=Path,
                   help="subscription: trusted host text appended to the Claude Code preset system prompt "
                        "instead of the built-in host note (cutex writes # Cutex, AGENTS.md, file references, memory)")
    p.add_argument("--http-tools", type=Path, help="fixed HTTP-JSON tool definitions; subscription bridges these through SDK in-process MCP")
    p.add_argument("--cutex-mcp-config", type=Path, help="Cutex launcher-owned MCP tool descriptor")
    p.add_argument("--plugin", action="append", default=[], help="operator-selected module:function registering Python tools")
    p.add_argument("--agent-turn-questions", choices=["decide", "wait"], default="decide",
                   help="AskUserQuestion in a turn not started by the owner (Agent Bus / service input): "
                        "decide = answer at once that no human is available (default); wait = block for a client answer")
    return p


async def run(args) -> None:
    workspace = args.workspace.expanduser().resolve(strict=True)
    # Reject an explicit symlink before resolving the private state directory.
    state = args.state_dir.expanduser().absolute()
    if state.is_symlink():
        raise ValueError("state-dir must not be a symlink")
    state = state.resolve()
    model = args.model or ("mock-claude" if args.mock else "")
    if not model:
        raise ValueError("real mode requires --model or ANTHROPIC_MODEL; no invented model default")
    system = Settings.__dataclass_fields__["system"].default
    extra = args.system_file.read_text(encoding="utf-8") if args.system_file else ""
    if extra:
        system += "\n" + extra
    system_append = None
    if args.system_append_file:
        if args.backend != "subscription":
            raise ValueError("--system-append-file requires --backend subscription")
        system_append = args.system_append_file.read_text(encoding="utf-8")
        if extra:
            system_append = system_append.rstrip() + "\n\n" + extra
    settings = Settings(workspace, state, model=model, mock=args.mock, backend=args.backend,
            api_base=args.api_base, max_tokens=args.max_tokens if args.max_tokens is not None else 4096, max_steps=args.max_steps,
            effort=args.effort, thinking=args.thinking, thinking_display=args.thinking_display, allow_write=args.allow_write, allow_exec=args.allow_exec,
            file_checkpointing=args.file_checkpointing == "enabled",
            api_timeout=args.api_timeout, tool_timeout=args.tool_timeout, system=system, system_append=system_append,
            agent_turn_questions=args.agent_turn_questions,
            managed_skills_roots=tuple(root.expanduser().resolve() for root in args.managed_skills_root),
            external_owner_id=args.external_owner_id, external_runtime_generation=args.external_runtime_generation)
    settings.validate()
    driver = None
    if args.backend == "subscription":
        from .subscription_auth import check_subscription_environment, load_api_account, official_cli, resolve_config_dir
        from .subscription import OfficialSdkDriver
        # The adapter's own environment never carries API routing; an API
        # account's settings reach only the official runtime (SDK options env).
        check_subscription_environment()
        api_account = load_api_account(args.api_account_config) if args.api_account_config else None
        if args.max_tokens is not None:
            raise ValueError("--max-tokens is Messages-only; subscription generation is controlled by the official runtime")
        if args.api_base != "https://api.anthropic.com":
            raise ValueError("--api-base is not applicable to subscription mode")
        cli = official_cli(args.claude_cli)
        config_dir = resolve_config_dir(args.claude_config_dir)
        # Authentication is checked in the owner at connect, not at every turn.
        driver = OfficialSdkDriver(cli, config_dir, api_account=api_account)
    elif args.claude_cli or args.claude_config_dir or args.api_account_config:
        raise ValueError("--claude-cli, --claude-config-dir and --api-account-config require --backend subscription")
    elif not args.mock and not os.getenv("ANTHROPIC_API_KEY"):
        raise ValueError("Messages backend requires ANTHROPIC_API_KEY. With only a subscription, explicitly use --backend subscription (official runtime dependency); no automatic fallback.")
    store = Store(state)
    engine = None
    try:
        store.bind_backend("mock" if args.mock else args.backend)
        tools = ToolRegistry(settings, include_builtins=settings.backend != "subscription")
        if args.backend == "subscription":
            from .subscription import SubscriptionEngine
            engine = SubscriptionEngine(settings, store, driver, tools)
        else:
            provider = MockProvider() if args.mock else AnthropicMessages(settings, os.environ["ANTHROPIC_API_KEY"])
            engine = Engine(settings, store, provider, tools)
        if args.http_tools:
            tools.load_http_tools(args.http_tools)
        if args.cutex_mcp_config:
            from .cutex_tools import load
            load(tools, args.cutex_mcp_config)
        for spec in args.plugin:
            tools.load_plugin(spec)
        server = Server(settings, store, engine)
        backend_label = 'MOCK' if args.mock else (
            ('official SDK / API account' if args.api_account_config else 'official SDK / subscription login')
            if args.backend == 'subscription' else 'Anthropic Messages')
        print(f"cute-claude-app-server {__version__}; provider={backend_label}; no OS sandbox", file=sys.stderr)
        if args.backend == 'subscription':
            print("NOTE: depends on the official Claude runtime; account/organization billing and seat access still apply. No API fallback.", file=sys.stderr)
        if args.allow_exec:
            print("WARNING: exec is explicitly enabled with full host-user permissions.", file=sys.stderr)
        if args.listen == "stdio://":
            await stdio(server)
        else:
            stop = asyncio.Event()
            loop = asyncio.get_running_loop()
            for signum in (signal.SIGINT, signal.SIGTERM):
                with contextlib.suppress(NotImplementedError):
                    loop.add_signal_handler(signum, stop.set)
            await websocket(server, args.listen, rpc_token(state, args.rpc_token_env), stop, ready_file=args.ready_file)
    finally:
        if engine:
            await engine.close()
        store.close()


def main() -> None:
    args = parser().parse_args()
    try:
        asyncio.run(run(args))
    except KeyboardInterrupt:
        pass
    except Exception as exc:
        # Configuration diagnostics only. Runtime prompt/tool errors travel over RPC.
        print(f"startup error: {exc}", file=sys.stderr)
        raise SystemExit(2) from None


if __name__ == "__main__":
    main()
