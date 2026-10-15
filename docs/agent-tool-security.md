# Agent tool security: identity, project boundary, roles, envelopes, approvals

Owner decisions of 2026-10-07 on the design review of the cutex agent-facing tools
(cutex-desktop `docs/research/cutex-tools-review.md`, findings S1–S4, C2, C4, C7, U1).
This document describes what is implemented on `feat/tool-security` and what is left.

## 1. Trust model in one paragraph

**Identity is always verified; the project is the distrust boundary; authority is
role-based inside a project; the owner is asked only when an action exceeds the caller's
own envelope or a profile the owner marked as needing approval.**

- Every agent runtime authenticates to the Agent Bus with a **per-agent credential** bound
  to its runtime id and durable session. There is no setting that turns authentication off.
  Debug builds have one escape hatch (`CUTEX_DEBUG_SHARED_AGENT_BUS_TOKEN=1`, below).
- Inside one project (Director, members, parents and children) agents cooperate freely:
  they list each other with status, message each other, see the project's services and
  memory, and need no human approval for that.
- Across projects agents are **isolated by default**: not listed, cannot message each
  other, cannot see each other's services. The owner can link two domains, grant a
  project specific external agents (directional, §4), or switch the host back to
  host-wide visibility (`agentSecurity`, §4). The switch controls isolation, never
  authentication.
- An agent outside every project (an independent agent, a quick chat) has its own creator
  tree as its boundary: itself, the agents it created, the agents those created, …
- Who may manage agents is still decided by roles (§5). Roles also decide which tools an
  agent is shown.
- A child agent never gets more than its creator (§6). The owner is asked only when an
  action exceeds the caller's envelope (managed services, §7) or uses a profile marked
  `agentUseRequiresApproval` (§7). A Director creating or closing members within its own
  envelope is not asked.

## 2. Code map

| Concern | Code |
|---|---|
| Per-agent credential, request authentication, agent route allowlist | `src/agent_bus/agent_auth.rs` (`agent_credential`, `launch_credential`, `authenticate`, `BusRequest`, `agent_route_allowed`) |
| Project boundary (domains, links, external agent grants, host mode) | `src/agent_bus/boundary.rs` (`AgentBoundary`), config `src/config/agent_security.rs` |
| Role → tool table | `src/agent_bus/tool_policy.rs` (`CUTEX_TOOL_RULES`, the only table) |
| Child ⊆ creator | `src/agent_management/envelope.rs` (`clamp_or_refuse`) |
| Owner approvals (store) | `src/approvals.rs` |
| Owner approvals (Management v2) | `src/management/v2/approvals.rs` |
| Bus wiring (auth wrapper, toolset route, list/send boundary, launch policy) | `src/agent_bus/server.rs` |
| Services boundary, redaction, managed-service gate | `src/services/agent_api.rs` (`AgentServicePolicy`) |
| Launch: credential, shell env blanking, native subagents | `src/cli_app/stock_lifecycle.rs`, `src/runtime/session_online.rs`, `src/runtime/claude.rs`, `src/agent_bus/launch.rs` |
| MCP adapter: role-filtered `tools/list`, error classes, old-name aliases | `src/agent_bus/mcp.rs`, `src/agent_bus/client.rs` |

New Agent Bus routes reuse the same pieces: `BusRequest::agent()` gives the verified
principal, `AgentBoundary::load()?` the boundary check (`may_see(caller, other)` for
listing and showing, `may_message(caller, other)` for addressing, `may_interact` for
mutual cooperation; see §4),
`clamp_or_refuse` the envelope check, `ApprovalStore::gate` the approval gate, and a new
tool gets one row in `CUTEX_TOOL_RULES`.

## 3. Per-agent credentials (S1)

### Format and verification

```
cxa1.<hex HMAC-SHA256(root, "cutex-agent-bus-credential-v1\0" | runtime_agent_id | "\0" | cutex_session_id)>.<cutex_session_id>
```

- `root` is the existing `agent_bus_token` in `config.json`. It never leaves the daemon
  any more (except the residual paths in §11).
- Verification is stateless: the bus recomputes the MAC from the presented
  `X-Cutex-Agent-Id` header and the session embedded in the credential (constant-time
  compare). A credential is therefore valid only together with the runtime id it was
  issued for; presenting it with another agent's id is rejected (401).
- The session is embedded so that the MCP adapter can ask for its toolset before the
  runtime is registered on the bus (MCP startup precedes registration).
- An agent credential is accepted only on the routes the cutex MCP adapter uses
  (`MCP_ROUTE_PATHS`, each with the MCP caller fence headers) and on the unfenced
  `POST /api/mcp/v1/toolset`. Everything else (registration, polling, acks, group
  updates, federation writes) stays root-only (403).
- On fenced MCP routes the bus additionally checks that the fenced durable session and
  runtime are the ones the credential was issued for
  (`MCP occurrence does not match the agent credential`). `X-Cutex-Agent-Id` alone is no
  longer trusted: it must match a valid credential (or come with the root token).
- The root token is still accepted everywhere, so runtimes launched before the upgrade
  keep working until they are relaunched (§10).

### Where agents get it

| Launch path | Before | Now |
|---|---|---|
| Stock (cute-codex app-server, `stock_lifecycle.rs`) | root token in the app-server env, inherited by shell tools | per-agent credential in the app-server env, passed to the `cutex` MCP server through `env_vars`; `-c shell_environment_policy.set.CUTEX_AGENT_BUS_TOKEN=""` blanks it for shell/exec tools (`set` is applied after include/exclude, so it holds even under `inherit = "all"`) |
| Fork app-server (`session_online.rs`) | root token in env | per-agent credential in env |
| Claude (`runtime/claude.rs`, `cutex-mcp-binding.json`) | root token in the binding file | per-agent credential in the binding file |
| Interactive `cutex <profile> --agent` (`agent_bus/launch.rs`) | root token in env | no token (nothing in the native process reads it; the shell fallback `cutex agent …` reads the cutex config) |

### Provider API keys (API-key profiles)

An API-key profile's provider key reaches native as `OPENAI_API_KEY` (every API
provider's `env_key`; native reads it from its own environment per request). Every Codex
launch that carries it (selected-profile `native_args`: stock agents, quick chats, the
foreground `cutex` CLI; legacy `profile_launch_command`) also passes
`-c shell_environment_policy.set.OPENAI_API_KEY=""`, so shell / exec tools see an empty
value and native's shell snapshots (`<native_home>/shell_snapshots/*.sh`, which drop every
`set` key from the captured exports) never store it. Before this, snapshots held
`declare -x OPENAI_API_KEY=…` and `env` in a tool printed the key. Runtimes launched before
the change keep the old environment until relaunched. Not covered: a same-user process
can still read the app-server's `/proc/<pid>/environ`, and Claude API accounts
(`ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` in the Claude Code process env) are
inherited by Claude Code's Bash tool.

Native r53 (0.162.1-r53, fork 6eec316d6) never passes the active provider's `env_key` to
model-reachable child processes, and an env-key provider whose variable is missing falls
back to the `OPENAI_API_KEY` in the launch-selected `--auth-file`. cutex still exports
the key and keeps the `-c` override for every native: **r52 and older agents need the
exported key** (no auth-file fallback) and the override, so neither may be dropped while
any agent can run on r52 (including after an r53 -> r52 rollback). Under r53 the variable
is absent in tools rather than empty (e2e 2026-10-14: `printenv OPENAI_API_KEY | wc -c`
= 0 on r53, 1 on r52). See `docs/native-r53.md`.

Debug builds only: `CUTEX_DEBUG_SHARED_AGENT_BUS_TOKEN=1` in the daemon environment makes
every launch path hand out the root token again (for debugging old adapters). Release
builds ignore it.

## 4. Project boundary (S4 and the coordinator refinement)

### Domains

`AgentBoundary` (`agent_bus/boundary.rs`) maps every durable session to a **domain**:

- `project:<id>` — the session's current project (member, or the project's Director);
- `agent:<cutex_session_id>` — otherwise, the root of its creator tree (walking
  `created_by_operator_session` / `created_by_director_session` up to the first creator
  that has a project, or the top).

Two agents may interact when they are the same agent, share a domain, or their domains are
linked by the owner. With `crossProjectAccess = "host"` every agent may interact with
every other (the pre-upgrade behaviour).

### External agent grants (owner decision 2026-10-07)

Projects stay isolated; the owner can give one project visibility of **specific** external
agents without linking whole domains: `externalAgents: [{project, agent}]`. A grant is
directional:

| | Project P's agents → granted agent X | X → P's agents |
|---|---|---|
| fenced agent list (`/api/agents`, alarm name resolution) | X is listed (row has `"external_grant": true`) | not listed |
| `/api/messages/send` (bus; the MCP `send` tool was removed 2026-10-07) | allowed | allowed (so X can answer) |
| `alarm_set` target | by name or id | by complete `cutex_session_id` only (names resolve through the list) |
| `service_list` | services **owned by X** (not X's project or domain) | nothing of P |

- The grant names the agent, not its creator tree or project: X's children, X's project
  and its other members stay invisible to P.
- Only agents currently in P benefit; a member that leaves P loses the grant.
- Because X cannot list the sender, a message from P to X carries a header line (r208, agent messaging v2):
  `Reply: this sender is not in your agent_list; answer with agent_tell to="<sender cutex_session_id>"`
  (before r208 it was a line appended to the text). Clients see `replyOnly: true`.
- Removing the grant (PATCH without it) cuts both directions on the next bus request.
  Alarms already set keep their target (as for links).
- A grant whose agent is already inside P is harmless (P sees it anyway).
- `host` mode ignores grants (everything is visible as before).

Code: `AgentBoundary::may_see(caller, other)` = `may_interact` or `external_grant(caller,
other)`; `may_message(caller, other)` = `may_see(caller, other)` or
`external_grant(other, caller)`; `may_interact` (same agent, same or linked domain) is
symmetric and unchanged. Call sites: the fenced agent list (`mcp_visible_agents`) and the
services policy use `may_see`; the bus send route uses `may_message`; MCP `alarm_set` accepts a
target in the caller's list, or by complete id when `may_message` holds
(`reply_alarm_allowed` in `agent_bus/mcp.rs`, fails closed if the boundary cannot be loaded).

### What the boundary filters

| Surface | Isolated (default) |
|---|---|
| fenced agent list (`/api/agents`) | only agents the caller may see (`may_see`; replaces group visibility for MCP callers) |
| `/api/messages/send` (fenced bus route) | recipient must be a durable session the caller may message (`may_message`); else HTTP 403 `recipient_outside_boundary` |
| `alarm_set` to another agent | target must be in the caller's (boundary-filtered) list, or a complete id the caller may message |
| `service_list` | own services, services owned by agents the caller may see, services of its own and linked projects, and the owner's unassigned host services. `command`, `cwd`, `envKeys` and `process` only for the caller's own services |

Not (yet) filtered: federation routes (`/api/federation/*`), Human/Management v2 views
(root only anyway).

### Settings: `GET|PATCH /v2/settings` → `agentSecurity`

```json
"agentSecurity": {
  "crossProjectAccess": "isolated",          // "isolated" | "host"
  "crossProjectAccessOptions": ["isolated", "host"],
  "crossProjectAccessDefault": "isolated",
  "links": [["project:alpha", "project:beta"], ["project:alpha", "agent:cutex.<uuid>"]],
  "maxLinks": 256,
  "externalAgents": [{"project": "alpha", "agent": "cutex.<uuid>"}],
  "maxExternalAgents": 256
}
```

PATCH accepts `{"agentSecurity": {"crossProjectAccess"?: "...", "links"?: [[a, b], ...],
"externalAgents"?: [{"project": "<project id>", "agent": "<cutex_session_id>"}, ...]}}`;
omitted fields keep their stored value; `links` and `externalAgents` each replace the whole
list (`[]` revokes all). Rejected: unknown fields, self links, duplicate links (either
order), malformed keys; for grants also non-object entries, unknown entry fields, a
malformed project id or agent id (`cutex.<uuid>`), duplicate `(project, agent)` pairs, more
than 256 entries. Grants are not checked against the store (a grant for an unknown project
or agent simply matches nothing). Applies to the next bus request (no restart, not in
`newRuntimesOnlyFields`). Stored in `config.json` as
`agent_security: {cross_project, links, external_agents: [{project, agent}]}`, each
omitted while default.

## 5. Role → tool table (decision 2)

`src/agent_bus/tool_policy.rs::CUTEX_TOOL_RULES` is the single table. Every tool the MCP
adapter defines must have exactly one row (unit test).

| Tool | Audience |
|---|---|
| asks tools (`agent_list`, … see the table in code) | everyone |
| `alarm_set`, `alarm_list`, `alarm_cancel` | everyone |
| `service_register`, `service_update`, `service_unregister`, `service_list` | everyone |
| `web_search` | everyone whose profile has `searchTool` on (Codex API-key profiles; also the owner's quick chats), see `docs/PROFILE-SEARCH-TOOL.md` |
| `agent_create`, `agent_archive`, `member_runtime`, `member_replace` | project managers: the project's Director or an Agent Operator grant |

`director_rotate` (Directors only) was removed in r219 together with the Director seat copy and the
legacy `cutex-director` / `cutex-release` seats (see `docs/r219-removals.md`): the Director role comes
only from the Agent Management project authority (`authorized_director_session`), and a
`director_rotate` call answers `tool_removed`.

Tool rename 2026-10-07 (`docs/research/cutex-tool-naming.md` in cutex-desktop): `send`,
`query_managed`, `cutex_agent_list` and all `cutex_task_service*` tools were removed;
`cutex_agent_management` was split into the management tools above; `set_alarm` etc. and
`cutex_service_*` were renamed. For one release `tools/call` still accepts the old names
(`legacy::TOOLS` in `src/agent_bus/mcp.rs`): renamed tools run the new tool, removed ones answer
`no_write` / `tool_removed` naming the replacement (`director_rotate` and the `cutex_task_service*`
tools have none). Old names are never listed.

- The MCP adapter asks `POST /api/mcp/v1/toolset` at `tools/list`
  (`{ok, cutex_session_id, roles: {director, operator}, tools: [names]}`) and
  lists only those tools. If the bus cannot answer (older daemon, bus down) it lists every
  tool except the opt-in browser tools and `web_search`: listing is not authority, every
  call is still authorised by the bus and its providers.
- Claude runtimes get the same filtered list in `cutex-mcp-tools.json` at adapter start.
- cute-codex caches `tools/list` at startup, so roles are evaluated at launch. After an
  operator grant or a Director change, restart the agent to refresh its tools.

## 6. Child ⊆ creator (decision 3)

`agent_management::envelope::clamp_or_refuse(creator, requested)` is the one check for
every agent-initiated launch: Agent Management `create`, `replace`
today, `agent_start` later. The creator's envelope comes from its durable session
(`PermissionEnvelope::from_record`: effective sandbox and approval policy, network access,
launch cwd plus `sandbox_writable_roots`). The child must satisfy:

- sandbox no more permissive: `read-only` < `workspace-write` < `danger-full-access`
  (the spec's `permissions` alias and `sandbox_mode` are both checked);
- approval policy no less strict: `never` < `on-request` (`on-failure`) < `untrusted`;
- network access only if the creator has it (specs cannot request it today);
- for a `workspace-write` child of a non-full-access creator: its cwd and writable roots
  inside the creator's writable roots (lexical, case-insensitive on Windows).

Violations are never silently narrowed: the call answers `no_write` with
code `permission_exceeds_creator` and the offending fields
(`[{field, requested, limit}]`). A creator without explicit permissions answers
`creator_permissions_unknown`. Exact replays of an already admitted `action_id` and
Human-reviewed `bootstrap_intent` launches keep their admission.

## 7. Owner approvals

Server-side, because cute-codex auto-denies MCP prompts and elicitations under approval
policy `never`, and because a gate must hold whatever adapter made the call.

| Kind | When | Reason code |
|---|---|---|
| `managed_service` | `service_register` with `runtime: managed`, or `service_update` changing runtime/command/cwd/env of a managed service, by an agent that does not itself run with `danger-full-access` + approval `never` (S3: cutex runs managed commands unsandboxed as the owner). External services and identical re-registers are not gated. | `outside_caller_envelope` |
| `agent_profile` | `agent_create` / `member_replace` with a profile marked `agentUseRequiresApproval` | `profile_requires_approval` |

Flow: the first call records a pending approval and answers
`{ok:false, status:"approval_required", code:"approval_required", approval_id, detail}`.
The owner decides in a client. The agent retries the **same** call (same arguments, same
`action_id`); an approved record is consumed once and the action proceeds. A denial answers
`{status:"no_write", code:"approval_denied"}`. Pending and approved records lapse after
24 h; decided records are pruned after 7 days (max 500). The request is bound by a SHA-256
digest of its canonical JSON; env values are part of the digest but only env keys are
stored. Store: `<cutex runtime dir>/approvals/approvals.json` (private, file-locked).
Fail closed: without a readable store the call answers `approvals_unavailable` /
`persistence_unavailable`.

Management v2 (root credential only):

- `GET /v2/approvals?state=pending|all` (default `pending`) →
  `{hostId, approvals: [ApprovalRecord]}`, newest first.
- `POST /v2/approvals/{approvalId}/approve` and `/deny`, body `{note?}` (≤ 1000 chars) →
  `{approval}`; 404 `approval_not_found`; 409 `approval_not_pending` (`details.state`).
- Event `cutex/approvals/changed` `{reason: "requested"|"approved"|"denied", approval}` on
  the host event stream (`cutexSessionId` = the services host event session id).

`ApprovalRecord` (camelCase): `approvalId`, `kind`, `reason`, `requesterCutexSessionId`,
`requesterName?`, `projectId?`, `summary`, `details`, `requestSha256`,
`state: pending|approved|denied|consumed|expired`, `createdAt`, `decidedAt?`, `note?`,
`consumedAt?`.

The agent is not notified of the decision (the stock bridge does not deliver
owner-originated bus messages); the tool text tells it to end its turn or set an alarm and
retry. A push notification is a follow-up of the ask primitive.

## 8. Native subagents per profile (decision 4)

Profile option `nativeSubagents` (Codex profiles only):

- `/v2/profiles` items: `nativeSubagents: bool | null` (effective; `null` for Claude),
  `agentUseRequiresApproval: bool`.
- Detail: `detail.nativeSubagentsExplicit: bool | null`,
  `detail.nativeSubagentsDefault: true | null`, `detail.editable.nativeSubagents`,
  `detail.editable.agentUseRequiresApproval`.
- PATCH `{nativeSubagents: true|false|null}` (`null` restores the default, enabled) and
  `{agentUseRequiresApproval: bool}`. `nativeSubagents` on a Claude profile is refused.
- Launch (stock): explicit `false` → `-c features.multi_agent=false -c
  features.multi_agent_v2=false`; explicit `true` → `-c features.multi_agent=true`; unset
  → nothing (cute-codex 0.160 default: enabled). Takes effect at the next launch.

Tool text (`agent_create`) now says when to use which: cutex agents for
long-lived work, agents the owner should see and talk to, or another provider/model/
profile; native subagents for short parallel subtasks inside the agent's own task or for
small projects.

## 9. Tool text and result shapes (C2, C4, C7)

- `send` (removed 2026-10-07; use `agent_delegate` / `agent_tell`) had `delivery_mode` `after_turn | soon | passive`;
  `interrupt` is no longer offered and answers `unsupported_delivery_mode`. The description
  says replies arrive as a new turn (`[message from <name>]`) and not to poll.
- `agent_tell {to, text, wake?}` (r208): a note by default (`passive`, starts no turn). `wake: true` sends
  `after_turn` and is allowed only where `agent_delegate` to that target would run without owner review
  (target visible in `agent_list`, agent-enabled, delegation gate not required); otherwise `wake_not_allowed`
  and nothing is sent. Loop guard: a wake back to the agent whose wake started the caller's current turn
  continues that chain (`wakeChain` + 1); a chain above 3 is sent as a note (`wakeDowngraded: true`). The
  result is `{ok, to, name, deliveryKind, recipientState, wakeDowngraded?, detail}`; `detail` says what
  will happen given the recipient's state (busy / idle / offline) at send time.
- `agent_create {name, task?, task_mode?, ...}` / `member_replace {..., task?, task_mode?}`
  (capability `agent-create-task-delegation`, 2026-10-15): `task` is delegated from the caller
  to the new agent. The agent starts idle (no start message); the Agent Bus then runs
  `agent_delegate {target: <new agent>, text: task}` for the caller (`asks/create_task.rs`),
  so the delegation gate, project boundary, questions, progress, `delegation_*` tools and the
  automatic result are exactly those of `agent_delegate`. Its action id is derived from the
  create's (`agent-create-task:<sha256 prefix>`): an exact create retry finds the same
  delegation, or makes it if the first call could not. The result's `result` gains
  `task_mode: "delegation"` and `delegation` (the `agent_delegate` reply: `delegation`,
  `state`, `created`, `detail`, ...; the create waits up to 8 s for it to leave the queue), or
  `{ok: false, code, detail, note}` when the agent exists but the task was not delegated
  (nothing was sent to it; retry the create or use `agent_delegate`). `task_mode: "message"`
  keeps the plain start message and its `first_message` delivery state. A Human-reviewed
  `bootstrap_intent` keeps its reviewed start message. Only the sparse MCP arguments do this:
  the legacy full-spec arguments (`start_mode`, `frozen_message`) and `POST /v2/human/agents`
  (no task) are unchanged.
- Every non-Management tool failure is a tool result, not a JSON-RPC error:

  | status | code | meaning |
  |---|---|---|
  | `no_write` | `invalid_arguments` | nothing was sent; fix the arguments (bounded validation message) |
  | `no_write` | `bus_unavailable` | connection refused; nothing was sent; retry later |
  | `no_write` | bus code (e.g. `recipient_outside_boundary`) | the bus refused with 4xx; nothing was done |
  | `response_uncertain` | `outcome_unknown` | sent, answer lost or 5xx/408; retry with the same id (deduplicated) or check first |

  Removed tools answer `no_write` / `tool_removed` naming the replacement. Transport
  diagnostics and 5xx bodies are never shown. Management
  results keep their own receipt shape.
- C7: the agent list description no longer talks about a "prototype" or "A4"; the
  `CUTEX_AGENT_HINT` text names the real tools (`agent_list`, `agent_delegate`, `agent_tell`).
- Services tool texts describe the boundary, the redaction and the managed-service
  approval.

## 10. Capabilities, migration and deploy

`GET /v2/capabilities` adds: `agent-bus-per-agent-auth`, `agent-project-boundary`,
`agent-tool-roles`, `owner-approvals`, `profile-native-subagents`, `profile-agent-approval`,
`agent-external-grants` (`agentSecurity.externalAgents`).

Migration:

1. Deploy the daemon (and the `cutex-mcp` facade). Runtimes launched earlier keep the
   root token in their environment and keep working (the bus still accepts it).
2. The MCP adapter of a stock runtime is the facade pinned in the reviewed bundle
   manifest (path + sha256): role-filtered `tools/list`, the new `send` enum and the new
   error shapes reach stock agents only after the bundle/package is upgraded. The
   server-side checks (credential binding, boundary, child ⊆ creator, approvals, services
   policy) apply to old adapters immediately.
3. Relaunch all agents (they get per-agent credentials), then rotate `agent_bus_token`
   (daemon restart) to revoke the root token copies that leaked into old runtimes' shells.
4. **Behaviour change:** isolation is the default. Independent agents that message each
   other today, or agents in different projects, stop seeing each other. Put them in one
   project, link their domains, or set `crossProjectAccess = "host"`.
5. **Behaviour change:** a `workspace-write` Director can no longer create a
   `workspace-write` member whose cwd is outside its own writable roots, nor any member more
   permissive than itself.
6. Rollback hazard: profile options are parsed with `deny_unknown_fields`. A profile that
   has `native_subagents` or `agent_use_requires_approval` set makes an older cutex refuse
   the accounts store. Clear both (PATCH `null` / `false`) before rolling back. The same
   applies to the Claude profile option `agent_turn_questions` (`agentTurnQuestions`, see
   `claude-adapter/docs/DESKTOP_PARITY.md`).
   `agent_security` in `config.json` is safe: `CodezConfig` has no `deny_unknown_fields`,
   so an older cutex ignores it (and drops it on its next config save). The same holds for
  `agent_security.external_agents` on a cutex that knows `agent_security` but not grants
  (`AgentSecurityConfig` tolerates unknown fields): the grants are ignored, i.e. revoked.
7. Host events: `cutex/approvals/changed` uses the host event session `cutex.host`.

## 11. Remaining risks

- `config.json` holds the root token and is readable by any same-user process an agent can
  start outside a sandbox (`danger-full-access` agents, and every agent on Linux/Windows
  whose sandbox allows reading the cutex home). Per-agent credentials remove the token
  from the environment, not from disk. A real fix is a separate OS user or a
  root-token-less agent channel.
- The Agent Bus is started with `--token <root>` on its command line
  (`cli_app/agent_bus_runtime.rs`): visible to same-user process listings.
- `/api/federation/*` routes are unauthenticated; a local process can still spoof a
  federated sender.
- A Claude agent can read its own `cutex-mcp-binding.json` (its own credential only).
- Credentials are stateless and cannot be revoked one by one; they are bound to the
  runtime id (new per launch for stock), and the fence rejects stale runtimes. Rotating the
  root token revokes all.
- The Linux Job MCP adapter receives the per-agent credential (it is in its `env_vars`);
  if it calls bus routes beyond the MCP set, those calls now get 403.
- Managed services approved by the owner still run unsandboxed as the owner (by design);
  cwd is not confined to the caller's workspace — the owner sees it in the request.
- Roles are evaluated at launch for tool listing; enforcement is per call.

## 12. Deferred (owner is designing the ask primitive)

These stay proposals; `agent_ask` / `agent_wait` / `agent_start` / `agent_tell` will
replace `send` (the Task Service tools were removed in r219, see `docs/r219-removals.md`):

- U4 (naming): done 2026-10-07 — `<object>_<verb>` without the `cutex_` prefix
  (see §5 and cutex-desktop `docs/research/cutex-tool-naming.md`), old names
  as aliases for one release.
- C5 (alarms): move alarms behind a bus route (no MCP-side file writer, no file lock held
  during delivery), list only pending + recent, RFC 3339 or `in_seconds` only, show
  `set by <name>` to the recipient.
- S2 per-tool `approval_mode` in the launch config is superseded by the server-side gate
  above; the remaining `prompt` candidates (e.g. `close`) are owner policy questions.
- New tools get one row in `CUTEX_TOOL_RULES`, call `clamp_or_refuse` when they launch
  agents, `may_message` when they address agents (`may_see` when they show them), and
  `ApprovalStore::gate` when they exceed the caller's envelope.
