"""A declared subset of cute-codex r91 app-server. Unknown operations fail closed.

The claude/* extension remains compatible; native ExternalInput uses its own
validated owner/generation-bound envelope and persistence receipts.
All clients holding the RPC transport token are trusted controllers; source.id
is caller-supplied provenance, not cryptographically authenticated agent identity.
"""
from __future__ import annotations

import asyncio
import json
import platform
import inspect
import logging
import logging.handlers
import traceback
import datetime
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Awaitable, Callable

from . import __version__, asks
from .config import Settings
from .engine import Engine
from .errors import RpcError, require, text
from .provider import ProviderError
from .store import COMPACT_EVENT, Store, dump, identifier
from .model_config import ModelConfig
from .model_catalog import FAST_TIER, ModelCatalog, fast_mode_supported, native_models
from .approvals import Approvals
from .history import PROVIDER, History, thread_wire
from .inputs import parse_turn_input
from .rate_limits import RateLimits
from .runtime_metrics import RuntimeMetrics
from .r8_protocol import R8Protocol
from .external_input import ExternalInputService, METHODS as EXTERNAL_INPUT_METHODS
from .skills import ManagedSkills
from .r8_services import read_usage
from .busy_delivery import BusyDelivery
from .safety_grants import SafetyGrants, METHODS as SAFETY_METHODS

Send = Callable[[dict], Awaitable[None]]
FEATURES = json.loads((Path(__file__).with_name("features.json")).read_text(encoding="utf-8"))

# Codex v2 methods with no Claude equivalent: a typed error, never a hang.
# `data.feature` names the `features` flag a client can use to hide the UI.
UNSUPPORTED = {
    # Served for the subscription backend (`_steer`); only other backends refuse it.
    "turn/steer": ("turnSteer", "Adding input to a running turn requires the Claude subscription backend"),
    "thread/goal/set": ("goals", "Codex thread goals have no Claude Agent SDK equivalent"),
    "review/start": ("review", "Codex review mode has no Claude equivalent; ask for a review in a normal turn"),
    "thread/memoryMode/set": ("memoryMode", "Codex native memories do not apply to Claude agents"),
    "thread/rollback": ("historyRevert", "use thread/revert with beforeTurnId"),
    "thread/shellCommand": ("userShell", "use cutex /v2/sessions/{cutexSessionId}/terminals"),
    "thread/realtime/start": ("realtime", None),
}

# Composer permission presets <-> Claude permission modes. Claude has no OS
# sandbox; the sandbox shape only names the preset the desktop composer shows.
SANDBOX_FOR_MODE = {
    "bypassPermissions": {"type": "dangerFullAccess"},
    "acceptEdits": {"type": "workspaceWrite", "writableRoots": [], "networkAccess": True,
                    "excludeTmpdirEnvVar": False, "excludeSlashTmp": False},
    "default": {"type": "readOnly", "networkAccess": False},
    "auto": {"type": "dangerFullAccess"},
}
SANDBOX_TYPES = {"danger-full-access": "dangerFullAccess", "workspace-write": "workspaceWrite", "read-only": "readOnly"}


def unsupported(method: str) -> RpcError:
    feature, hint = UNSUPPORTED.get(method, (method, None))
    message = f"{method} is not supported for Claude agents" + (f": {hint}" if hint else "")
    return RpcError(-32601, message, {"unsupported": True, "runtime": "claude", "feature": feature, "method": method, "hint": hint})


def requested_mode(params: dict, current: str) -> str | None:
    """Claude permission mode for a Codex approval/sandbox patch, or None if untouched."""
    sandbox = params.get("sandboxPolicy")
    sandbox_type = sandbox.get("type") if isinstance(sandbox, dict) else SANDBOX_TYPES.get(params.get("sandbox"))
    approval, reviewer = params.get("approvalPolicy"), params.get("approvalsReviewer")
    if approval is None and reviewer is None and sandbox_type is None:
        return None
    if approval == "never" or (approval is None and current == "bypassPermissions" and sandbox_type in (None, "dangerFullAccess")):
        return "bypassPermissions"
    if reviewer == "auto_review" or (reviewer is None and current == "auto" and sandbox_type is None):
        return "auto"
    if sandbox_type == "workspaceWrite":
        return "acceptEdits"
    if sandbox_type == "readOnly":
        return "default"
    return current if current in {"default", "acceptEdits"} else "default"


@dataclass(eq=False)
class Peer:
    send: Send
    connected_order: int = 0
    interactive: bool = False
    initialized: bool = False
    ready: bool = False
    extensions: bool = False
    subscriptions: set[str] = field(default_factory=set)
    opted_out: set[str] = field(default_factory=set)


class Server:
    def __init__(self, settings: Settings, store: Store, engine: Engine):
        self.settings, self.store, self.engine = settings, store, engine
        self.history = History(store)
        self.model_config = ModelConfig(settings)
        self.model_catalog = ModelCatalog(self)
        self.peers: set[Peer] = set()
        self.runtime_id = identifier()
        self.shutdown_requested = False
        engine.notify = self.broadcast
        engine.notify_view = self.broadcast_view
        self.peer_sequence = 0
        self.approvals = Approvals(self)
        engine.approve_tool = self.approvals.approve
        engine.rate_limits = RateLimits(store)
        engine.metrics = RuntimeMetrics(engine)
        self.r8 = R8Protocol(self)
        self.skills = ManagedSkills(settings)
        self.external_inputs = ExternalInputService(self)
        self.busy_delivery = engine.busy_delivery = BusyDelivery(self)
        self.safety_grants = engine.safety_grants = SafetyGrants(self)

    def connect(self, send: Send) -> Peer:
        self.peer_sequence += 1
        peer = Peer(send, connected_order=self.peer_sequence)
        self.peers.add(peer)
        return peer

    def disconnect(self, peer: Peer) -> None:
        self.peers.discard(peer)
        self.approvals.disconnect(peer)
        # Intentionally do not cancel model work when a visible TUI disconnects.

    async def broadcast(self, tid: str, method: str, params: dict) -> None:
        self.busy_delivery.observe(tid, method, params)
        await self.external_inputs.observe_event(tid, method, params)
        # Transport send() is a bounded queue operation, not network I/O.
        for peer in list(self.peers):
            if not peer.ready or tid not in peer.subscriptions or method in peer.opted_out:
                continue
            if method.startswith("claude/") and not peer.extensions:
                continue
            try:
                await peer.send({"method": method, "params": params})
            except Exception:
                self.disconnect(peer)

    async def broadcast_view(self, tid: str, method: str, params: dict) -> None:
        """Presentation frames for subscribers of `tid` whose `params.threadId` names another
        (subagent) thread: never logged as `tid`'s events nor observed by its input bookkeeping."""
        for peer in list(self.peers):
            if not peer.ready or tid not in peer.subscriptions or method in peer.opted_out:
                continue
            try:
                await peer.send({"method": method, "params": params})
            except Exception:
                self.disconnect(peer)

    async def handle(self, peer: Peer, message: object) -> None:
        if isinstance(message, dict) and 'method' not in message and 'id' in message:
            self.approvals.respond(peer, message)
            return
        rid = message.get("id") if isinstance(message, dict) else None
        try:
            require(isinstance(message, dict), "JSON-RPC message must be an object", -32600)
            require(message.get("jsonrpc", "2.0") == "2.0", "invalid JSON-RPC version", -32600)
            require(isinstance(message.get("method"), str), "method must be a string", -32600)
            require("id" not in message or (isinstance(rid, (str, int)) and not isinstance(rid, bool)), "request id must be a string or integer", -32600)
            method = message["method"]
            params = message.get("params")
            if params is None:
                params = {}
            require(isinstance(params, dict), "params must be an object")
            if "id" not in message:
                if method == "initialized":
                    require(peer.initialized, "initialize first", -32002)
                    peer.ready = True
                # Unknown notifications have no response and no side effects.
                return
            result, deferred = await self.dispatch(peer, method, params)
            try:
                await peer.send({"id": rid, "result": result})
            finally:
                # A lost response cannot strand a durably accepted operation.
                if deferred:
                    outcome = deferred()
                    if inspect.isawaitable(outcome):
                        await outcome
        except RpcError as exc:
            await peer.send({"id": rid, "error": exc.wire()})
        except ProviderError as exc:
            await peer.send({"id": rid, "error": {"code": -32020, "message": str(exc)[:512]}})
        except Exception as exc:
            diagnostic_id = identifier()
            # Stack locations only: no request payload, locals, or exception strings.
            try:
                logger = logging.getLogger("cute_claude.rpc." + str(self.settings.state_dir))
                if not logger.handlers:
                    handler = logging.handlers.RotatingFileHandler(self.settings.state_dir / "rpc-errors.jsonl", maxBytes=1048576, backupCount=2)
                    logger.addHandler(handler)
                    logger.propagate = False
                    (self.settings.state_dir / "rpc-errors.jsonl").chmod(0o600)
                frames = [{"file": f.filename, "line": f.lineno, "function": f.name} for f in traceback.extract_tb(exc.__traceback__)]
                logger.error(json.dumps({"time": datetime.datetime.now(datetime.timezone.utc).isoformat(), "diagnosticId": diagnostic_id, "requestId": rid,
                    "method": message.get("method") if isinstance(message, dict) else None,
                    "threadId": params.get("threadId") if isinstance(locals().get("params"), dict) else None,
                    "exception": type(exc).__name__, "frames": frames}))
            except Exception:
                print("Claude RPC diagnostic write failed", file=sys.stderr)
            await peer.send({"id": rid, "error": {"code": -32603, "message": f"Internal error ({type(exc).__name__}); diagnostic {diagnostic_id}"}})

    def capabilities(self) -> dict:
        subscription = self.settings.backend == "subscription"
        return {"implementation": "cute-claude-app-server", "version": __version__, "runtimeId": self.runtime_id,
                "backend": "mock" if self.settings.mock else ("claude-subscription-sdk" if subscription else "anthropic-messages"),
                "officialClaudeCode": False, "officialRuntimeDependency": subscription,
                "cliDependency": subscription, "apiKeyRequired": not subscription and not self.settings.mock,
                "subscriptionEntitlementVerified": False, "extraChargesDisabled": False, "input": {"version": 1, "submit": "claude/input/submit",
                    "status": "claude/input/status", "policy": "fifo-separate-turns", "receiptScope": "local-inbox",
                    "callerSuppliedSource": True,
                    # Owner `turn/steer`: read after the running turn's next main-agent tool
                    # call (PostToolUse), else the next turn (docs/BUSY_DELIVERY.md).
                    "sameTurnSteering": subscription, "steerDelivery": "next-tool-call-or-next-turn" if subscription else None,
                    "admissions": [asks.ADMISSION], "askEvents": list(asks.EVENT_TYPES),
                    # cutex agent messaging v2 (0.4.0): `view` param, passive notes,
                    # `external_event` timeline items keyed by message id.
                    "views": True, "deliveryModes": ["after-turn", "passive"] + (["soon"] if subscription else []),
                    "durableNextStepResults": subscription, "itemProjection": True},
                "history": "paginated", "tools": {"python": True, "httpJson": True, "mcp": subscription, "externalMcpServerRequired": False,
                    "nativePreset": "claude_code" if subscription else None,
                    "nativeInventoryMethod": "claude/tools/list" if subscription else None,
                    "writeEnabled": self.settings.allow_write, "execEnabled": self.settings.allow_exec,
                    "osSandbox": False, "interactiveApprovals": subscription,
                    "interactiveApprovalScope": "sdk-tools" if subscription else [],
                    "interactiveApprovalConfiguration": "claude/approvals/set", "nativePermissionPolicy": subscription, "askUserQuestion": subscription, "planMode": subscription,
                    "livePermissionMode": subscription, "persistentAllowRules": "claude/permissionRules/reload" if subscription else None,
                    "agentTurnQuestions": self.settings.agent_turn_questions if subscription else None},
                "presentationOnly": False, "automaticCompaction": False,
                "contextManagement": "official-sdk-native" if subscription else "local-messages-history",
                "nativeTranscriptAuthority": "official-sdk" if subscription else "adapter",
                "automaticCrashReplay": False, "readOnlyResume": True,
                "ensureThread": "claude/thread/ensure",
                "persistentRuntime": bool(subscription and getattr(self.engine.driver, "persistent", False)),
                "runtimeControl": ["claude/runtime/start", "claude/runtime/status", "claude/runtime/close"] if subscription else [],
                "continuousEventReader": subscription, "idleKeepaliveModelRequests": False,
                "features": self.features(), **self.external_inputs.capabilities()}

    def features(self) -> dict:
        """Codex v2 surface this adapter serves (features.json); off without the SDK."""
        features = dict(FEATURES["features"])
        features['externalInput'] = bool(self.settings.external_owner_id is not None
                                         and self.settings.external_runtime_generation is not None)
        if self.settings.backend != "subscription":
            native = {"imageInput", "reasoning", "commandItems", "fileChangeItems", "turnDiff", "approvals",
                      "userInputRequests", "agentTurnQuestions", "planMode", "planChecklist", "compact", "effort", "rateLimits",
                      "historyRevert", "fork", "threadDelete", "subagents", "subagentTranscripts", "autoReconnect", "backgroundTerminals", "mcpStatus", "fileRewind",
                      "versionedModelCatalog", "modelObservation", "safetyGrants", "fastMode",
                      "turnSteer", "permissionRules"}
            features.update({name: False for name in native})
            features["permissionPresets"] = ["full"]
            features["claudePermissionModes"] = ["bypassPermissions"]
        return features

    def _thread_wire(self, tid: str, include_turns: bool = False) -> dict:
        self.store.thread(tid)
        active = self.store.db.execute("SELECT 1 FROM turns WHERE thread_id=? AND state='inProgress' LIMIT 1", (tid,)).fetchone()
        kinds = {p.kind for p in self.approvals.pending.values() if p.thread_id == tid}
        flags = (["waitingOnApproval"] if kinds - {"question"} else []) + (["waitingOnUserInput"] if "question" in kinds else [])
        status = {"type": "active", "activeFlags": flags} if active else {"type": "idle"}
        return thread_wire(self.store, self.history, tid, include_turns=include_turns,
                           effort=self._effort(tid), status=status)

    def _thread_response(self, tid: str, include_turns: bool = False) -> dict:
        return {"thread": self._thread_wire(tid, include_turns), "model": self.store.thread(tid)["model"],
                "modelProvider": PROVIDER, "serviceTier": self._service_tier(tid), "cwd": str(self.settings.workspace),
                "instructionSources": [], **self._permission_wire(tid),
                "sandbox": self._sandbox_wire(tid), "reasoningEffort": self._effort(tid)}

    def _sandbox_wire(self, tid):
        if self.settings.backend != "subscription":
            return {"type": "dangerFullAccess"}
        return dict(SANDBOX_FOR_MODE.get(self.approvals.mode(tid), {"type": "dangerFullAccess"}))

    def _thread_settings(self, tid):
        model = self.store.thread(tid)['model']
        return {"cwd": str(self.settings.workspace), **self._permission_wire(tid),
                "sandboxPolicy": self._sandbox_wire(tid), "activePermissionProfile": None,
                "model": model, "modelProvider": PROVIDER, "serviceTier": self._service_tier(tid), "effort": self._effort(tid),
                "summary": None, "personality": None,
                "collaborationMode": {"mode": "plan" if self.approvals.planning(tid) else "default",
                    "settings": {"model": model, "reasoning_effort": self._effort(tid), "developer_instructions": None}}}

    async def _replay_recovered(self, peer: Peer, tid: str) -> None:
        """The events that settled a turn the previous adapter process left
        running (Store.recover): written while nobody was connected, so each
        subscriber of this process gets them once it resumes the thread,
        until the thread's next turn starts."""
        for method, params in list(getattr(self.store, 'recovered_events', {}).get(tid, ())):
            if not peer.ready or method in peer.opted_out:
                continue
            try:
                await peer.send({"method": method, "params": params})
            except Exception:
                self.disconnect(peer)
                return

    async def _emit_settings(self, tid):
        await self.engine.metrics.refresh_context(tid)
        await self.engine.emit(tid, "thread/settings/updated", {"threadId": tid, "threadSettings": self._thread_settings(tid)})

    def _permission_wire(self, tid):
        mode = self.approvals.mode(tid)
        return {'approvalPolicy':'never' if mode == 'bypassPermissions' else 'on-request',
                'approvalsReviewer':'auto_review' if mode == 'auto' else 'user'}

    def _fast(self, tid) -> bool:
        thread_fast = getattr(self.engine, 'thread_fast', None)
        return bool(self.settings.backend == 'subscription' and thread_fast and thread_fast(tid))

    def _service_tier(self, tid):
        """Codex `serviceTier` of Claude Code fast mode: the Fast tier or null (standard)."""
        return FAST_TIER if self._fast(tid) else None

    def _effort(self, tid):
        return self.engine.thread_effort(tid) if self.settings.backend == 'subscription' else self.settings.effort

    def _validate_launch(self, params: dict) -> None:
        require(params.get("historyMode") in (None, "legacy", "paginated"), "unsupported historyMode")
        require(params.get("modelProvider") in (None, PROVIDER), "select modelProvider=cute-claude")
        if params.get("model") is not None:
            text(params["model"], "model", limit=256)
            if self.settings.backend != "subscription":
                require(params["model"] == self.settings.model, "this backend exposes only its configured model")
        require(params.get("approvalPolicy") in ((None, "never", "on-request") if self.settings.backend == "subscription" else (None, "never")), "Claude supports on-request or never approval policy")
        if self.settings.backend == "subscription":
            # Preset names only: workspace-write = Claude acceptEdits, read-only =
            # Claude default (asks before edits and commands). No OS sandbox.
            sandbox_policy = params.get("sandboxPolicy")
            require(params.get("sandbox") in (None, *SANDBOX_TYPES), "unsupported sandbox mode")
            require(sandbox_policy is None or (isinstance(sandbox_policy, dict) and sandbox_policy.get("type") in SANDBOX_TYPES.values()),
                    "sandboxPolicy must be dangerFullAccess, workspaceWrite or readOnly")
        else:
            require(params.get("sandbox") in (None, "danger-full-access"), "OS sandbox unsupported; explicitly select danger-full-access")
            require(params.get("sandboxPolicy") in (None, {"type": "dangerFullAccess"}), "sandboxPolicy enforcement unsupported; select dangerFullAccess")
        require(params.get("approvalsReviewer") in ((None, "user", "auto_review") if self.settings.backend == "subscription" else (None, "user")), "unsupported approval reviewer")
        mode = params.get("collaborationMode")
        if mode is not None:
            require(self.settings.backend == 'subscription' and isinstance(mode, dict)
                    and mode.get('mode') in {'default', 'plan'}, 'unsupported collaboration mode')
            require(isinstance(mode.get('settings', {}), dict)
                    and not mode.get('settings', {}).get('developer_instructions'), 'custom collaboration instructions unsupported')
        require(not params.get("permissionProfile"), "permission profile enforcement is unsupported")
        require(not params.get("ephemeral"), "ephemeral threads are not supported")
        require(not params.get("dynamicTools"), "client-owned dynamic tools unsupported; register host Python/HTTP tools")
        require(params.get("serviceTier") in (None, "default")
                or (params.get("serviceTier") == FAST_TIER and params.get("threadId") and self._fast(params["threadId"])),
                "Use thread/settings/update to change Claude fast mode (serviceTier priority)")
        selected_effort = self._effort(params["threadId"]) if params.get("threadId") else self.model_config.effort()
        for field_name in ("reasoningEffort", "effort"):
            requested_effort = params.get(field_name)
            require(requested_effort in (None, selected_effort) or (requested_effort == "none" and selected_effort is None),
                    "Use thread/settings/update to change the current session effort before sending a turn")
        if params.get("cwd"):
            require(Path(params["cwd"]).resolve() == self.settings.workspace, "cwd must equal the server's configured workspace")

    async def _models(self) -> dict:
        model = self.model_config.model()
        subscription = self.settings.backend == "subscription"
        if subscription:
            entries = (await self.model_catalog.read())["entries"]
        else:
            models = list(dict.fromkeys([model] + [r[0] for r in
                self.store.db.execute("SELECT DISTINCT model FROM threads LIMIT 100")]))
            entries = [{"id": value, "displayName": value, "description": "Current model",
                        "supportedEffortLevels": [self.settings.effort] if self.settings.effort else []}
                       for value in models]
        return native_models(entries, default_model=model, configured_effort=self.model_config.effort(),
                             subscription=subscription, mock=self.settings.mock)

    async def dispatch(self, peer: Peer, method: str, params: dict) -> tuple[dict, Callable | None]:
        require(not self.shutdown_requested, "adapter is shutting down", -32009)
        if method == "initialize":
            require(not peer.initialized, "already initialized", -32600)
            caps = params.get("capabilities") or {}
            require(isinstance(caps, dict), "capabilities must be an object")
            client = params.get('clientInfo') or {}
            peer.interactive = ((isinstance(client,dict) and client.get('name') == 'codex-tui')
                                or caps.get('claudeInteractiveApprovals') is True)
            opt = caps.get("optOutNotificationMethods") or []
            require(isinstance(opt, list) and all(isinstance(x, str) for x in opt), "invalid notification opt-outs")
            peer.initialized, peer.extensions, peer.opted_out = True, bool(caps.get("claudeExtensions")), set(opt)
            return {"userAgent": f"cute-claude-app-server/{__version__}", "codexHome": str(self.settings.state_dir),
                    "presentationVersion": 1, "platformFamily": "windows" if sys.platform == "win32" else "unix", "platformOs": platform.system().lower(),
                    **self.external_inputs.capabilities()}, None
        require(peer.initialized and peer.ready, "initialize and initialized are required", -32002)
        if method in EXTERNAL_INPUT_METHODS:
            return await self.external_inputs.dispatch(peer, method, params)
        if method == 'claude/externalInput/bind':
            return self.external_inputs.bind(params), None
        if method in {'claude/runtime/start', 'thread/settings/update', 'claude/approvals/set'}:
            require(params.get('threadId') not in self.store.history_mutations,
                    'thread history is being changed; retry after the operation completes', -32009)
        if method in SAFETY_METHODS:
            require(self.settings.backend == 'subscription', 'Claude subscription backend required', -32601)
            return await self.safety_grants.dispatch(peer, method, params), None
        extension = await self.r8.dispatch(peer, method, params)
        if extension is not None:
            return extension
        if method.startswith(('fs/', 'process/', 'command/exec', 'fuzzyFileSearch')):
            raise RpcError(-32601, 'Host operations use the cutex workspace API', {
                'unsupported': True, 'runtime': 'claude', 'feature': 'hostWorkspace', 'method': method,
                'hint': 'Use /v2/sessions/{cutexSessionId}/fs/{dir,file,search,watch} and /terminals; control PTYs under /v2/terminals/{id}.'})
        if method == 'thread/name':
            method = 'thread/name/set'
        if method == 'account/usage/read':
            return read_usage(self.store, params), None
        if method == "claude/server/shutdown":
            require(params.get("runtimeId") == self.runtime_id, "stale adapter identity", -32011)
            require(not self.engine.workers, "turn workers are still active", -32009)
            for owner in getattr(self.engine.driver, "owners", {}).values() if hasattr(self.engine, "driver") else []:
                require(owner.task.done() and owner.lease is None, "native child exit is not confirmed", -32009)
            self.shutdown_requested = True
            return {"runtimeId": self.runtime_id, "shutdownAccepted": True}, None
        if method in {'claude/approvals/get', 'claude/approvals/set'}:
            require(self.settings.backend == 'subscription', 'Claude subscription backend required')
            tid = text(params.get('threadId'), 'threadId', limit=256)
            self.store.thread(tid)
            if method.endswith('/set'):
                return await self.approvals.configure(tid, params.get('mode')), None
            return {'threadId':tid,'mode':self.approvals.mode(tid),'scope':'sdk-tools'}, None
        if method == "claude/tools/list":
            require(self.settings.backend == 'subscription', 'Claude subscription backend required')
            tid = text(params.get('threadId'), 'threadId', limit=256)
            self.store.thread(tid)
            from .native_tools import exclusions
            return {'threadId':tid, 'preset':'claude_code', 'excluded':exclusions(self.settings),
                    'observedTools':self.engine.native_tool_inventory.get(tid),
                    'customTools':list(self.engine.tools.tools), 'observationSource':'native-system-init'}, None
        if method == "claude/capabilities":
            return self.capabilities(), None
        if method == "thread/timeline/list":
            tid = text(params.get("threadId"), "threadId", limit=256)
            return self.store.timeline(tid, self._cursor(params["cursor"]) if params.get("cursor") is not None else None, self._limit(params.get("limit"), 100)), None
        if method == "model/list":
            return await self._models(), None
        if method in {"claude/models/catalog", "claude/model/status"}:
            feature = "versionedModelCatalog" if method.endswith("/catalog") else "modelObservation"
            if self.settings.backend != "subscription":
                raise RpcError(-32601, "This model inspection method requires the Claude subscription backend", {
                    "unsupported": True, "runtime": "claude", "feature": feature, "method": method})
            require(set(params) <= {"threadId"}, "only threadId is accepted")
            tid = text(params["threadId"], "threadId", limit=256) if "threadId" in params else None
            if method == "claude/models/catalog":
                return await self.model_catalog.read(tid), None
            require(tid is not None, "threadId is required")
            return self.engine.model_status(tid), None
        if method == "account/rateLimits/read":
            await self.engine.metrics.refresh_status(peer.subscriptions)
            return self.engine.rate_limits.response(), None
        if method == "account/read":
            return {"account": None, "requiresOpenaiAuth": False}, None
        if method == "configRequirements/read":
            return {"requirements": None}, None
        if method == "config/batchWrite":
            return self.model_config.write(params), None
        if method == "config/read":
            return {"config": {"model": self.model_config.model(), "model_provider": PROVIDER,
                    "approval_policy": "never", "sandbox_mode": "danger-full-access", "web_search": "disabled",
                    "model_reasoning_effort": self.model_config.effort(),
                    "model_reasoning_summary": self.model_config.reasoning_summary(), "service_tier": None,
                    "projects": {str(self.settings.workspace): {"trust_level": "trusted"}},
                    "model_providers": {PROVIDER: {"name": "Claude app-server UI descriptor", "wire_api": "responses", "requires_openai_auth": False}}},
                    "origins": {}, "layers": []}, None
        if method in {"mcpServerStatus/list", "experimentalFeature/list"}:
            return {"data": [], "nextCursor": None}, None
        if method == "collaborationMode/list":
            return {"data": ([{"name": name, "mode": mode, "model": None}
                              for name, mode in [("Default", "default"), ("Plan", "plan")]]
                             if self.settings.backend == 'subscription' else [])}, None
        if method == "skills/list":
            return self.skills.list(params), None
        if method == "thread/start":
            self._validate_launch(params)
            instructions = "\n".join(text(params[k], k) for k in ("baseInstructions", "developerInstructions") if params.get(k))
            tid = self.store.new_thread(params.get('model') or self.model_config.model(), str(self.settings.workspace), instructions)
            if self.settings.backend == 'subscription':
                self.store.db.execute('INSERT INTO thread_efforts VALUES(?,?)', (tid, self.model_config.effort()))
                initial = requested_mode(params, 'bypassPermissions')
                if initial and initial != 'bypassPermissions':
                    # No runtime yet: the mode is applied when it connects.
                    self.store.db.execute('INSERT OR REPLACE INTO thread_tool_approvals VALUES(?,?)', (tid, initial))
            if params.get('collaborationMode'):
                await self.approvals.set_plan(tid, params['collaborationMode']['mode'] == 'plan')
            peer.subscriptions.add(tid)
            result = self._thread_response(tid)
            async def started():
                await self.engine.emit(tid, "thread/started", {"thread": self._thread_wire(tid)})
                await self._emit_settings(tid)
            return result, started
        if method == "claude/thread/ensure":
            require(set(params) <= {"threadId", "model", "cwd", "instructions"}, "unsupported ensure fields")
            self._validate_launch(params)
            tid = text(params.get("threadId"), "threadId", limit=256)
            instructions = params.get("instructions", "")
            if instructions != "":
                instructions = text(instructions, "instructions")
            ensure_thread = getattr(self.engine, 'ensure_launch_thread', self.store.ensure_thread)
            created = ensure_thread(tid, self.settings.model, str(self.settings.workspace), instructions)
            peer.subscriptions.add(tid)
            return {"threadId": tid, "created": created}, None
        if method == "thread/resume":
            self._validate_launch(params)
            require(params.get('collaborationMode') is None,
                    'Resume restores the saved mode; use thread/settings/update to change it')
            tid = text(params.get("threadId"), "threadId", limit=256)
            saved = self.store.thread(tid)
            if any(params.get(k) for k in ("baseInstructions", "developerInstructions")):
                requested = "\n".join(text(params[k], k) for k in ("baseInstructions", "developerInstructions") if params.get(k))
                require(requested == saved["instructions"], "resume cannot change saved instructions; create a new thread")
            peer.subscriptions.add(tid)
            async def after_resume():
                await self._replay_recovered(peer, tid)
                await self._emit_settings(tid)
            return self._thread_response(tid), after_resume
        if method in {"thread/turns/list", "thread/items/list"}:
            tid = text(params.get("threadId"), "threadId", limit=256)
            return self.history.page(tid, params, items=method == "thread/items/list"), None
        if method == "thread/read":
            subagents = getattr(self.engine, "subagents", None)
            if subagents is not None and isinstance(params.get("threadId"), str):
                # A foreground subagent's read-only transcript (subagent_transcripts.py).
                wire = subagents.thread_wire(params["threadId"], bool(params.get("includeTurns")))
                if wire is not None:
                    return {"thread": wire}, None
            tid = text(params.get("threadId"), "threadId", limit=256)
            return {"thread": self._thread_wire(tid, bool(params.get("includeTurns")))}, None
        if method == "thread/unsubscribe":
            tid = text(params.get("threadId"), "threadId", limit=256)
            self.store.thread(tid)
            status = "unsubscribed" if tid in peer.subscriptions else "notSubscribed"
            peer.subscriptions.discard(tid)
            return {"status": status}, None
        if method in {"claude/runtime/start", "claude/runtime/status", "claude/runtime/close"}:
            require(self.settings.backend == "subscription", "runtime control requires subscription backend", -32601)
            require(set(params) <= {"threadId"}, "unsupported runtime control fields")
            tid = text(params.get("threadId"), "threadId", limit=256)
            if method.endswith("/start"):
                result = await self.engine.start_runtime(tid)
            elif method.endswith("/close"):
                result = await self.engine.close_runtime(tid)
            else:
                result = self.engine.runtime_status(tid)
            peer.subscriptions.add(tid)
            return result, None
        if method == "thread/loaded/list":
            return {"data": sorted(set().union(*(p.subscriptions for p in self.peers)))}, None
        if method == "thread/list":
            limit = self._limit(params.get("limit"), 50)
            offset = self._cursor(params.get("cursor"))
            archived = bool(params.get("archived", False))
            rows = self.store.db.execute("SELECT id FROM threads WHERE archived=? ORDER BY updated DESC,id DESC LIMIT ? OFFSET ?", (archived, limit + 1, offset)).fetchall()
            return {"data": [self._thread_wire(r[0]) for r in rows[:limit]], "nextCursor": str(offset + limit) if len(rows) > limit else None}, None
        if method == "thread/name/set":
            tid = text(params.get("threadId"), "threadId", limit=256)
            self.store.thread(tid)
            name = text(params.get("name"), "name", limit=512)
            self.store.db.execute("UPDATE threads SET name=? WHERE id=?", (name, tid))
            return {}, None
        if method in {"thread/archive", "thread/unarchive"}:
            tid = text(params.get("threadId"), "threadId", limit=256)
            self.store.thread(tid)
            require(not self.store.pending(tid), "cannot archive/unarchive with pending work", -32009)
            self.store.db.execute("UPDATE threads SET archived=? WHERE id=?", (method.endswith("/archive"), tid))
            return ({"thread": self._thread_wire(tid)} if method.endswith("/unarchive") else {}), None
        if method == "thread/settings/update":
            require(self.settings.backend == "subscription", "model switching requires the Claude subscription backend", -32601)
            tid = text(params.get("threadId"), "threadId", limit=256)
            mode = params.get("collaborationMode")
            checked = dict(params)
            for field_name in ('effort', 'reasoningEffort', 'serviceTier'):
                checked.pop(field_name, None)
            self._validate_launch(checked)
            for name in ("permissions", "personality", "summary", "multiAgentMode"):
                require(params.get(name) is None, f"{name} override unsupported")
            model = params.get("model") or (mode or {}).get("settings", {}).get("model") or self.store.thread(tid)["model"]
            desired_effort = params.get('effort') or self._effort(tid)
            if 'effort' not in params and mode and 'reasoning_effort' in mode.get('settings', {}):
                desired_effort = mode['settings']['reasoning_effort'] or self._effort(tid)
            if desired_effort == 'none':
                desired_effort = None
            owner = getattr(getattr(self.engine, 'driver', None), 'owners', {}).get(tid)
            levels = getattr(owner, 'model_efforts', {}).get(model)
            if desired_effort is not None and levels is None:
                # Same levels model/list advertises: the live handshake when it names
                # this model, else the reference catalog (pinned IDs are not in the
                # handshake, and an offline thread has no handshake at all).
                entry = next((e for e in (await self.model_catalog.read(tid))['entries'] if e['id'] == model), None)
                levels = entry.get('supportedEffortLevels') if entry else None
            if desired_effort is not None:
                require(desired_effort in (levels if levels is not None else [self._effort(tid)]),
                        f'Claude model {model} does not offer effort {desired_effort}'
                        + (f" (offered: {', '.join(levels)})" if levels else ' (it offers no effort levels)' if levels is not None
                           else ' (its effort levels are unknown)'))
            # Fast mode = Codex service tier: the Fast tier on, `default` / null off.
            tier = params.get('serviceTier', 'unchanged')
            require(tier in ('unchanged', None, 'default', FAST_TIER), 'serviceTier must be priority (Fast), default or null')
            fast = None if tier == 'unchanged' else tier == FAST_TIER
            if fast:
                require(fast_mode_supported(model), 'Claude fast mode needs an Opus 4.8 or Opus 5 model')
            elif fast is None and self._fast(tid) and not fast_mode_supported(model):
                fast = False  # A model without fast mode turns it off.
            if fast is not None and fast == self._fast(tid):
                fast = None
            if model != self.store.thread(tid)['model'] or desired_effort != self._effort(tid) or fast is not None:
                # Permission mode and plan mode may change while a turn runs (Claude
                # Code's shift+tab); a model, effort or fast mode change needs an idle thread.
                require(not self.store.user_turn_busy(tid), "Finish or interrupt the current turn before changing Claude settings", -32009)
                if fast is None:
                    await self.engine.set_model_effort(tid, model, desired_effort)
                else:
                    await self.engine.set_model_effort(tid, model, desired_effort, fast)
            requested = requested_mode(params, self.approvals.mode(tid))
            if requested is not None and requested != self.approvals.mode(tid):
                await self.approvals.configure(tid, requested)
            if mode:
                await self.approvals.set_plan(tid, mode['mode'] == 'plan')
            return {}, lambda: self._emit_settings(tid)
        if method == "turn/start":
            self._validate_launch(params)
            tid = text(params.get("threadId"), "threadId", limit=256)
            require(tid not in getattr(self.engine, "offline_threads", set()), "runtime explicitly closed; call claude/runtime/start", -32009)
            self.store.check_user_continuation(tid)
            owner = getattr(getattr(self.engine, 'driver', None), 'owners', {}).get(tid)
            require(owner is None or not owner.task.done(),
                    "Claude runtime has exited. Restart this Agent runtime in Cutex, then submit your new instruction; the failed input will not be replayed.", -32009)
            if self.store.user_turn_busy(tid):
                # `data.reason` lets cutex keep the input in its own queue
                # (e.g. an Agent Bus message was queued just before it).
                raise RpcError(-32009, "thread busy; use claude/input/submit for a separate queued turn",
                               {"reason": "threadBusy", "runtime": "claude"})
            value, attachments, display = parse_turn_input(params.get("input"), images=self.settings.backend == "subscription",
                skills=self.skills, cwd=self.store.thread(tid)['cwd'])
            require(not params.get("toolOutput"), "arbitrary tool-output injection is unsupported")
            if self.settings.backend == "subscription" and params.get("model"):
                await self.engine.set_model(tid, params["model"])
                require(not self.store.user_turn_busy(tid), "thread became busy while changing model", -32009)
            if params.get('collaborationMode'):
                await self.approvals.set_plan(tid, params['collaborationMode']['mode'] == 'plan')
            payload = {"source": {"kind": "user", "id": "local-controller"}, "eventType": "user.message", "text": value, "metadata": {}}
            if attachments:
                payload["attachments"] = attachments
            if display:
                payload["display"] = display
            client_id = params.get("clientUserMessageId")
            if isinstance(client_id, str) and client_id.strip():
                # Echoed as userMessage.clientId: cutex deduplicates submits and
                # clients match pending messages by it.
                payload["clientId"] = client_id[:256]
            receipt = self.store.enqueue(tid, identifier(), payload, self.settings.max_pending, resume_interrupted=True)
            record = self.store.claim(tid, receipt["messageId"])
            assert record is not None
            peer.subscriptions.add(tid)
            return {"turn": self.store.turn(receipt["turnId"], include_items=False)}, lambda: self.engine.wake(tid)
        if method == "turn/steer":
            return self._steer(peer, params)
        if method == "claude/permissionRules/reload":
            require(self.settings.backend == "subscription", "Claude subscription backend required", -32601)
            require(set(params) <= {"threadId"}, "only threadId is accepted")
            tid = text(params.get("threadId"), "threadId", limit=256)
            self.store.thread(tid)
            return await self._reload_rules(tid), None
        if method == "thread/compact/start":
            if self.settings.backend != "subscription":
                raise unsupported(method)
            tid = text(params.get("threadId"), "threadId", limit=256)
            require(tid not in getattr(self.engine, "offline_threads", set()), "runtime explicitly closed; call claude/runtime/start", -32009)
            self.store.check_user_continuation(tid)
            owner = getattr(getattr(self.engine, 'driver', None), 'owners', {}).get(tid)
            require(owner is not None and not owner.task.done(), "Start the Claude runtime before compacting", -32009)
            require(not self.store.user_turn_busy(tid), "Finish or interrupt the current turn before compacting", -32009)
            payload = {"source": {"kind": "user", "id": "local-controller"}, "eventType": COMPACT_EVENT,
                       "text": "/compact", "metadata": {}}
            receipt = self.store.enqueue(tid, identifier(), payload, self.settings.max_pending, resume_interrupted=True)
            self.store.claim(tid, receipt["messageId"])
            peer.subscriptions.add(tid)
            return {}, lambda: self.engine.wake(tid)
        if method == "thread/backgroundTerminals/list":
            self.store.thread(text(params.get("threadId"), "threadId", limit=256))
            return {"data": [], "nextCursor": None}, None
        if method == "thread/goal/get":
            self.store.thread(text(params.get("threadId"), "threadId", limit=256))
            return {"goal": None}, None
        if method == "thread/goal/clear":
            self.store.thread(text(params.get("threadId"), "threadId", limit=256))
            return {"cleared": False}, None
        if method in UNSUPPORTED:
            raise unsupported(method)
        if method == "turn/interrupt":
            tid = text(params.get("threadId"), "threadId", limit=256)
            turn_id = text(params.get("turnId"), "turnId", limit=256)
            await self.engine.interrupt(tid, turn_id)
            return {}, None
        if method == "claude/input/submit":
            require(not (set(params) - {"threadId", "messageId", "source", "eventType", "text", "metadata", "expectedRuntimeId", "admission", "view"}), "unsupported input fields")
            admission = params.get("admission")
            require(admission in (None, asks.ADMISSION, 'next_step'), "unsupported admission")
            tid = text(params.get("threadId"), "threadId", limit=256)
            if params.get("expectedRuntimeId"):
                require(params["expectedRuntimeId"] == self.runtime_id, "stale runtimeId", -32011)
            mid = text(params.get("messageId"), "messageId", limit=256)
            source = params.get("source")
            require(isinstance(source, dict) and set(source) == {"kind", "id"}, "source must contain kind and id only")
            require(isinstance(source["kind"], str) and source["kind"] in {"agent", "service", "user"}, "unsupported source kind")
            text(source["id"], "source.id", limit=256)
            metadata = params.get("metadata", {})
            require(isinstance(metadata, dict), "metadata must be an object")
            require(len(dump(metadata).encode()) <= 8192, "metadata too large")
            payload = {"source": source, "eventType": text(params.get("eventType", "message"), "eventType", limit=256),
                       "text": text(params.get("text"), "text"), "metadata": metadata}
            asks.validate_metadata(payload["eventType"], metadata)
            soon = metadata.get('deliveryMode') == 'soon'
            if soon or admission == 'next_step':
                require(self.busy_delivery.enabled and soon and admission == 'next_step', 'unsupported next-step input')
                require(source == {'kind': 'service', 'id': 'cutex-asks'}
                        and payload['eventType'] in {'ANSWER', 'QUESTION'}
                        and mid.startswith('cutex-ask-soon-') and metadata.get('asks')
                        and metadata.get('attempt') is None, 'next-step admission requires a delegation result')
                if not self.store.input_known(tid, mid):
                    reason = asks.busy_reason(self, tid)
                    if reason and reason != 'active_turn':
                        raise asks.refuse_busy(reason)
                    # pending() hides other gates behind active_turn.
                    if self.approvals.planning(tid) or self.engine.closing:
                        raise asks.refuse_busy('plan_mode' if self.approvals.planning(tid) else 'server_draining')
            view = params.get("view")
            if view is not None:
                # cutex structured view (`externalInputView` of the timeline item).
                require(isinstance(view, dict) and set(view) == {"schema", "data"} and isinstance(view["data"], dict),
                        "view must be {schema, data}")
                text(view["schema"], "view.schema", limit=128)
                require(len(dump(view).encode()) <= 16384, "view too large")
                require(source["kind"] != "user", "views are for external inputs")
                payload["view"] = view
            if metadata.get("deliveryMode") == "passive":
                # A note: never starts a turn; read at the next turn start or
                # after the next tool call of the running turn (at most once).
                require(admission is None and source["kind"] != "user", "a passive note is external input without admission")
                receipt = self.store.enqueue_note(tid, mid, payload, self.settings.max_pending)
                peer.subscriptions.add(tid)
                return receipt, None
            if admission == asks.ADMISSION and not self.store.input_known(tid, mid):
                # Exclusive: start its own turn now or queue nothing. A known
                # message id falls through to enqueue (receipt replay, or the
                # conflict error) and never queues a second turn.
                reason = asks.busy_reason(self, tid)
                if reason:
                    raise asks.refuse_busy(reason)
            with self.store.transaction():
                receipt = self.store.enqueue(tid, mid, payload, self.settings.max_pending)
                if soon:
                    self.busy_delivery.register(tid, mid, receipt)
            receipt = self.busy_delivery.receipt(receipt)
            peer.subscriptions.add(tid)
            return receipt, (None if receipt["duplicate"] else lambda: self.engine.wake(tid))
        if method == "claude/input/status":
            return self.busy_delivery.receipt(self.store.input_status(text(params.get("threadId"), "threadId", limit=256), text(params.get("messageId"), "messageId", limit=256))), None
        if method == "claude/queue/resume":
            tid = text(params.get("threadId"), "threadId", limit=256)
            t = self.store.thread(tid)
            require(params.get("acknowledgeUncertainEffects") is True, "explicit acknowledgeUncertainEffects=true is required")
            require(not t["archived"], "thread is archived", -32009)
            self.store.db.execute("UPDATE threads SET halted=0 WHERE id=?", (tid,))
            # Uncertain/interrupted inputs are NEVER requeued; only untouched queued inputs run.
            return {"threadId": tid, "resumed": True}, lambda: self.engine.wake(tid)
        if method == "claude/events/read":
            tid = text(params.get("threadId"), "threadId", limit=256)
            after = self._cursor(params.get("afterSequence"))
            events = self.store.events(tid, after, self._limit(params.get("limit"), 100))
            return {"data": events, "lastSequence": events[-1]["sequence"] if events else after,
                    "containsStreamingDeltas": False}, None
        if method == "claude/thread/inspect":
            tid = text(params.get("threadId"), "threadId", limit=256)
            t = self.store.thread(tid)
            entries = self.store.db.execute("SELECT message_id,state,turn_id FROM inputs WHERE thread_id=? ORDER BY seq DESC LIMIT 128", (tid,)).fetchall()
            calls = self.store.db.execute("SELECT call_id,turn_id,state FROM tool_calls WHERE thread_id=? ORDER BY rowid DESC LIMIT 128", (tid,)).fetchall()
            usage = self.store.db.execute("SELECT id,usage FROM turns WHERE thread_id=? ORDER BY started DESC LIMIT 128", (tid,)).fetchall()
            return {"threadId": tid, "paused": bool(t["halted"]), "inputs": [dict(r) for r in entries], "toolCalls": [dict(r) for r in calls],
                    "usage": [{"turnId": r[0], **json.loads(r[1])} for r in usage], "limit": 128,
                    "runtime": self.engine.runtime_status(tid) if hasattr(self.engine, "runtime_status") else None,
                    "nativeSession": self.engine.inspect_session(tid) if hasattr(self.engine, "inspect_session") else None,
                    "localHistoryScope": "audit-projection" if self.settings.backend == "subscription" else "canonical-messages"}, None
        raise RpcError(-32601, f"Method not supported by the Claude adapter: {method}",
                       {"unsupported": True, "runtime": "claude", "feature": method, "method": method})

    def _steer(self, peer: Peer, params: dict) -> tuple[dict, Callable | None]:
        """Owner input into the running turn (Codex `turn/steer`).

        The input waits in the durable FIFO as a `soon` input: the next
        main-agent PostToolUse of that turn reads it (busy_delivery); a turn
        that completes first runs it as the next ordinary turn, a turn that
        ends interrupted or failed drops it (Store.drop_unread_steers, like
        cute-codex): the client resends it. Images cannot ride
        a hook's text context, so input with images is queued for the next
        turn at once."""
        if self.settings.backend != "subscription":
            raise unsupported("turn/steer")
        require(set(params) <= {"threadId", "input", "clientUserMessageId", "expectedTurnId"}, "unsupported steer fields")
        tid = text(params.get("threadId"), "threadId", limit=256)
        thread = self.store.thread(tid)
        row = self.store.db.execute("SELECT id FROM turns WHERE thread_id=? AND state='inProgress' ORDER BY rowid DESC LIMIT 1", (tid,)).fetchone()
        if row is None:
            raise RpcError(-32600, "no active turn to steer")
        active = row[0]
        expected = params.get("expectedTurnId")
        if expected is not None and expected != active:
            raise RpcError(-32600, f"expected active turn id {expected} but found {active}")
        require(not thread["archived"], "thread is archived", -32009)
        require(not thread["halted"], "queue paused; resume the agent before adding input", -32009)
        value, attachments, display = parse_turn_input(params.get("input"), images=True,
            skills=self.skills, cwd=thread["cwd"])
        # `steer` marks it for Store.drop_unread_steers (with or without images).
        payload = {"source": {"kind": "user", "id": "local-controller"}, "eventType": "user.message", "text": value,
                   "metadata": {"steer": True} if attachments else {"deliveryMode": "soon", "steer": True}}
        if attachments:
            payload["attachments"] = attachments
        if display:
            payload["display"] = display
        client_id = params.get("clientUserMessageId")
        mid = identifier()
        if isinstance(client_id, str) and client_id.strip():
            payload["clientId"] = client_id[:256]
            mid = "steer-" + client_id[:200]
        with self.store.transaction():
            receipt = self.store.enqueue(tid, mid, payload, self.settings.max_pending)
            if not attachments:
                self.busy_delivery.register(tid, mid, receipt)
        peer.subscriptions.add(tid)
        return {"turnId": active}, (None if receipt["duplicate"] else lambda: self.engine.wake(tid))

    async def _reload_rules(self, tid: str) -> dict:
        """cutex changed the persisted allow rules (permission-rules.json): new
        connections read the file; a live one gets them as flag settings when
        the runtime accepts that, else at its next start."""
        from . import permission_rules
        rules = permission_rules.load(self.settings.state_dir)
        driver = getattr(self.engine, "driver", None)
        owner = getattr(driver, "owners", {}).get(tid)
        applied = "nextStart"
        if owner is not None and not owner.task.done():
            if hasattr(self.engine, "flag_settings_dict"):
                settings = self.engine.flag_settings_dict(tid, explicit_rules=True)
            else:
                base = driver.api_account.cli_settings() if getattr(driver, "api_account", None) else {}
                settings = permission_rules.with_rules(dict(base or {}), rules, explicit=True)
            try:
                await owner.control("flag_settings", settings=settings)
                applied = "live"
            except Exception:
                applied = "nextStart"
        return {"threadId": tid, "rules": len(rules), "applied": applied}

    @staticmethod
    def _limit(value: object, default: int) -> int:
        if value is None:
            return default
        require(isinstance(value, int) and not isinstance(value, bool) and 1 <= value <= 200, "limit must be between 1 and 200")
        return value

    @staticmethod
    def _cursor(value: object) -> int:
        if value is None:
            return 0
        require(isinstance(value, (str, int)) and not isinstance(value, bool), "invalid cursor")
        try:
            result = int(value)
        except ValueError:
            raise RpcError(-32602, "invalid cursor") from None
        require(result >= 0, "cursor must be nonnegative")
        return result
