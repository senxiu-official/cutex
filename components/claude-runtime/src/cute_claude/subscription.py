"""Optional official Agent SDK backend. THIS BACKEND DEPENDS ON CLAUDE CODE.

The open adapter owns RPC, FIFO inbox and host tool execution. The unmodified
selected official runtime owns inference/authentication/native context. Local history
is an audit projection, never replayed as a native Claude transcript.

Public SDK API target: claude-agent-sdk 0.2.166. No credentials are extracted.
"""
from __future__ import annotations

import asyncio
from copy import deepcopy
import dataclasses
import importlib
import json
import os
import re
import time
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

from .engine import Engine
from .errors import RpcError
from .provider import ProviderError
from .store import COMPACT_EVENT, Store, dump, identifier
from .tools import ToolContext, ToolRegistry, ToolResult
from .browser_content import BROWSER_TOOLS, journal_result
from .subscription_auth import ApiAccount, resolve_config_dir
from .sdk_runtime import RuntimeOwner, TurnInterrupted, OutcomeUncertain, system_prompt_option
from .inputs import sdk_user_message
from .side_chats import BOUNDARY_BLOCKS as SIDE_BOUNDARY_BLOCKS
from .model_config import ModelConfig
from .live_settings import Compaction
from .model_observations import ModelObservations
from .mcp_recovery import RecoveringSdkClient, cancelled_host_tool_result

from .subagent_transcripts import SubagentTranscripts
from .native_tools import (COMMAND_TOOLS, EDIT_TOOLS, OWNER_DECLINED, OWNER_STOPPED, UNANSWERED_PERMISSION, NativeTools,
                           PlanState, bypass_safety_denial, command_spec, exclusions, external_mcp_allowed,
                           external_mcp_tool, interrupted_item, preview)
from .safety_grants import BLOCKED_EVENT, UPDATED_EVENT
from . import permission_rules
from .approvals import is_allowed

# Largest single message read from Claude Code (SDK `max_buffer_size`; its default is 1 MiB).
MAX_MESSAGE_BYTES = 64 * 1024 * 1024

SDK_TARGET = '0.2.166'
SERVER_NAME = 'cutex'
CALL_FIELD = '_cutex_invocation'
# Tools a subagent may use in plan mode (besides MCP tools).
PLAN_SUBAGENT_TOOLS = frozenset({'Read', 'Grep', 'Glob', 'LS', 'WebSearch', 'WebFetch', 'AskUserQuestion',
                                 'Agent', 'Task', 'TodoWrite', 'TaskCreate', 'TaskUpdate', 'TaskList', 'TaskGet'})
# A main-agent tool call outside any cutex turn: Claude Code's own turn for a
# background task's notification. The message keeps the old prefix (logs, tests).
NO_TURN_TOOLS = ('No active turn accepts tools. This is Claude Code\'s own turn for a background task '
                 'notification: the user does not see it and it cannot use tools. Reply in one line and end '
                 'the turn; cutex sends you the result as a visible message, act on it there.')
# Background subagents (claude-r23): admitted by agent id after their turn ends.
MAX_BACKGROUND = 64
BACKGROUND_NO_HOST_TOOLS = ('cutex tools are not available to a background subagent. Finish without them and say '
                            'in your final reply what the main agent should do with them.')
BACKGROUND_NO_AGENTS = 'A background subagent cannot start subagents. Do the work yourself.'
BACKGROUND_NO_QUESTIONS = ('A background subagent cannot ask the user. Make a reasonable choice or stop, and say '
                           'in your final reply what needs the user.')
BACKGROUND_NEEDS_APPROVAL = ('This tool call needs approval, and a background subagent cannot ask for it. Do not '
                             'retry it or work around it: finish without it and say in your final reply which step '
                             'needs approval. The main agent can do it itself or use a foreground subagent '
                             '(run_in_background: false).')


def field(obj: Any, key: str, default: Any = None) -> Any:
    return obj.get(key, default) if isinstance(obj, dict) else getattr(obj, key, default)


def message_type(obj: Any) -> str:
    return obj.get('type', '') if isinstance(obj, dict) else type(obj).__name__


# Claude Code's own placeholder reply (model "<synthetic>"), written to the
# native transcript after "[Request interrupted by user]" when a session is
# resumed whose last message is the user's (r243 acceptance, claude-r17).
# Not something Claude said: never an agent message.
SYNTHETIC_PLACEHOLDERS = frozenset({'No response requested.'})


def synthetic_placeholder(message: Any) -> bool:
    if field(message, 'model') != '<synthetic>':
        return False
    blocks = field(message, 'content', None) or []
    if not isinstance(blocks, list) or not blocks:
        return False
    return all(message_type(block) in {'TextBlock', 'text'}
               and str(field(block, 'text', '')).strip() in SYNTHETIC_PLACEHOLDERS for block in blocks)


def api_error_subject(account: ApiAccount) -> str:
    # Launch configs currently carry an account id, not its display label.
    # Never expose URL userinfo, path/query, provider text, or the account key.
    account_id = re.sub(r'[^a-zA-Z0-9_-]', '_', account.account_id)[:80]
    try:
        host = urlsplit(account.base_url or 'https://api.anthropic.com').hostname or 'configured endpoint'
    except ValueError:
        host = 'configured endpoint'
    subject = f"Claude API account '{account_id}' ({host})"
    return account.redact(subject)


# Official CLI text (headless and interactive variants) when the OAuth refresh lock is held.
REFRESH_LOCK_TEXT = re.compile(r'another Claude Code process is refreshing', re.I)

# Waits before re-sending an input whose turn failed on the OAuth refresh lock
# before Claude produced anything (the lock is released, or goes stale after
# about 60 s). `CUTE_CLAUDE_REFRESH_RETRY_DELAYS` overrides them: comma-separated
# seconds, empty = never retry.
REFRESH_RETRY_DELAYS = (20.0, 45.0)


def permission_mode(engine: Any, tid: str) -> str | None:
    """The thread's Claude permission mode (not plan) for the prompt, if known."""
    permissions = getattr(engine, 'permissions', None)
    mode = getattr(permissions, 'mode', None)
    try:
        return mode(tid) if callable(mode) else None
    except Exception:
        return None


def refresh_retry_delays() -> tuple[float, ...]:
    raw = os.environ.get('CUTE_CLAUDE_REFRESH_RETRY_DELAYS')
    if raw is None:
        return REFRESH_RETRY_DELAYS
    try:
        delays = tuple(float(part) for part in raw.split(',') if part.strip())
    except ValueError:
        return REFRESH_RETRY_DELAYS
    return tuple(d for d in delays if 0 <= d <= 600)[:5]


HISTORY_SIGNATURE_REJECTION = ('The account rejected encrypted reasoning or a thinking signature in conversation history. '
    'The session history is preserved; nothing was stripped or deleted. Use an account that accepts this history, '
    'or start a new conversation.')


def history_signature_rejected(texts) -> bool:
    # Native error diagnostics only, bounded and never echoed: provider text may contain secrets.
    pattern = r'(?:thinking|redacted_thinking|reasoning).{0,100}signature|signature.{0,100}(?:thinking|reasoning)|encrypted.{0,80}(?:reasoning|content|decrypt)'
    return any(isinstance(text, str) and re.search(pattern, text[:8192], re.I | re.S) for text in texts)


class SdkResponseError(ProviderError):
    """A recognized SDK failure, with only adapter-authored diagnostic text."""
    transient_refresh = False

    def __init__(self, message: Any, account: ApiAccount | None):
        known = {'authentication_failed', 'billing_error', 'rate_limit', 'server_error', 'invalid_request'}
        code = field(message, 'error')
        code = code if isinstance(code, str) and code in known else 'unknown'
        self.codex_error_info = {
            'authentication_failed': 'unauthorized', 'billing_error': 'usageLimitExceeded',
            'rate_limit': 'rateLimitExceeded' if account else 'usageLimitExceeded',
            'server_error': 'internalServerError', 'invalid_request': 'badRequest',
        }.get(code, 'other')
        texts = [field(block, 'text', '') for block in field(message, 'content', [])]
        if code == 'server_error' and any(isinstance(text, str) and re.search(r'\b529\b|overloaded', text, re.I) for text in texts):
            self.codex_error_info = 'serverOverloaded'
        if code == 'invalid_request' and history_signature_rejected(texts):
            text = HISTORY_SIGNATURE_REJECTION
        elif account:
            hint = ' Check the API key and base URL.' if code == 'authentication_failed' else ''
            text = f'{api_error_subject(account)} rejected the request ({code}).{hint}'
        elif any(isinstance(text, str) and REFRESH_LOCK_TEXT.search(text) for text in texts):
            # The official CLI found the login's refresh lock held: another Claude Code process
            # on this config dir (or one that exited mid-refresh) was refreshing an expired
            # token. Not a login problem; transient (the lock is released or goes stale).
            # Seen on the first turn of a new chat on an account idle for hours (2026-10-11).
            self.codex_error_info = 'other'
            self.transient_refresh = True
            text = ('Official Claude could not refresh its login token: another Claude Code process using this '
                    'account was refreshing it (or exited mid-refresh). This is usually transient: send the '
                    'message again in a minute. The failed input is not automatically retried.')
        elif any(isinstance(text, str) and text.startswith('Failed to refresh OAuth token:') for text in texts):
            self.codex_error_info = 'unauthorized'
            text = 'Official Claude OAuth refresh failed. Check the official CLI login/refresh state.'
        else:
            # The prefix is what cutex's quota detection matches (quota_continue::quota_error_text).
            text = (f'Official Claude returned an error ({code}). You may submit a fresh instruction. This runtime '
                    'does not retry it itself; cutex may requeue delegations once the limit resets.')
        super().__init__(text)


class OfficialSdkDriver:
    persistent = True

    def __init__(self, cli: Path, config_dir: Path | None = None, *,
                 connect_timeout: float = 90, control_timeout: float = 10,
                 interrupt_timeout: float = 15, api_account: ApiAccount | None = None):
        self.cli = cli
        self.config_dir = resolve_config_dir(config_dir)
        # API account (cutex claude_accounts): its key / base URL / settings go
        # only into the official runtime's env; no claude.ai login is checked.
        self.api_account = api_account
        self.connect_timeout = connect_timeout
        self.control_timeout = control_timeout
        self.interrupt_timeout = interrupt_timeout
        self.owners: dict[str, RuntimeOwner] = {}
        self.closing = False
        try:
            self.sdk = importlib.import_module('claude_agent_sdk')
        except ImportError:
            raise ValueError('Install the optional [subscription] extra; select an installed official CLI with --claude-cli.') from None
        expected = {'tools', 'strict_mcp_config', 'session_id', 'resume', 'setting_sources',
                    'include_partial_messages', 'stderr', 'env', 'effort', 'thinking', 'settings'}
        fields = {f.name for f in dataclasses.fields(self.sdk.ClaudeAgentOptions)}
        if not expected <= fields:
            raise ValueError(f'Official SDK lacks required options; use the documented API target {SDK_TARGET}.')

    def _owner(self, engine, tid: str, *, explicit: bool = False) -> RuntimeOwner:
        if self.closing:
            raise ProviderError('Adapter is closing; no new SDK connection was started.')
        owner = self.owners.get(tid)
        if owner and owner.task.done():
            if owner.lease is not None:
                raise ProviderError('Prior child exit is unverified; inspect runtime status before another writer is started.')
            if not explicit:
                raise ProviderError('SDK connection ended. Call claude/runtime/start explicitly; no automatic reconnect/replay.')
            owner = None
        if owner is None:
            owner = RuntimeOwner(self, engine, tid, engine.session(tid))
            self.owners[tid] = owner
        return owner

    async def start(self, engine, tid: str) -> dict:
        owner = self._owner(engine, tid, explicit=True)
        await asyncio.shield(owner.ready)
        return owner.snapshot()

    def status(self, tid: str) -> dict:
        owner = self.owners.get(tid)
        return owner.snapshot() if owner else {
            'threadId': tid, 'state': 'offline', 'processId': None,
            'processExitConfirmed': None, 'activeTurnId': None,
            'ownerTaskAlive': False, 'automaticReconnect': True}

    async def close_runtime(self, tid: str) -> dict:
        owner = self.owners.get(tid)
        return await owner.close() if owner else self.status(tid)

    async def close(self) -> None:
        self.closing = True
        await asyncio.gather(*(owner.close() for owner in self.owners.values()))

    async def interrupt(self, tid: str, turn_id: str) -> bool:
        owner = self.owners.get(tid)
        if not owner:
            return False
        await owner.interrupt(turn_id)
        return True

    async def control(self, engine, tid: str, kind: str, **arguments):
        owner = self.owners.get(tid)
        if owner is None:
            raise ProviderError('Start the Claude runtime before using native controls')
        return await owner.control(kind, **arguments)

    async def run(self, *, engine: 'SubscriptionEngine', tid: str, turn_id: str,
                  session_id: str, resume: bool, prompt: str, on_message, message: dict | None = None) -> None:
        owner = self._owner(engine, tid)
        if owner.session_id != session_id:
            raise ProviderError('Native session binding changed while the runtime was alive.')
        await owner.submit(turn_id, message if message is not None else prompt, on_message)

    def session_rules(self, name: str, args: dict):
        """`acceptForSession`: allow this tool (edits: all edit tools; Bash: this
        exact command) for the rest of the native session only. Never written
        to a settings file."""
        update = getattr(self.sdk, 'PermissionUpdate', None)
        rule = getattr(self.sdk, 'PermissionRuleValue', None)
        if update is None or rule is None:
            return None
        if name in EDIT_TOOLS:
            rules = [rule(tool_name=tool) for tool in sorted(EDIT_TOOLS)]
        elif name == 'Bash' and isinstance(args.get('command'), str):
            rules = [rule(tool_name='Bash', rule_content=args['command'])]
        else:
            rules = [rule(tool_name=name)]
        return [update(type='addRules', rules=rules, behavior='allow', destination='session')]

    def project_rules(self, rules: list[str]):
        """"Always allow in this project": the chosen rules take effect in the
        live native session at once (destination session); cutex persists them
        per workspace and passes them as flag settings on every start."""
        update = getattr(self.sdk, 'PermissionUpdate', None)
        rule = getattr(self.sdk, 'PermissionRuleValue', None)
        if update is None or rule is None:
            return None
        values = []
        for text in rules:
            name, paren, rest = text.partition('(')
            values.append(rule(tool_name=name, rule_content=rest[:-1] if paren else None))
        return [update(type='addRules', rules=values, behavior='allow', destination='session')]

    async def make_client(self, engine, tid: str, session_id: str, resume: bool):
        # No pre-start `claude auth status` (subscription or API accounts). For a
        # subscription login that check is a second official CLI process on the
        # same config dir: when the access token has expired it races the SDK
        # process for Claude Code's OAuth refresh lock, and the loser fails the
        # first turn ("another Claude Code process is refreshing it"). The SDK
        # process refreshes the token itself; a missing or broken login fails the
        # first request with an authentication error (mapped by SdkResponseError).
        # The adapter never refreshes OAuth tokens. API accounts have no login:
        # the first request checks the key. Login status stays available through
        # `subscription_auth status` and cutex's account status check.
        bridge = SdkToolBridge(engine, tid, None, session_id)
        sdk_tools = []
        for tool in engine.tools.tools.values():
            # The envelope avoids changing the caller's original JSON Schema.
            # A PreToolUse hook adds a one-use correlation handle; the model's
            # supplied value is ignored. The host sees ordinary original args.
            schema = {'type': 'object', 'properties': {
                'arguments': deepcopy(tool.schema),
                CALL_FIELD: {'type': 'string', 'description': 'Runtime-managed field; omit this field.'}},
                'required': ['arguments'], 'additionalProperties': False}
            async def handler(args: dict, name=tool.name):
                return await bridge.execute(name, args)
            sdk_tools.append(self.sdk.SdkMcpTool(name=tool.name, description=tool.description,
                input_schema=schema, handler=handler))
        server = self.sdk.create_sdk_mcp_server(name=SERVER_NAME, version='0.3.0', tools=sdk_tools)
        async def background_permission(name, args, context, call_id, agent_id):
            """A background subagent's permission request: no turn can show an
            approval, so it is declined; under full access a safety block is
            recorded on the origin turn and may be granted once, as in a turn."""
            if name == 'AskUserQuestion':
                return self.sdk.PermissionResultDeny(message=BACKGROUND_NO_QUESTIONS)
            if name.startswith(f'mcp__{SERVER_NAME}__'):
                return self.sdk.PermissionResultDeny(message=BACKGROUND_NO_HOST_TOOLS)
            if engine.permissions.effective_mode(tid) != 'bypassPermissions':
                return self.sdk.PermissionResultDeny(message=BACKGROUND_NEEDS_APPROVAL)
            safety = getattr(engine, 'safety_grants', None)
            used = safety.consume(tid, name, args, call_id) if safety is not None and call_id else None
            if used is not None:
                await engine.emit(tid, UPDATED_EVENT, {'threadId': tid, 'block': used})
                return self.sdk.PermissionResultAllow(updated_input=args)
            reason = getattr(context, 'title', None) or getattr(context, 'decision_reason', None)
            reason = reason if isinstance(reason, str) and reason.strip() else None
            message = bypass_safety_denial(reason)
            if safety is not None and call_id and isinstance(args, dict):
                origin = bridge.background.get(agent_id, (None, None))[0]
                command = (command_spec(name, args, engine.settings.workspace)[0] if name in COMMAND_TOOLS
                           else f"{name} {json.dumps(preview(args, 2048), ensure_ascii=False)}")
                block = safety.record(tid, origin, call_id, name, args, command,
                                      reason or 'Claude Code asked for approval of this tool call.')
                await engine.emit(tid, BLOCKED_EVENT, {'threadId': tid, 'turnId': origin, 'itemId': call_id, 'block': block})
            return self.sdk.PermissionResultDeny(message=message)
        async def permission_request(name, args, context):
            call_id = field(context, 'tool_use_id')
            background = bridge.background_agent(call_id, field(context, 'agent_id'))
            if background is not None:
                return await background_permission(name, args, context, call_id, background)
            # In-process MCP also has the one-use PreToolUse grant. Older SDK
            # callbacks can omit context.tool_use_id, without weakening this
            # correlation to the authenticated native call.
            if not call_id and name.startswith('mcp__') and isinstance(args, dict):
                grant = bridge.grants.get(args.get(CALL_FIELD, ''))
                call_id = grant[1]['id'] if grant else None
            if not bridge.permission_context_valid(call_id, field(context, 'agent_id')):
                return self.sdk.PermissionResultDeny(message='No matching live native tool context.')
            if name == 'AskUserQuestion':
                call_id = getattr(context, 'tool_use_id', None)
                native = bridge.native_questions.pop(call_id, None)
                if not native or not bridge.accepting or native != (bridge.binding, args):
                    return self.sdk.PermissionResultDeny(message='No matching active question.')
                # Turns started by another agent or a service do not block on a human.
                notice = await engine.permissions.decline_unattended(tid, bridge.binding[0], args, call_id)
                if notice is not None:
                    return self.sdk.PermissionResultDeny(message=notice)
                task = asyncio.create_task(engine.permissions.ask(tid, bridge.binding[0], args, call_id))
                bridge.host_tasks.add(task)
                try:
                    answers = await task
                finally:
                    bridge.host_tasks.discard(task)
                if answers is not None:
                    return self.sdk.PermissionResultAllow(updated_input={**args, 'answers': answers})
                return self.sdk.PermissionResultDeny(message='No user answer received. Do not invent an answer or repeat the question automatically.')
            # Claude Code's own tools and host-selected external MCP servers /
            # connectors: the frontend decides, like any native permission request.
            if not name.startswith('mcp__') or external_mcp_tool(engine.settings, name):
                decision = await bridge.native.approve(name, args, context)
                if decision['allow']:
                    rules = (self.project_rules(decision['rules']) if decision.get('rules')
                             else self.session_rules(name, args) if decision['session'] else None)
                    return self.sdk.PermissionResultAllow(updated_input=args, updated_permissions=rules)
                return self.sdk.PermissionResultDeny(message=decision.get('message') or UNANSWERED_PERMISSION,
                                                     interrupt=decision['interrupt'])
            grant = bridge.grants.get(args.get(CALL_FIELD, '')) if isinstance(args,dict) else None
            if not grant or not bridge.accepting or grant[0] != bridge.binding:
                return self.sdk.PermissionResultDeny(message='No active adapter invocation.')
            binding, call = grant
            if name != f'mcp__{SERVER_NAME}__{call["name"]}' or args.get('arguments') != call['input']:
                return self.sdk.PermissionResultDeny(message='Invocation parameters differ.')
            task = asyncio.create_task(engine.approve_tool(tid, binding[0], call['name'], call['input'], call['id']))
            bridge.host_tasks.add(task)
            try:
                approved = await task
            finally:
                bridge.host_tasks.discard(task)
            if is_allowed(approved):
                # "This session" keeps the tool allowed in the live native session
                # (never cutex's own exec: each command is reviewed).
                rules = (self.session_rules(name, args) if approved == 'acceptForSession'
                         and call['name'] not in {'exec', 'Bash'} else None)
                return self.sdk.PermissionResultAllow(updated_input=args, updated_permissions=rules)
            return self.sdk.PermissionResultDeny(
                message=(OWNER_STOPPED if approved == 'cancel' else OWNER_DECLINED if approved == 'decline'
                         else UNANSWERED_PERMISSION), interrupt=approved == 'cancel')
        async def post_tool(data, tool_use_id, context):
            binding = bridge.binding
            call_id = tool_use_id or data.get('tool_use_id')
            # A claim needs the PreToolUse-admitted main-agent tool id:
            # callback_valid alone accepts a callback without any call id.
            authenticated = (data.get('hook_event_name') == 'PostToolUse'
                             and isinstance(call_id, str) and call_id != ''
                             and bridge.callback_valid(data, call_id) and call_id not in bridge.context_hooks)
            if authenticated:
                bridge.context_hooks.add(call_id)
            result = await bridge.native.post(data, tool_use_id, context)
            # Notes other agents left while this turn runs: read after the
            # main agent's tool call (never into a subagent's context).
            if data.get('agent_id') or data.get('parent_tool_use_id') or not bridge.binding or not bridge.accepting:
                return result
            if data.get('session_id') != bridge.sid or bridge.binding != binding:
                return result
            # Only a main-agent tool id admitted by PreToolUse may take wakes.
            notes = await engine.inject_notes(tid, binding[0]) if engine.store.pending_notes(tid) else ''
            if bridge.binding != binding or not bridge.accepting:
                # inject_notes already marked these notes injected and emitted
                # their items: return them as before (base behaviour), only
                # never claim busy inputs after the turn ended.
                if not notes:
                    return result
                return {'hookSpecificOutput': {'hookEventName': 'PostToolUse', 'additionalContext': notes}}
            # This claim and the final hook return have no yielding operation
            # between them. A turn end before this point leaves the FIFO intact.
            busy = engine.busy_delivery.inject(tid, binding[0]) if authenticated else ''
            notes = '\n\n'.join(part for part in (busy, notes) if part)
            if not notes:
                return result
            if busy:
                engine.busy_delivery.offered(tid, binding[0])
            return {'hookSpecificOutput': {'hookEventName': 'PostToolUse', 'additionalContext': notes}}
        thread = engine.store.thread(tid)
        connectors = engine.settings.claude_ai_connectors and self.api_account is None
        system_prompt, bridge.prompt_digest = system_prompt_option(
            engine.store, tid, engine.settings.preset_append(thread['instructions'], permission_mode(engine, tid)), resume)
        options = self.sdk.ClaudeAgentOptions(
            cli_path=str(self.cli), cwd=str(engine.settings.workspace), model=thread['model'],
            effort=engine.thread_effort(tid), thinking=ModelConfig(engine.settings).thinking(),
            tools={'type':'preset','preset':'claude_code'}, disallowed_tools=exclusions(engine.settings), allowed_tools=[f'mcp__{SERVER_NAME}__{tool.name}' for tool in engine.tools.tools.values() if tool.read_only],
            permission_mode=engine.permissions.effective_mode(tid),
            extra_args={'allow-dangerously-skip-permissions': None, 'replay-user-messages': None},
            # One NDJSON message can carry a whole tool result: a screenshot or a large file read is easily
            # over the SDK's 1 MiB default, which ended the turn with CLIJSONDecodeError (2026-10-19).
            max_buffer_size=MAX_MESSAGE_BYTES,
            # Host-selected external servers (cutex's MCP layers) next to the in-process cutex server.
            # --strict-mcp-config keeps every other MCP source out; claude.ai connectors (subscription
            # logins only) are fetched by Claude Code only without it.
            mcp_servers={**engine.settings.mcp_servers, SERVER_NAME: server}, strict_mcp_config=not connectors,
            # The SDK warns that can_use_tool is never invoked under bypassPermissions; Claude Code still
            # calls it for AskUserQuestion and for its safety checks (NativeTools.approve declines those).
            # Settings sources are the profile's opt-in (project / user = the account home); plugins stay off.
            setting_sources=list(engine.settings.setting_sources), plugins=[], can_use_tool=permission_request,
            hooks={'PreToolUse': [self.sdk.HookMatcher(hooks=[bridge.pre_tool])],
                   'PostToolUse': [self.sdk.HookMatcher(hooks=[post_tool])],
                   'PostToolUseFailure': [self.sdk.HookMatcher(hooks=[bridge.native.post])],
                   'SubagentStart': [self.sdk.HookMatcher(hooks=[bridge.subagent_start])],
                   'SubagentStop': [self.sdk.HookMatcher(hooks=[bridge.subagent_stop])]},
            # Claude Code's own prompt (environment, date, tool guidance) plus
            # the host's text; CLAUDE.md / settings only with setting_sources.
            # `snapshot` off only when the recorded prompt is stale (system_prompt_option).
            system_prompt=system_prompt,
            include_partial_messages=True, max_turns=None,
            enable_file_checkpointing=engine.settings.file_checkpointing,
            # Subagent text and thinking reach subagent_transcripts (never the parent's reply).
            forward_subagent_text=True,
            session_id=None if resume else session_id, resume=session_id if resume else None,
            continue_conversation=False,
            # API accounts: the key comes from the apiKeyHelper flag setting, never
            # the env, which Bash / hooks / MCP stdio servers inherit.
            settings=engine.flag_settings(tid) if hasattr(engine, 'flag_settings') else (
                json.dumps(self.api_account.cli_settings()) if self.api_account else None),
            env={**(self.api_account.environment() if self.api_account else {}),
                 'CLAUDE_CONFIG_DIR': str(self.config_dir), 'CLAUDE_CODE_MAX_RETRIES': '3' if self.api_account else '0',
                 'API_TIMEOUT_MS': str(int(engine.settings.api_timeout * 1000)),
                 # Cross-session messaging off (ListAgents / SendMessage / SendFile and the
                 # inbound peer socket); otherwise a server flag that defaults to on decides.
                 'CLAUDE_CODE_HARBOR_KITE': '0',
                 # Memory is cutex's (project memory in the append): Claude Code's own,
                 # account-local auto memory stays off for every Claude agent.
                 'CLAUDE_CODE_DISABLE_AUTO_MEMORY': '1',
                 # This model's auto-compaction when the profile sets it apart from the launch values.
                 **(engine.compaction.child_env(thread['model']) if hasattr(engine, 'compaction') else {})},
            stderr=lambda line: None,  # Do not inherit raw CLI diagnostics on public stderr.

        )
        return RecoveringSdkClient(self.sdk.ClaudeSDKClient(options=options)), bridge


class SdkToolBridge:
    """SDK's in-process MCP is private plumbing, not a separately deployed server."""
    def __init__(self, engine: 'SubscriptionEngine', tid: str, turn_id: str | None, sid: str):
        self.engine, self.tid, self.sid = engine, tid, sid
        self.binding: tuple[str, str] | None = None
        self.accepting = False
        # Digest of this process's system prompt `append` (system_prompt_option).
        self.prompt_digest: str | None = None
        self.native = NativeTools(self)
        self.native_questions: dict[str, tuple] = {}
        self.grants: dict[str, tuple[tuple[str, str], dict]] = {}
        self.host_tasks: set[asyncio.Task] = set()
        self.children: dict[str, tuple] = {}
        self.call_agents: dict[str, str | None] = {}
        # Background subagents (claude-r23): spawning Agent call id -> origin binding
        # until the task-start frame names the child, then child agent id -> origin
        # binding, and the child's tool-call ids. Unlike `children`, revoke() keeps
        # them: the child runs on after its turn ends, until its task ends.
        self.background_spawns: dict[str, tuple[str, str]] = {}
        self.background: dict[str, tuple[str, str]] = {}
        self.background_calls: dict[str, str] = {}
        self.context_hooks: set[str] = set()
        if turn_id is not None:
            self.begin_turn(turn_id)

    def begin_turn(self, turn_id: str) -> None:
        if self.binding or self.host_tasks:
            raise OutcomeUncertain('Previous tool context is still live; refusing to change turn ownership.')
        self.binding = (turn_id, identifier())
        self.native.begin_turn()
        self.accepting = True

    def revoke(self) -> None:
        self.accepting = False
        if self.binding:
            self.engine.store.clear_browser_replay(self.tid, self.binding[0])
        self.grants.clear()
        self.native_questions.clear()
        self.native.calls.clear()
        self.native.seen.clear()
        self.children.clear()
        self.call_agents.clear()
        self.context_hooks.clear()

    async def cancel_tools(self) -> None:
        tasks = list(self.host_tasks)
        for task in tasks: task.cancel()
        if tasks:
            _, pending = await asyncio.wait(tasks, timeout=5)
            if pending:
                raise OutcomeUncertain('A host plugin did not stop after cancellation; do not start another writer.')
            await asyncio.gather(*tasks, return_exceptions=True)
        self.host_tasks.difference_update(tasks)

    async def end_turn(self) -> None:
        if self.binding is not None and getattr(self.engine, 'subagents', None) is not None:
            await self.engine.subagents.turn_ended(self.tid, self.binding[0])
        self.revoke()
        if self.host_tasks:
            await self.cancel_tools()
            raise OutcomeUncertain('Native result arrived while a host tool was still live; inspect effects.')
        self.binding = None

    @staticmethod
    def denied(reason: str) -> dict:
        return {'hookSpecificOutput': {'hookEventName': 'PreToolUse',
                'permissionDecision': 'deny', 'permissionDecisionReason': reason}}

    def callback_valid(self, data: dict, call_id=None) -> bool:
        if not self.accepting or not self.binding or data.get('session_id') != self.sid:
            return False
        agent = data.get('agent_id')
        if agent and self.children.get(agent) != self.binding:
            return False
        # Official child hooks carry agent_id. A lone parent tool id is not
        # enough to authenticate an unregistered child callback.
        if data.get('parent_tool_use_id') and not agent:
            return False
        return call_id is None or (call_id in self.call_agents and self.call_agents[call_id] == agent)

    def adopt_background(self, agent_id: Any, call_id: Any, backgrounded: bool = True) -> tuple[str, str] | None:
        """The task-start frame ties a child agent to a background spawn of ours."""
        if not isinstance(agent_id, str) or not agent_id or len(agent_id) > 256:
            return None
        if agent_id in self.background:
            return self.background[agent_id]
        origin = self.background_spawns.pop(call_id, None) if isinstance(call_id, str) else None
        if origin is None or not backgrounded or len(self.background) >= MAX_BACKGROUND:
            return None
        self.background[agent_id] = origin
        self.children.pop(agent_id, None)
        return origin

    def background_agent(self, call_id: Any, agent_id: Any) -> str | None:
        """The background child a permission request belongs to, if any."""
        if isinstance(agent_id, str) and agent_id in self.background:
            owner = self.background_calls.get(call_id) if isinstance(call_id, str) else None
            return agent_id if owner in (None, agent_id) else None
        owner = self.background_calls.get(call_id) if isinstance(call_id, str) else None
        return owner if owner in self.background and not agent_id else None

    async def background_pre_tool(self, data: dict, tool_use_id: str | None) -> dict:
        """A background child's tool call: admitted by its agent id, with or without a turn."""
        agent_id = data['agent_id']
        if data.get('session_id') != self.sid:
            return self.denied('Native session or child context identity mismatch')
        name = data.get('tool_name', '')
        call_id = tool_use_id or data.get('tool_use_id')
        if not isinstance(call_id, str) or not call_id:
            return self.denied('Missing native tool-call ID')
        if self.background_calls.get(call_id, agent_id) != agent_id or call_id in self.call_agents:
            return self.denied('Native tool-call identity belongs to another agent')
        if name in {'Agent', 'Task'}:
            return self.denied(BACKGROUND_NO_AGENTS)
        if name == 'AskUserQuestion':
            return self.denied(BACKGROUND_NO_QUESTIONS)
        if name.startswith(f'mcp__{SERVER_NAME}__'):
            return self.denied(BACKGROUND_NO_HOST_TOOLS)
        if self.engine.permissions.planning(self.tid) and name not in PLAN_SUBAGENT_TOOLS and not name.startswith('mcp__'):
            return self.denied('Plan mode allows read-only subagent tools')
        external = external_mcp_tool(self.engine.settings, name)
        if name in exclusions(self.engine.settings) or (name.startswith('mcp__') and not external):
            return self.denied('Tool is not enabled by this runtime configuration')
        if external and not external_mcp_allowed(self.engine.settings, *external):
            return self.denied('This MCP tool is turned off for this agent')
        while len(self.background_calls) >= 4096:
            self.background_calls.pop(next(iter(self.background_calls)))
        self.background_calls[call_id] = agent_id
        if getattr(self.engine, 'subagents', None) is not None:
            await self.engine.subagents.child_tool(agent_id, call_id)
        return {}

    def permission_context_valid(self, call_id, agent_id) -> bool:
        if not self.accepting or call_id not in self.call_agents:
            return False
        owner = self.call_agents[call_id]
        # Older SDK permission contexts omit agent_id; correlation still uses
        # the unforgeable native tool-use id admitted by our PreToolUse hook.
        return (not agent_id or agent_id == owner) and (not owner or self.children.get(owner) == self.binding)

    async def subagent_start(self, data, tool_use_id, context):
        agent_id = data.get('agent_id')
        if (data.get('session_id') != self.sid or not self.accepting or not self.binding
                or not isinstance(agent_id, str) or not agent_id or len(agent_id) > 256 or agent_id in self.background):
            return {}
        self.children[agent_id] = self.binding
        owner = getattr(self.engine.driver, 'owners', {}).get(self.tid)
        if owner:
            await owner.tasks.child_started(agent_id, self.binding[0])
        return {}

    async def subagent_stop(self, data, tool_use_id, context):
        agent_id = data.get('agent_id')
        if isinstance(agent_id, str) and agent_id in self.background and data.get('session_id') == self.sid:
            # Kept until its task ends: a late tool hook of the child stays admitted.
            owner = getattr(self.engine.driver, 'owners', {}).get(self.tid)
            if owner:
                await owner.tasks.child_stopped(agent_id)
            return {}
        if not self.callback_valid(data) or agent_id not in self.children:
            return {}
        self.children.pop(agent_id, None)
        owner = getattr(self.engine.driver, 'owners', {}).get(self.tid)
        if owner:
            await owner.tasks.child_stopped(agent_id)
        return {}

    async def pre_tool(self, data: dict, tool_use_id: str | None, context: Any) -> dict:
        if data.get('agent_id') and data.get('agent_id') in self.background:
            return await self.background_pre_tool(data, tool_use_id)
        binding = self.binding
        if binding is None or not self.accepting:
            # Claude Code's own notification turn (or a call after the turn ended):
            # no cutex turn owns it (docs/BUSY_DELIVERY.md, "Claude Code's notification turn").
            return self.denied(NO_TURN_TOOLS)
        if not self.callback_valid(data):
            return self.denied('Native session or child context identity mismatch')
        full_name = data.get('tool_name', '')
        call_id = tool_use_id or data.get('tool_use_id')
        if not isinstance(call_id, str) or not call_id:
            return self.denied('Missing native tool-call ID')
        agent_id = data.get('agent_id')
        if call_id in self.call_agents and self.call_agents[call_id] != agent_id:
            return self.denied('Native tool-call identity belongs to another agent')
        self.call_agents[call_id] = agent_id
        if agent_id and getattr(self.engine, 'subagents', None) is not None:
            await self.engine.subagents.child_tool(agent_id, call_id)
        background = False
        if full_name in {'Agent', 'Task'}:
            arguments = data.get('tool_input', {})
            if not isinstance(arguments, dict):
                return self.denied('Invalid subagent arguments')
            # Newer Claude Code runs a subagent in the background unless the call says otherwise, so
            # only an explicit `run_in_background: false` is a foreground subagent (claude-r23:
            # background ones run on after the turn; a subagent's own subagents stay foreground).
            background = arguments.get('run_in_background') is not False
            if background and agent_id:
                return self.denied('A subagent can only start foreground subagents: pass run_in_background: false.')
            if arguments.get('mode') not in (None, self.engine.permissions.effective_mode(self.tid)):
                return self.denied('Subagents must inherit the parent permission mode')
        if agent_id and self.engine.permissions.planning(self.tid) and full_name not in PLAN_SUBAGENT_TOOLS \
                and not full_name.startswith('mcp__'):
            return self.denied('Plan mode allows read-only subagent tools')
        if full_name == 'AskUserQuestion':
            args = data.get('tool_input', {})
            call_id = tool_use_id or data.get('tool_use_id')
            if not isinstance(call_id, str) or not call_id or not isinstance(args, dict):
                return self.denied('Invalid native question')
            self.native_questions[call_id] = (binding, deepcopy(args))
            return {}  # Native requires-user-interaction dispatches can_use_tool even in bypass.

        if not full_name.startswith('mcp__') or external_mcp_tool(self.engine.settings, full_name):
            # Native tools and host-selected external MCP tools: observed, never run here.
            decision = await self.native.start(full_name, data.get('tool_input', {}), tool_use_id or data.get('tool_use_id'))
            if full_name in {'Agent', 'Task'} and not agent_id and decision == {}:
                if getattr(self.engine, 'subagents', None) is not None:
                    await self.engine.subagents.start(self.tid, self.sid, binding[0], call_id, data.get('tool_input', {}),
                                                      background=background)
                if background:
                    while len(self.background_spawns) >= MAX_BACKGROUND:
                        self.background_spawns.pop(next(iter(self.background_spawns)))
                    self.background_spawns[call_id] = binding
            return decision

        prefix = f'mcp__{SERVER_NAME}__'
        name = full_name[len(prefix):] if full_name.startswith(prefix) else ''
        if name not in self.engine.tools.tools:
            return self.denied('Tool is not registered by this adapter')
        if self.engine.permissions.planning(self.tid) and not self.engine.tools.tools[name].read_only:
            return self.denied('Plan mode allows read-only tools. Switch to Default mode before executing changes.')
        args = data.get('tool_input', {})
        call_id = tool_use_id or data.get('tool_use_id')
        if not isinstance(call_id, str) or not call_id or not isinstance(args, dict) or not isinstance(args.get('arguments'), dict):
            return self.denied('Missing native tool-call ID or arguments')
        original = args['arguments']
        call = {'id': call_id, 'name': name, 'input': original}
        old = self.engine.store.db.execute('SELECT * FROM tool_calls WHERE thread_id=? AND call_id=?', (self.tid, call_id)).fetchone()
        if old and (old['turn_id'] != binding[0] or old['arguments'] != dump(call) or old['state'] != 'completed'):
            return self.denied('Prior invocation differs or has uncertain effects; no automatic replay')
        token = identifier()
        if len(self.grants) >= 256:
            return self.denied('Too many pending tool callbacks; inspect the runtime')
        self.grants[token] = (binding, deepcopy(call))
        return {'hookSpecificOutput': {'hookEventName': 'PreToolUse',
            'updatedInput': {'arguments': original, CALL_FIELD: token}}}

    async def execute(self, name: str, args: dict) -> dict:
        grant = self.grants.pop(args.get(CALL_FIELD, ''), None)
        if not grant or not self.accepting or grant[0] != self.binding:
            return {'content': [{'type': 'text', 'text': 'Missing or expired invocation correlation; tool was not executed.'}], 'is_error': True}
        binding, call = grant
        if call['name'] != name or call['input'] != args.get('arguments'):
            return {'content': [{'type': 'text', 'text': 'Invocation parameters differ; tool was not executed.'}], 'is_error': True}
        # Freeze the turn ID before any await. Never read mutable current_turn
        # after executing host code; late callbacks cannot write a later turn.
        task = asyncio.create_task(self._execute_bound(binding[0], name, call))
        self.host_tasks.add(task)
        try:
            return await task
        except asyncio.CancelledError:
            # Cancelling the host child (stop/revoke/turn teardown) does not
            # cancel this MCP request task. Letting that child's cancellation
            # escape kills mcp 2.2's entire server task group, including sibling
            # tools, and SDK 0.2.166 then stays stopped until a new initialize.
            # Preserve cancellation aimed at this request/connection itself.
            if asyncio.current_task().cancelling():
                raise
            return cancelled_host_tool_result()
        finally:
            self.host_tasks.discard(task)

    async def _execute_bound(self, turn_id: str, name: str, call: dict) -> dict:
        call_id = call['id']
        store = self.engine.store
        # Recheck atomically immediately before effects, including concurrent
        # identical tool invocations and a second callback for the same ID.
        with store.transaction():
            old = store.db.execute('SELECT * FROM tool_calls WHERE thread_id=? AND call_id=?', (self.tid, call_id)).fetchone()
            if old:
                if old['turn_id'] == turn_id and old['state'] == 'completed' and old['arguments'] == dump(call):
                    result = json.loads(old['result'])
                    if result.get('browserImagesOmitted'):
                        return self._replay_browser_image(turn_id, name, result)
                    return {'content': result.get('mcpContent', [{'type': 'text', 'text': result['content']}]), 'is_error': result['is_error']}
                return {'content': [{'type': 'text', 'text': 'Invocation is in progress or uncertain; not re-executed.'}], 'is_error': True}
            store.db.execute("INSERT INTO tool_calls VALUES(?,?,?,'started',?,NULL)",
                (self.tid, call_id, turn_id, dump(call)))
        item = {'type': 'dynamicToolCall', 'id': identifier(), 'namespace': 'claude',
                'tool': name, 'arguments': call['input'], 'status': 'inProgress',
                'contentItems': None, 'success': None, 'durationMs': None}
        mcp_server = self.engine.tools.tools[name].mcp_server
        if mcp_server:
            item = {'type':'mcpToolCall','id':item['id'],'server':mcp_server,'tool':name,
                    'arguments':call['input'],'status':'inProgress','result':None,'error':None,
                    'appContext':None,'pluginId':None,'readOnlyHint':None,'durationMs':None}
        self.engine.open_tools.setdefault(turn_id, {})[item['id']] = item
        store.put_item(turn_id, item)
        await self.engine.emit(self.tid, 'item/started', {'threadId': self.tid, 'turnId': turn_id, 'item': item})
        started = time.monotonic()
        result = await self.engine.tools.execute(name, call['input'],
            ToolContext(self.tid, turn_id, call_id, self.engine.settings.workspace))
        stored = journal_result(name, {'content': result.text, 'mcpContent': result.mcp_content(), 'is_error': not result.success})
        if stored.get('browserImagesOmitted') and self.accepting and self.binding and self.binding[0] == turn_id:
            stored['browserReplayItemId'] = item['id']
        item.update(status='completed' if result.success else 'failed', success=result.success,
            contentItems=result.timeline_content(), durationMs=int((time.monotonic()-started)*1000))
        if mcp_server:
            item.pop('success', None)
            item.pop('contentItems', None)
            item['result'] = {'content':result.mcp_content(), 'structuredContent':None, '_meta':None}
            item['error'] = None if result.success else {'message':'Cutex tool returned an error'}
        with store.transaction():
            store.put_item(turn_id, item)
            store.db.execute("UPDATE tool_calls SET state='completed',result=? WHERE thread_id=? AND call_id=?", (dump(stored), self.tid, call_id))
        self.engine.open_tools[turn_id].pop(item['id'], None)
        await self.engine.emit(self.tid, 'item/completed', {'threadId': self.tid, 'turnId': turn_id, 'item': item})
        return {'content': result.mcp_content(), 'is_error': not result.success}

    def _replay_browser_image(self, turn_id: str, name: str, result: dict) -> dict:
        """Resolve only this active call's canonical item; never execute again.

        No persistent third image copy or long-lived in-memory image cache.
        Missing/expired/corrupt references fail explicitly instead of silently
        turning a successful screenshot into an image-free success.
        """
        unavailable = {'content': [{'type': 'text', 'text': 'Browser image replay is unavailable; tool was not re-executed.'}], 'is_error': True}
        if (name not in BROWSER_TOOLS or not self.accepting or not self.binding
                or self.binding[0] != turn_id or not isinstance(result.get('browserReplayItemId'), str)):
            return unavailable
        row = self.engine.store.db.execute('''SELECT i.payload FROM items i JOIN turns t ON t.id=i.turn_id
            WHERE i.id=? AND i.turn_id=? AND t.thread_id=? AND t.state='inProgress'
              AND length(CAST(i.payload AS BLOB))<=?''',
            (result['browserReplayItemId'], turn_id, self.tid, 8 * 1024 * 1024)).fetchone()
        if not row:
            return unavailable
        try:
            item = json.loads(row[0])
            if item.get('tool') != name:
                return unavailable
            if item.get('type') == 'mcpToolCall' and item.get('server') == 'cutex':
                blocks = item['result']['content']
            elif item.get('type') == 'dynamicToolCall' and item.get('namespace') == 'claude':
                blocks = []
                for block in item['contentItems']:
                    if block.get('type') == 'inputText':
                        blocks.append({'type': 'text', 'text': block['text']})
                    elif block.get('type') == 'inputImage':
                        mime, data = block['imageUrl'].removeprefix('data:').split(';base64,', 1)
                        blocks.append({'type': 'image', 'mimeType': mime, 'data': data})
                    else:
                        return unavailable
            else:
                return unavailable
            replay = ToolResult.from_mcp(blocks, not result['is_error'])
            return {'content': replay.mcp_content(), 'is_error': not replay.success}
        except (KeyError, TypeError, ValueError, AttributeError):
            return unavailable


class SubscriptionEngine(Engine):
    def __init__(self, settings, store: Store, driver, tools: ToolRegistry):
        store.bind_backend('subscription')
        super().__init__(settings, store, driver, tools)
        self.driver = driver
        self.open_tools: dict[str, dict[str, dict]] = {}
        self.native_tool_inventory = {}
        self.offline_threads: set[str] = set()
        self.plan_state = PlanState(store)
        self.reset_launch_model = os.environ.get('CUTEX_CLAUDE_RESET_LAUNCH_MODEL') == '1'
        self.launch_models_applied: set[str] = set()
        self.model_observations = ModelObservations(store)
        # Auto-compaction per model (cutex's profile table and launch values; live_settings).
        self.compaction = Compaction()
        self.subagents = SubagentTranscripts(self)
        # Background subagent usage for turns still running (add_task_usage).
        self.pending_task_usage: dict[str, list[dict]] = {}
        store.db.execute('''CREATE TABLE IF NOT EXISTS sdk_sessions(
            thread_id TEXT PRIMARY KEY REFERENCES threads(id), session_id TEXT NOT NULL UNIQUE,
            attempted INTEGER NOT NULL DEFAULT 0, last_status TEXT NOT NULL DEFAULT 'allocated')''')
        store.db.execute('''CREATE TABLE IF NOT EXISTS thread_efforts(
            thread_id TEXT PRIMARY KEY REFERENCES threads(id), effort TEXT)''')
        store.db.execute('CREATE TABLE IF NOT EXISTS thread_launch_models(thread_id TEXT PRIMARY KEY REFERENCES threads(id), model TEXT NOT NULL)')
        # Claude Code fast mode per thread (a `fastMode` flag setting at connect).
        store.db.execute('CREATE TABLE IF NOT EXISTS thread_fast_mode(thread_id TEXT PRIMARY KEY REFERENCES threads(id), enabled INTEGER NOT NULL)')

    async def add_task_usage(self, tid: str, turn_id: str, task_id: str, usage: dict) -> None:
        """G9, best effort: a background subagent's model usage (its messages' usage,
        summed per API message) is added to its origin turn as a `modelRequests`
        entry of kind `background-task`. The thread's `total` then grows once, so
        cutex's usage ledger counts it under the session's profile. An origin turn
        still running takes it with its Result."""
        entry = {'kind': 'background-task', 'taskId': task_id[:256], 'outcome': 'received', 'usage': usage,
                 'billingScope': 'background-subagent-messages-estimate'}
        row = self.store.db.execute('SELECT state FROM turns WHERE id=? AND thread_id=?', (turn_id, tid)).fetchone()
        if row is None:
            return
        if row[0] == 'inProgress':
            self.pending_task_usage.setdefault(turn_id, []).append(entry)
            return
        await self._append_turn_usage(tid, turn_id, [entry])

    async def _flush_task_usage(self, tid: str, turn_id: str) -> None:
        """A turn that ended without its Result keeps the background usage it was given."""
        pending = self.pending_task_usage.pop(turn_id, [])
        if pending:
            await self._append_turn_usage(tid, turn_id, pending)

    async def _append_turn_usage(self, tid: str, turn_id: str, entries: list[dict]) -> None:
        row = self.store.db.execute('SELECT usage FROM turns WHERE id=?', (turn_id,)).fetchone()
        record = json.loads(row[0]) if row and row[0] else {}
        calls = list(record.get('modelRequests') or []) + entries
        self.store.db.execute('UPDATE turns SET usage=? WHERE id=?', (dump({**record, 'modelRequests': calls}), turn_id))
        await self._emit_usage(tid, turn_id, calls, keep_last=True)

    def ensure_launch_thread(self, tid: str, model: str, cwd: str, instructions: str) -> bool:
        """Apply changed host launch defaults only while the native owner is offline.

        An unchanged launch descriptor preserves an interactive model override.
        Profile switches change the descriptor; they keep native identity and
        queued inputs, and never submit or replay an instruction.
        """
        row = self.store.db.execute('SELECT model FROM threads WHERE id=?', (tid,)).fetchone()
        if row is None:
            created = self.store.ensure_thread(tid, model, cwd, instructions)
            self.launch_models_applied.add(tid)
            return created
        original = self.store.db.execute('SELECT model FROM thread_launch_models WHERE thread_id=?', (tid,)).fetchone()
        launch_model = original[0] if original else row[0]
        with self.store.transaction():
            self.store.ensure_thread(tid, launch_model, cwd, instructions)
            if launch_model != model or (self.reset_launch_model and tid not in self.launch_models_applied):
                owner = getattr(self.driver, 'owners', {}).get(tid)
                if tid in self.workers or (owner is not None and (not owner.task.done() or owner.lease is not None)):
                    raise RpcError(-32009, 'Close the native runtime before changing its launch model')
                self.store.db.execute('UPDATE threads SET model=?,updated=? WHERE id=?', (model, time.time(), tid))
                self.store.db.execute('INSERT OR REPLACE INTO thread_launch_models VALUES(?,?)', (tid, model))
        self.launch_models_applied.add(tid)
        return False

    def save_model_effort(self, tid: str, model: str, effort, fast: bool | None = None) -> None:
        with self.store.transaction():
            self.store.db.execute('INSERT OR IGNORE INTO thread_launch_models SELECT id,model FROM threads WHERE id=?', (tid,))
            self.store.db.execute('UPDATE threads SET model=? WHERE id=?', (model, tid))
            self.store.db.execute('INSERT OR REPLACE INTO thread_efforts VALUES(?,?)', (tid, effort))
            if fast is not None:
                self.store.db.execute('INSERT OR REPLACE INTO thread_fast_mode VALUES(?,?)', (tid, int(fast)))

    def thread_fast(self, tid: str) -> bool:
        row = self.store.db.execute('SELECT enabled FROM thread_fast_mode WHERE thread_id=?', (tid,)).fetchone()
        return bool(row[0]) if row else False

    def flag_settings(self, tid: str) -> str | None:
        """`--settings` JSON for the SDK child: an API account's settings plus
        fast mode (Claude Code requires the flag-settings opt-in in SDK mode)
        plus cutex's persisted allow rules (``permissions.allow``) plus
        ``disableAllHooks`` while the profile's command execution is off."""
        settings = self.flag_settings_dict(tid)
        return json.dumps(settings) if settings else None

    def flag_settings_dict(self, tid: str, *, explicit_rules: bool = False) -> dict:
        settings = dict(self.driver.api_account.cli_settings()) if getattr(self.driver, 'api_account', None) else {}
        if self.thread_fast(tid):
            settings['fastMode'] = True
        if not self.settings.allow_exec:
            # Exec off: no settings hooks (project / user / local settings, skill and agent
            # frontmatter, plugins) and no statusLine run commands. Flag settings outrank those
            # sources. The adapter's SDK callback hooks (`hooks=`, registered by `initialize`)
            # still run: Claude Code (checked in 2.1.289) only drops configured hooks for it.
            settings['disableAllHooks'] = True
        return permission_rules.with_rules(settings, permission_rules.load(self.settings.state_dir), explicit=explicit_rules)

    def thread_effort(self, tid: str):
        row = self.store.db.execute('SELECT effort FROM thread_efforts WHERE thread_id=?', (tid,)).fetchone()
        return row[0] if row else self.settings.effort

    async def set_model_effort(self, tid: str, model: str, effort, fast: bool | None = None, *, live: bool = False) -> None:
        """`live` (a CLI that takes these on the running child, live_settings): applied to the running
        child, also while a turn runs. Otherwise a model change alone goes live and an effort / fast mode
        change (or a model change that changes auto-compaction) replaces the idle child."""
        if fast is not None and fast == self.thread_fast(tid):
            fast = None
        owner = self.driver.owners.get(tid) if getattr(self.driver, 'persistent', False) else None
        if live and owner is not None and not owner.task.done():
            await owner.set_model_effort(model, effort, fast, live=True)
            return
        if effort == self.thread_effort(tid) and fast is None and not self.compaction.differs(self.store.thread(tid)['model'], model):
            await self.set_model(tid, model)
            return
        if owner is None or owner.task.done():
            if effort == self.thread_effort(tid) and model == self.store.thread(tid)['model']:
                # Fast mode alone, runtime offline: the next connect applies it.
                self.save_model_effort(tid, model, effort, fast)
                return
            raise RpcError(-32009, 'Start the Claude runtime before changing effort')
        if fast is None:
            await owner.set_model_effort(model, effort)
        else:
            await owner.set_model_effort(model, effort, fast)

    async def start_runtime(self, tid: str) -> dict:
        self.store.thread(tid)
        if not getattr(self.driver, 'persistent', False):
            raise RpcError(-32601, 'This driver does not implement persistent runtime lifecycle')
        status = await self.driver.start(self, tid)
        self.offline_threads.discard(tid)
        # Start is connection-only; no wake()/claim()/business prompt.
        return status

    async def runtime_control(self, tid: str, kind: str, **arguments):
        self.store.thread(tid)
        if not getattr(self.driver, 'persistent', False):
            raise RpcError(-32601, 'This driver does not implement native runtime controls')
        return await self.driver.control(self, tid, kind, **arguments)

    def runtime_status(self, tid: str) -> dict:
        self.store.thread(tid)
        result = self.driver.status(tid) if getattr(self.driver, 'persistent', False) else {'threadId': tid, 'state': 'notImplemented'}
        result['modelObservation'] = self.model_status(tid)
        return result

    def model_status(self, tid: str) -> dict:
        """Read stored evidence without starting a client or resolving an alias."""
        owner = self.driver.owners.get(tid) if getattr(self.driver, 'persistent', False) else None
        return self.model_observations.status(tid, owner)

    async def observe_model(self, tid: str, turn_id: str, message: Any) -> None:
        owner = self.driver.owners.get(tid) if getattr(self.driver, 'persistent', False) else None
        if (owner is None or owner.task.done() or owner.state != 'working' or
                not owner.active or owner.active.turn_id != turn_id or owner.active.interrupted or
                message_type(message) not in {'AssistantMessage', 'assistant'} or
                field(message, 'parent_tool_use_id') or field(message, 'agent_id') or
                field(message, 'error')):
            return
        sid = field(message, 'session_id')
        if sid is not None and sid != owner.session_id:
            return
        # SDK AssistantMessage has no required session_id. Its reader and active
        # Run bind it to the session; an explicit contradictory ID is rejected.
        if self.session(tid)['session_id'] != owner.session_id:
            return
        origin = field(message, 'origin')
        if origin is not None and (not isinstance(origin, dict) or origin.get('kind') != 'human'):
            return
        if self.model_observations.record(tid, turn_id, field(message, 'model'),
                requested_model=self.store.thread(tid)['model'], connection_id=owner.connection_id,
                connection_generation=owner.native_connection_generation, selection_id=owner.model_selection_id):
            await self.emit(tid, 'claude/model/observed', self.model_status(tid))

    async def set_model(self, tid: str, model: str, *, live: bool = False) -> None:
        current = self.store.thread(tid)['model']
        if current == model:
            return
        owner = self.driver.owners.get(tid) if getattr(self.driver, 'persistent', False) else None
        if owner is None:
            raise RpcError(-32009, 'Start the Claude runtime before changing model')
        if live or self.compaction.differs(current, model):
            # Live: the model and its auto-compaction together. Otherwise a model whose auto-compaction
            # differs needs a new child (the environment is read at start).
            await owner.set_model_effort(model, self.thread_effort(tid), live=live)
            return
        await owner.set_model(model)

    async def _interrupt_before_close(self, tid: str) -> None:
        """Stop or restart while a turn runs: interrupt it first, as cutex does
        for a Codex runtime before stopping it, so the turn ends `interrupted`
        with its open tools closed. Only a confirmed interrupt claims that: if
        the native runtime does not drain it in time, Close ends the turn as
        before (failed, outcome uncertain)."""
        if tid not in self.driver.owners:
            return
        row = self.store.db.execute("SELECT id FROM turns WHERE thread_id=? AND reverted=0 AND state='inProgress'", (tid,)).fetchone()
        if not row:
            return
        limit = getattr(self.driver, 'interrupt_timeout', 15) + getattr(self.driver, 'control_timeout', 10) + 1
        try:
            async with asyncio.timeout(limit):
                await self.interrupt(tid, row[0])
        except (Exception, TimeoutError):
            pass  # Close below settles the turn either way.

    async def close_runtime(self, tid: str) -> dict:
        self.store.thread(tid)
        self.offline_threads.add(tid)
        if not getattr(self.driver, 'persistent', False):
            raise RpcError(-32601, 'This driver does not implement persistent runtime lifecycle')
        await self._interrupt_before_close(tid)
        worker = self.workers.get(tid)
        # A worker not yet scheduled cannot own an SDK client. Cancelling that
        # waiter does not violate SDK task ownership.
        if worker and tid not in self.driver.owners:
            worker.cancel()
        status = await self.driver.close_runtime(tid)
        if worker:
            await asyncio.gather(worker, return_exceptions=True)
        # If Close raced the first scheduling of a worker, terminate the
        # local claimed turn too. Queued-but-unclaimed inputs remain queued.
        row = self.store.db.execute("SELECT id FROM turns WHERE thread_id=? AND reverted=0 AND state='inProgress'", (tid,)).fetchone()
        if row:
            await super().interrupt(tid, row[0])
        return status

    def wake(self, tid: str) -> None:
        if tid not in self.offline_threads:
            super().wake(tid)

    async def _worker(self, tid: str) -> None:
        try:
            while not self.closing and tid not in self.offline_threads:
                record = self.store.claim(tid)
                if record is None: break
                await self._run_turn(tid, record)
                if self.store.thread(tid)['halted']: break
        finally:
            if self.workers.get(tid) is asyncio.current_task():
                self.workers.pop(tid, None)

    async def interrupt(self, tid: str, turn_id: str) -> None:
        if not getattr(self.driver, 'persistent', False):
            return await super().interrupt(tid, turn_id)
        row = self.store.db.execute("SELECT id FROM turns WHERE thread_id=? AND reverted=0 AND state='inProgress'", (tid,)).fetchone()
        if not row or row[0] != turn_id:
            raise RpcError(-32009, 'turnId does not name the active turn')
        # The owner stops this turn: its unread steers are dropped now, not at the
        # terminal result. The client resends them (desktop interrupt & send), so
        # neither a PostToolUse before the abort lands nor a turn that completes
        # anyway (FIFO fallback) may deliver them a second time.
        with self.store.transaction():
            dropped = self.store.drop_unread_steers(tid)
        try:
            stopping = await self.driver.interrupt(tid, turn_id)
        except BaseException:
            with self.store.transaction():
                self.store.restore_steers(tid, dropped)
            raise
        if not stopping:
            return await super().interrupt(tid, turn_id)
        worker = self.workers.get(tid)
        if worker: await asyncio.shield(worker)

    async def close(self) -> None:
        if not getattr(self.driver, 'persistent', False):
            return await super().close()
        self.closing = True
        await asyncio.gather(*(self._interrupt_before_close(tid) for tid in list(self.driver.owners)))
        # No raw owner cancellation: let official disconnect flush and reap.
        await self.driver.close()
        tasks = list(self.workers.values())
        await asyncio.gather(*tasks, return_exceptions=True)
        await self.tools.close()

    def session(self, tid: str) -> dict:
        self.store.thread(tid)
        row = self.store.db.execute('SELECT * FROM sdk_sessions WHERE thread_id=?', (tid,)).fetchone()
        if not row:
            sid = identifier()
            self.store.db.execute('INSERT INTO sdk_sessions(thread_id,session_id) VALUES(?,?)', (tid, sid))
            row = self.store.db.execute('SELECT * FROM sdk_sessions WHERE thread_id=?', (tid,)).fetchone()
        return dict(row)

    def inspect_session(self, tid: str) -> dict | None:
        row = self.store.db.execute('SELECT * FROM sdk_sessions WHERE thread_id=?', (tid,)).fetchone()
        return dict(row) if row else None

    async def _run_turn(self, tid: str, record: dict) -> None:
        turn_id = record['turn_id']
        state, error = 'completed', None
        uncertain = False
        pending: dict[int, dict] = {}
        reasoning: dict[int, dict] = {}
        last_item: dict | None = None
        session = self.session(tid)
        self.open_tools[turn_id] = {}
        got_result = False
        terminal_failure = None
        planning = self.permissions.planning(tid)
        usage_calls = [{'kind': 'sdk-turn', 'outcome': 'requested', 'usage': None}]
        last_call: dict = {}

        async def begin_text(index: int) -> dict:
            old = pending.pop(index, None)
            if old is not None:
                await finish_text(old)
            item = {'type': 'agentMessage', 'id': identifier(), 'text': '', 'phase': 'commentary',
                    'memoryCitation': None, 'delivery': None, 'questions': None}
            pending[index] = item
            self.store.put_item(turn_id, item)
            await self.emit(tid, 'item/started', {'threadId': tid, 'turnId': turn_id, 'item': item})
            return item

        async def finish_text(item: dict) -> None:
            self.store.put_item(turn_id, item)
            await self.emit(tid, 'item/completed', {'threadId': tid, 'turnId': turn_id, 'item': item})

        # One Claude thinking block = one Codex `reasoning` item. Claude 4+
        # never returns the raw chain of thought: with display "summarized" the
        # text is a summary, so it goes to `summary` (one part per block), never
        # `content`. `reasoning` holds blocks still streaming, by stream index;
        # `streamed` holds blocks completed at content_block_stop that the
        # matching AssistantMessage block has not confirmed yet.
        streamed: list[dict] = []
        redacted: set[str] = set()

        async def begin_reasoning(index: int | None) -> dict:
            item = {'type': 'reasoning', 'id': identifier(), 'summary': [], 'content': []}
            if index is not None:
                old = reasoning.pop(index, None)
                if old is not None:
                    await finish_reasoning(old)
                reasoning[index] = item
            self.store.put_item(turn_id, item)
            await self.emit(tid, 'item/started', {'threadId': tid, 'turnId': turn_id, 'item': item})
            return item

        async def finish_reasoning(item: dict) -> None:
            self.store.put_item(turn_id, item)
            await self.emit(tid, 'item/completed', {'threadId': tid, 'turnId': turn_id, 'item': item})

        async def reasoning_delta(item: dict, chunk: str) -> None:
            if not item['summary']:
                item['summary'].append('')
                await self.emit(tid, 'item/reasoning/summaryPartAdded', {'threadId': tid, 'turnId': turn_id,
                    'itemId': item['id'], 'summaryIndex': 0}, durable=False)
            item['summary'][0] += chunk
            await self.emit(tid, 'item/reasoning/summaryTextDelta', {'threadId': tid, 'turnId': turn_id,
                'itemId': item['id'], 'delta': chunk, 'summaryIndex': 0}, durable=False)

        async def thinking_block(thought: str) -> None:
            """Final text of one thinking block (empty when display is omitted)."""
            summary = [thought] if thought.strip() else []
            same = next((i for i in streamed if i['summary'] == summary), None)
            if same is not None or streamed:
                # Already completed from the stream; correct it only if it differs.
                item = same if same is not None else streamed[0]
                streamed.remove(item)
                if item['summary'] != summary and summary:
                    item['summary'] = summary
                    await finish_reasoning(item)
                return
            live = [i for i in sorted(reasoning) if reasoning[i]['id'] not in redacted]
            item = reasoning.pop(live[0]) if live else await begin_reasoning(None)
            item['summary'] = summary or item['summary']
            await finish_reasoning(item)

        async def on_message(message: Any) -> None:
            nonlocal got_result, last_item, terminal_failure, last_call
            typ = message_type(message)
            sid = field(message, 'session_id')
            if typ in {'SystemMessage', 'system'}:
                data = field(message, 'data', {})
                sid = data.get('session_id', sid) if isinstance(data, dict) else sid
            if sid and sid != session['session_id']:
                raise ProviderError('Official SDK returned a different native session ID; refusing cross-session continuation.')
            if field(message, 'parent_tool_use_id'):
                return
            if typ in {'UserMessage', 'user', 'AssistantMessage', 'assistant'}:
                native_uuid = field(message, 'uuid')
                record_message = getattr(self.store, 'record_sdk_message', None)
                if isinstance(native_uuid, str) and native_uuid and record_message:
                    record_message(tid, turn_id, native_uuid,
                                   'user' if typ in {'UserMessage', 'user'} else 'assistant')
            if typ in {'SystemMessage', 'system'}:
                if field(message, 'subtype') == 'compact_boundary':
                    item = {'type': 'contextCompaction', 'id': identifier()}
                    self.store.put_item(turn_id, item)
                    await self.emit(tid, 'item/started', {'threadId': tid, 'turnId': turn_id, 'item': item})
                    await self.emit(tid, 'item/completed', {'threadId': tid, 'turnId': turn_id, 'item': item})
                return
            if typ in {'StreamEvent', 'stream_event'}:
                if planning:
                    return  # Publish complete blocks so plan markup never leaks into the transcript.
                event = field(message, 'event', {})
                kind, index = event.get('type'), event.get('index', 0)
                block_type = event.get('content_block', {}).get('type') if kind == 'content_block_start' else None
                delta_type = event.get('delta', {}).get('type') if kind == 'content_block_delta' else None
                if kind == 'message_start':
                    # A new native message may reuse every content-block index.
                    # Close any unconfirmed previous blocks before reusing them.
                    for item in list(pending.values()):
                        await finish_text(item)
                    pending.clear()
                    for item in list(reasoning.values()):
                        await finish_reasoning(item)
                    reasoning.clear()
                    streamed.clear()
                elif block_type == 'text':
                    item = await begin_text(index)
                    initial = event['content_block'].get('text', '')
                    if isinstance(initial, str) and initial:
                        item['text'] = initial
                        await self.emit(tid, 'item/agentMessage/delta', {'threadId': tid, 'turnId': turn_id, 'itemId': item['id'], 'delta': initial}, durable=False)
                elif block_type in {'thinking', 'redacted_thinking'}:
                    item = await begin_reasoning(index)
                    if block_type == 'redacted_thinking':
                        # Encrypted thinking: an empty item still shows that and how long Claude reasoned.
                        redacted.add(item['id'])
                    else:
                        initial = event['content_block'].get('thinking', '')
                        if isinstance(initial, str) and initial:
                            await reasoning_delta(item, initial)
                elif delta_type == 'text_delta':
                    item = pending.get(index) or await begin_text(index)
                    chunk = event['delta']['text']
                    item['text'] += chunk
                    await self.emit(tid, 'item/agentMessage/delta', {'threadId': tid, 'turnId': turn_id, 'itemId': item['id'], 'delta': chunk}, durable=False)
                elif delta_type == 'thinking_delta':
                    item = reasoning.get(index) or await begin_reasoning(index)
                    chunk = event['delta'].get('thinking', '')
                    if isinstance(chunk, str) and chunk:
                        await reasoning_delta(item, chunk)
                elif kind == 'content_block_stop' and index in reasoning:
                    item = reasoning.pop(index)
                    await finish_reasoning(item)
                    if item['id'] not in redacted:  # The SDK drops redacted blocks from AssistantMessage.
                        streamed.append(item)
            elif typ in {'AssistantMessage', 'assistant'}:
                if field(message, 'error'):
                    terminal_failure = SdkResponseError(message, getattr(self.driver, 'api_account', None))
                    return
                if synthetic_placeholder(message):
                    return
                await self.observe_model(tid, turn_id, message)
                usage = field(message, 'usage')
                if isinstance(usage, dict):
                    counts = usage_counts(usage)
                    if counts and counts != last_call:
                        # Context occupancy after each model request, while the
                        # turn runs (claude-r22): not only at its terminal result.
                        last_call = counts
                        await self._emit_usage(tid, turn_id, usage_calls, last=last_call)
                content = field(message, 'content', [])
                for block in content:
                    if message_type(block) in {'ThinkingBlock', 'thinking'}:
                        thought = field(block, 'thinking', '')
                        await thinking_block(thought if isinstance(thought, str) else '')
                    elif message_type(block) in {'TextBlock', 'text'}:
                        text = field(block, 'text', '')
                        if planning and '<proposed_plan>' in text and '</proposed_plan>' in text:
                            before, rest = text.split('<proposed_plan>', 1)
                            plan, after = rest.split('</proposed_plan>', 1)
                            for body, kind in [(before.strip(), 'agentMessage'), (plan.strip(), 'plan'), (after.strip(), 'agentMessage')]:
                                if not body:
                                    continue
                                item = {'type':kind, 'id':identifier(), 'text':body}
                                if kind == 'agentMessage':
                                    item.update(phase='commentary', memoryCitation=None, delivery=None, questions=None)
                                self.store.put_item(turn_id, item)
                                await self.emit(tid, 'item/started', {'threadId':tid,'turnId':turn_id,'item':item})
                                await self.emit(tid, 'item/completed', {'threadId':tid,'turnId':turn_id,'item':item})
                                last_item = item
                            continue
                        # Canonical text can differ from the streamed prefix.
                        # Match text blocks in their native arrival order, not
                        # by prose or absolute indices (the SDK drops redacted
                        # thinking blocks from its canonical content list).
                        matched = next(iter(pending), None)
                        item = pending.pop(matched) if matched is not None else await begin_text(-1)
                        if matched is None:
                            pending.pop(-1, None)
                        item['text'] = text
                        last_item = item
                        self.store.put_item(turn_id, item)
                        await self.emit(tid, 'item/completed', {'threadId': tid, 'turnId': turn_id, 'item': item})

            elif typ in {'ResultMessage', 'result'}:
                if got_result:
                    raise ProviderError('Unexpected second terminal result for one queued input')
                got_result = True
                if sid != session['session_id']:
                    raise ProviderError('Terminal result omitted the expected native session ID')
                counts = usage_counts(field(message, 'usage') or {})
                usage_calls[-1].update(outcome='received', usage=counts,
                    sdkNumTurns=field(message, 'num_turns'), billingScope='sdk-main-loop-reported-not-an-invoice')
                # Background subagents of this turn that already ended (add_task_usage).
                usage_calls[0:0] = self.pending_task_usage.pop(turn_id, [])
                self._usage(turn_id, usage_calls)
                await self._emit_usage(tid, turn_id, usage_calls, last=last_call or None,
                                       window=context_window(field(message, 'model_usage')))
                terminal = field(message, 'terminal_reason')
                if field(message, 'is_error') or field(message, 'subtype') != 'success' or terminal not in (None, 'completed'):
                    subtype = field(message, 'subtype')
                    def safe_reason(value):
                        return value if isinstance(value, str) and len(value) <= 80 and value.replace('_', '').isalnum() and value.isascii() else 'unknown'
                    reason = f"subtype={safe_reason(subtype)}, terminal={safe_reason(terminal)}"
                    if history_signature_rejected(field(message, 'errors', []) or []):
                        terminal_failure = terminal_failure or HISTORY_SIGNATURE_REJECTION
                    else:
                        terminal_failure = terminal_failure or f'Official runtime ended unsuccessfully ({reason}). You may submit a new instruction; the failed input is not automatically retried.'
                    return
                if last_item and last_item['type'] == 'agentMessage' and last_item.get('phase') != 'final_answer':
                    # Only the terminal result tells which text was the last one: it completed as
                    # commentary; live subscribers (desktop, delegation answers, activity) get the
                    # same item again as the final answer, like a reload of the saved snapshot.
                    last_item['phase'] = 'final_answer'
                    self.store.put_item(turn_id, last_item)
                    await self.emit(tid, 'item/completed', {'threadId': tid, 'turnId': turn_id, 'item': last_item})
                await self.busy_delivery.acknowledge(tid, turn_id)
                await self.emit(tid, 'claude/input/contextPersisted', {
                    'threadId': tid, 'turnId': turn_id, 'messageId': record['message_id']})

        try:
            await self.emit(tid, 'turn/started', {'threadId': tid, 'turn': self.store.turn(turn_id, include_items=False)})
            await self.emit(tid, 'thread/status/changed', {'threadId': tid, 'status': {'type': 'active', 'activeFlags': []}})
            await self.emit_input_presentation(tid, record["message_id"])
            payload = json.loads(record['payload'])
            compact = payload.get('eventType') == COMPACT_EVENT
            quota_notes = self.busy_delivery.continue_quota_notes(tid, turn_id) if payload['source']['kind'] == 'user' and not compact else []
            for item in self.store.items(turn_id):
                await self.emit(tid, 'item/started', {'threadId': tid, 'turnId': turn_id, 'item': item})
                await self.emit(tid, 'item/completed', {'threadId': tid, 'turnId': turn_id, 'item': item})
            self._usage(turn_id, usage_calls)
            prompt = '/compact' if compact else (record.get('prompt') or Store.project_input(payload)[0]['text'])
            if planning and not compact:
                prompt = ('[Frontend Plan mode: use read-only tools and AskUserQuestion as needed. '
                          'This frontend displays and stores your final plan; no plan-file writing or ExitPlanMode tool is available. '
                          'Return the final plan inside <proposed_plan>...</proposed_plan> and end the turn. '
                          'The user will approve through the frontend before implementation.]\n\n' + prompt)
            # Other agents' and services' text is data: never expand its @paths or
            # run its slash commands. The user's own text keeps @path mentions, but
            # a leading "/" is literal; only thread/compact/start runs /compact.
            verbatim = payload['source']['kind'] != 'user' or (not compact and prompt.lstrip().startswith('/'))
            # Notes read in at an owner turn's start (Store.claim) go first, each its own
            # text block: Claude Code expands @path mentions only in the last text block
            # (the owner's text), never in these. An agent's turn has them in its prompt.
            notes = [*quota_notes, *self.store.turn_note_frames(turn_id)] if payload['source']['kind'] == 'user' and not compact else []
            # A Side chat's first message follows its instructions and boundary (cute-codex /side).
            side = getattr(self, 'side_chats', None)
            if side is not None and not compact and side.boundary_pending(tid):
                notes = [*SIDE_BOUNDARY_BLOCKS, *notes]
            message = sdk_user_message(prompt, payload.get('attachments') or [], verbatim=verbatim, preceding=notes)
            delays, attempt = refresh_retry_delays(), 0
            while True:
                await self.driver.run(engine=self, tid=tid, turn_id=turn_id,
                    session_id=session['session_id'], resume=bool(session['attempted']),
                    prompt=prompt, message=message, on_message=on_message)
                silent = last_item is None and not pending and not reasoning and not self.open_tools.get(turn_id)
                if not (isinstance(terminal_failure, SdkResponseError) and terminal_failure.transient_refresh
                        and silent and attempt < len(delays)):
                    break
                # The OAuth refresh lock was held (another Claude Code process on this
                # login was refreshing an expired token). Claude produced nothing, so
                # the same input is sent again in this turn once the lock is free. The
                # native transcript keeps the failed copy of the message.
                delay = delays[attempt]
                attempt += 1
                await self.emit(tid, 'error', {'threadId': tid, 'turnId': turn_id, 'willRetry': True, 'error': {
                    'message': f'Claude is waiting for its login token refresh (another Claude Code process holds it); '
                               f'retrying in {delay:g} s ({attempt}/{len(delays)}).',
                    'codexErrorInfo': 'other', 'additionalDetails': None}}, durable=False)
                await asyncio.sleep(delay)
                terminal_failure, got_result = None, False
                usage_calls.append({'kind': 'sdk-turn', 'outcome': 'requested', 'usage': None, 'retry': attempt})
                self._usage(turn_id, usage_calls)
            if attempt and isinstance(terminal_failure, SdkResponseError) and terminal_failure.transient_refresh:
                terminal_failure.args = (
                    f'Official Claude could not refresh its login token: another Claude Code process using this account '
                    f'kept the refresh lock. The input was sent again {attempt} time(s) without success; send it again '
                    f'later or check the login (cutex human claude account status).',)
            await self.metrics.refresh_context(tid)
            if terminal_failure:
                raise terminal_failure if isinstance(terminal_failure, SdkResponseError) else ProviderError(terminal_failure)
            if not got_result:
                raise ProviderError('Official SDK stream ended without a terminal result; outcome uncertain, no replay.')
        except (asyncio.CancelledError, TurnInterrupted):
            state = 'interrupted'
        except Exception as exc:
            state = 'failed'
            uncertain = isinstance(exc, OutcomeUncertain)
            account = getattr(self.driver, 'api_account', None)
            fallback = (f'{api_error_subject(account)} turn failed ({type(exc).__name__}). Check the API account configuration.'
                        if account else f'Subscription turn failed ({type(exc).__name__}). Inspect official login/seat access; no API fallback.')
            msg = str(exc) if isinstance(exc, (RpcError, ProviderError)) else fallback
            error = {'message': msg, 'codexErrorInfo': exc.codex_error_info if isinstance(exc, SdkResponseError) else 'other', 'additionalDetails': None}
        finally:
            for item in list(pending.values()) + list(reasoning.values()):
                self.store.put_item(turn_id, item)
                await self.emit(tid, 'item/completed', {'threadId': tid, 'turnId': turn_id, 'item': item})
            for item in self.open_tools.pop(turn_id, {}).values():
                interrupted_item(item, 'Execution interrupted; inspect the execution outcome and any side effects.')
                self.store.put_item(turn_id, item)
                await self.emit(tid, 'item/completed', {'threadId': tid, 'turnId': turn_id, 'item': item})
            self.store.db.execute('UPDATE sdk_sessions SET last_status=? WHERE thread_id=?', (state, tid))
            self.store.finish(tid, turn_id, state, error)
            await self._flush_task_usage(tid, turn_id)
            if uncertain:
                self.store.db.execute("UPDATE inputs SET state='uncertain' WHERE turn_id=?", (turn_id,))
            await self.emit(tid, 'turn/completed', {'threadId': tid, 'turn': self.store.turn(turn_id)})
            await self.emit(tid, 'thread/status/changed', {'threadId': tid, 'status': {'type': 'idle'}})
            await self.emit(tid, 'claude/input/statusChanged', self.store.input_status(tid, record['message_id']))


def usage_counts(usage: dict) -> dict:
    counts = {k: max(0, v) for k, v in usage.items()
              if k in {'input_tokens', 'output_tokens', 'cache_creation_input_tokens', 'cache_read_input_tokens'} and type(v) is int}
    # Preserve actual cache policy evidence without adding these breakdowns to
    # token totals a second time. Unknown/zero creation never implies a TTL.
    creation = usage.get('cache_creation')
    if isinstance(creation, dict):
        for ttl in ('5m', '1h'):
            value = creation.get(f'ephemeral_{ttl}_input_tokens')
            if type(value) is int and value >= 0:
                counts[f'cache_creation_ephemeral_{ttl}_input_tokens'] = value
    return counts


def context_window(model_usage) -> int | None:
    """Largest context window the CLI reported for the models used in this turn."""
    if not isinstance(model_usage, dict):
        return None
    windows = [v.get('contextWindow') for v in model_usage.values() if isinstance(v, dict)]
    windows = [w for w in windows if type(w) is int and w > 0]
    return max(windows) if windows else None
