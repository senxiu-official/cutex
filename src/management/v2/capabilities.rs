//! `GET /v2/capabilities`: version and feature discovery for clients.

use serde_json::json;
use serde_json::Value;

/// Additive Management features beyond the frozen v2 baseline. Clients
/// feature-detect on these strings (and the boolean `features` map).
pub const MANAGEMENT_FEATURES: &[(&str, &str)] = &[
    // GET /v2/sessions?includeHidden=true and hidden exact-ID resolution (root).
    ("include-hidden", "includeHidden"),
    // cutex/session/metadata/update + note/avatar/displayNameOverride.
    ("session-metadata", "sessionMetadata"),
    // Project workspace_root (set_workspace_root mutation, summaries).
    ("project-workspace-root", "projectWorkspaceRoot"),
    // POST /v2/human/agents.
    ("human-create-agent", "humanCreateAgent"),
    // POST /v2/human/sessions (quick chats: owned sessions, scratch cwd) and the
    // optional `launch` overrides object on POST /v2/human/{agents,sessions}.
    ("human-create-session", "humanCreateSession"),
    // /v2/settings newThreadDefaults {agent, quickChat}: sandbox, approvalPolicy,
    // networkAccess, writableRoots for newly created threads.
    ("new-thread-defaults", "newThreadDefaults"),
    // GET /v2/profiles/{profile}/quota, GET|PUT /v2/profiles/{profile}/quota-config.
    ("profile-quota", "profileQuota"),
    // GET /v2/profiles, GET|PATCH /v2/profiles/{profile}.
    ("profiles-api", "profilesApi"),
    // GET|PATCH /v2/settings.
    ("settings-api", "settingsApi"),
    // GET|PUT /v2/user-instructions.
    ("user-instructions", "userInstructions"),
    // cutex/session/profile/switch + allowSessionSwitch profile option.
    ("profile-switch", "profileSwitch"),
    // cutex/session/history/revert (live revert, no runtime restart).
    ("session-history-revert", "sessionHistoryRevert"),
    ("session-files-revert", "sessionFilesRevert"),
    // GET /v2/profiles/{p}/models, POST .../models/discover, PATCH|DELETE .../models/{id}, POST .../models/{id}/reset.
    ("profile-models", "profileModels"),
    // requestModel bindings and official catalog edits, gated by each profile's
    // modelVariantsSupported (the selected verified native server capability).
    ("profile-model-variants", "profileModelVariants"),
    // Per-entry model display names: /v2/profiles items `modelDisplayNames`
    // {modelId: name}, PATCH `modelDisplayNames` {modelId: name | null} (merge;
    // null removes), GET .../models `models[].entryDisplayName`. Display only.
    ("profile-model-display-names", "profileModelDisplayNames"),
    // Official model lists follow upstream; edits are a per-profile overlay.
    // GET .../models `followsUpstream`, `catalogOverlay {entries, migratedFrom}`,
    // `unavailableModels [{id, requestModel, displayName, entryDisplayName,
    // reason: baseModelMissing|modelMissing}]`, `modelsSource.kind = overlay`
    // with `modelsSource.upstream`; DELETE removes an unavailable entry.
    ("profile-catalog-overlay", "profileCatalogOverlay"),
    // /v2/projects/{id}/memory[...] (root) + launch `# Memory` / `# File references` injection.
    ("project-memory", "projectMemory"),
    // GET /v2/usage, GET|PUT /v2/usage/prices, GET /v2/usage/quota-history.
    ("usage-ledger", "usageLedger"),
    // GET /v2/{projects,sessions}/{id}/fs/{dir,file,search,watch}.
    ("workspace-fs", "workspaceFs"),
    // GET /v2/{projects,sessions}/{id}/git/{status,diff}.
    ("workspace-git", "workspaceGit"),
    // POST /v2/{projects,sessions}/{id}/terminals, /v2/terminals/*.
    ("terminal", "terminal"),
    // POST /v2/codex/requests (root): allowlisted global codex config methods.
    ("codex-config", "codexConfig"),
    // POST /v2/codex/requests also allows plugin/{list,read,search,install,uninstall}
    // and marketplace/{add,remove,upgrade} on the shared home's local catalog
    // (remote catalog off).
    ("codex-plugins", "codexPlugins"),
    // Settings `codexConfigFollowShared` (default true; old name
    // `codexPluginsFollowShared` still accepted): Codex agents with their own
    // native home launch with the shared home's [plugins], [mcp_servers],
    // skills.config and hooks.state layered over their own config (docs:
    // launch/shared_config.rs).
    ("codex-config-overlay", "codexConfigOverlay"),
    // Scoped MCP: /v2/codex/managed-mcp, /v2/projects/{id}/mcp, /v2/sessions/{id}/mcp[/effective].
    ("scoped-mcp", "scopedMcp"),
    // Read-only native thread/read, thread/turns/list, thread/items/list and
    // thread/backgroundTerminals/list on verified Codex sub-agent threads of the
    // bound thread; their live events share the parent session stream.
    ("subagent-threads", "subagentThreads"),
    // GET /v2/sessions/{id}/history?limit=&cursor= (root): stored thread history while offline.
    ("offline-history", "offlineHistory"),
    // cutex/userInput/submit {startIfOffline: true}: queue + start the runtime when offline.
    ("offline-submit", "offlineSubmit"),
    // GET /v2/profiles/{p}/account, POST /v2/profiles/{p}/login, GET .../login/{id},
    // POST .../login/{id}/cancel, POST /v2/profiles/{p}/logout (root, Codex profiles).
    ("profile-login", "profileLogin"),
    // /v2/services[...] (root): services registry, supervision, logs, open;
    // cutex/services/changed host events; service_* agent MCP tools.
    ("services", "services"),
    // Simplified services: no `pinned`, no `onDemand` runtime / idleTimeoutMinutes;
    // `open` starts a stopped managed service; PATCH ownerCutexSessionId reassigns
    // (validated against cutex sessions); GET /v2/services?owner=.
    ("services-v2", "servicesV2"),
    ("services-proxy", "servicesProxy"),
    ("services-lifetime", "servicesLifetime"),
    // Services setting `agentPersistentApproval` (ask | off, default ask):
    // whether an agent's persistent-lifetime request raises a service_lifetime
    // approval; managed execution keeps its own managed_service gate.
    ("services-persistent-approval", "servicesPersistentApproval"),
    // Owner-only managed `loginLinkCommand` (a view ticket runs it and sends the
    // browser to the printed one-time loopback URL; response `login`), and
    // `proxyHostHeader: upstream` also moves the proxy host's own Origin to
    // `http://127.0.0.1:<port>` (services whose CSRF / WebSocket gate admits only
    // their loopback origin).
    ("services-login-link", "servicesLoginLink"),
    // POST /v2/services/{slug}/lease accepts `clientLeaseId` (1–128 printable
    // characters): a repeated open with the same key from the same credential
    // renews and returns the live lease it created (no orphan lease from a
    // retried request); the key expires with the lease. No key = a new lease.
    ("services-lease-idempotent", "servicesLeaseIdempotent"),
    // GET /v2/profiles/{p}/models also lists models for profiles without an
    // editable catalog (ChatGPT / official Codex: the native models cache or
    // the native's bundled catalog; Claude: model aliases), with
    // `modelsSource {kind: catalog|nativeCache|nativeBundled|claudeAliases}`
    // and `modelsUnavailableReason`.
    ("profile-models-native", "profileModelsNative"),
    // POST /v2/human/{agents,sessions} `launch.model` / `launch.reasoningEffort`
    // are part of the profile selection: a profile that configures no model
    // can be created from when the request names one.
    ("launch-model-selection", "launchModelSelection"),
    // cutex/turn/stop (interrupt + durable queue pause, no auto-flush while
    // paused), cutex/userInput/queue/{pause,resume}, queue/list `paused`/`pause`,
    // bootstrap `userInputQueuePause`, cutex/userInput/queue{Paused,Resumed},
    // queue items `bypassPause` (management/v2/queue_pause.rs).
    ("queue-pause", "queuePause"),
    // cutex/userInput/submit strategy "queue" on an idle agent (connected,
    // no turn running) starts the turn at once (disposition "started")
    // unless the queue is paused or an ask goes first; older waiting items
    // are flushed first. A result with `queue` carries `paused` (held by a
    // queue pause).
    ("queue-idle-start", "queueIdleStart"),
    ("queue-reorder", "queueReorder"),
    // One durable admission order for owner input, asks and turn-starting deliveries.
    ("admission-order", "admissionOrder"),
    // cutex/turn/stop also holds asks and every ride-along (answers, bus
    // wakes / tells, alarms) until queue resume, asks resume or an owner
    // message that starts / steers a turn: asksPaused `origin: "ownerStop"`,
    // `stopHold`; admission `ridealongsHeld` and nextStep `held`.
    ("stop-holds-ridealongs", "stopHoldsRidealongs"),
    // cutex/userInput/queue/yield: the owner waives the next owner-first
    // opportunity after a ride-along-started turn once; admission
    // `ownerYielded` while armed (in memory, lost on restart).
    ("admission-owner-yield", "admissionOwnerYield"),
    // Admission nextStep rows carry display detail: `from` (sender name;
    // `name@host` + `fromHost` for a cross-host tell; null for an alarm),
    // `preview` (first non-empty line, <= 80 chars) and `askId` for a Job
    // completion bound to a delegation; `label` is the preview (alarm /
    // tell) or a short control-type description (bus) instead of the old
    // generic "Alarm" / "Agent message" / "Bus delivery".
    ("admission-ridealong-detail", "admissionRidealongDetail"),
    ("queue-steer", "queueSteer"),
    // GET /v2/profiles/{p}/quota reports native rate-limit gauges
    // (`codex-primary`, `codex-secondary`, `codex-credits`, `source:"native"`)
    // for ChatGPT-login profiles without a quota-config, from a persisted
    // snapshot refreshed via a short-lived `account/rateLimits/read` and live
    // `account/rateLimits/updated`; `nativeQuota` metadata; host event
    // `cutex/profile/quota/updated`; settings `profileQuotaRefreshMinutes`.
    ("profile-quota-native", "profileQuotaNative"),
    // Claude profiles: /v2/profiles/{p}/quota serves the subscription usage
    // (source "claude", gauges claude-<n>), refreshed every
    // claudeUsageRefreshMinutes and pushed as cutex/profile/quota/updated.
    ("profile-quota-claude", "profileQuotaClaude"),
    ("auto-thread-titles", "autoThreadTitles"),
    // Delegation tools (agent_delegate / delegation_* / question_answer) and
    // the profile delegationWait option in /v2/profiles.
    ("delegation-tools", "delegationTools"),
    // GET /v2/profiles/{p}/models `modelsSource.native {version, cliSha256}` and
    // `modelsSource.notInBundled` for `nativeCache` lists.
    ("profile-models-native-bundle", "profileModelsNativeBundle"),
    // cutex/session/delete {expectedRevision?} (root): permanent delete of the
    // record + native thread with a recovery copy under runtime/session-deletes/;
    // event cutex/session/deleted; session_referenced / session_busy refusals.
    ("session-delete", "sessionDelete"),
    // /v2/settings archiveDeleteEmpty (default true): cutex/session/retire of a
    // session that never received input deletes it and answers {deleted: true}.
    ("session-delete-empty-on-archive", "sessionDeleteEmptyOnArchive"),
    // cutex/session/fork {afterTurnId?, name?} (root): new agent bound to a
    // forked native thread; result carries the new session; event
    // cutex/session/forked on the source.
    ("session-fork", "sessionFork"),
    // Independent owner notes and transient Codex Side conversations.
    ("notes", "notes"),
    ("side-chat", "sideChat"),
    // POST /v2/profiles {name, kind?: chatgpt|claude|apiKey (default apiKey),
    // preset?, baseUrl?, model?, apiKey?} (201, ProfileDetail) and
    // GET /v2/profile-presets (API-key provider presets).
    ("profile-create", "profileCreate"),
    // POST /v2/profiles/{p}/duplicate {name}: copies settings, never credentials.
    ("profile-duplicate", "profileDuplicate"),
    // DELETE /v2/profiles/{p}: 409 profile_in_use {sessions} / profile_is_default;
    // files moved to <config dir>/deleted-profiles/<ts>-<id>/.
    ("profile-delete", "profileDelete"),
    // PATCH /v2/profiles/{p} {name}: rename + every durable reference
    // (detail.editable.name = true); 409 profile_name_conflict.
    ("profile-rename", "profileRename"),
    // Rename works while agents launched on the profile run (no more 409
    // profile_running): runtimes relate to the profile by id and report under
    // the new name. No alias: the old name stops resolving; unknown names
    // answer profile_not_found with details.availableProfiles. /v2/usage rows
    // grouped by profile carry key.profileId and profileState current|deleted.
    ("profile-rename-live", "profileRenameLive"),
    // Profile items carry providerFamily / providerFamilyLabel and
    // cutex/session/profile/switch refuses cross-family switches with
    // provider_family_mismatch.
    ("profile-switch-family", "profileSwitchFamily"),
    // /v2/settings quickChatDefaultProfile (+ read-only quickChatEffectiveProfile),
    // used by POST /v2/human/sessions without a profile before defaultProfile.
    ("quick-chat-default-profile", "quickChatDefaultProfile"),
    // Agents launched by this daemon get a per-agent Agent Bus credential
    // (never the shared root token) and shell tools do not inherit it.
    ("agent-bus-per-agent-auth", "agentBusPerAgentAuth"),
    // /v2/settings agentSecurity {crossProjectAccess, links}: agents are
    // isolated per project unless the owner links domains or picks host.
    ("agent-project-boundary", "agentProjectBoundary"),
    // /v2/settings agentSecurity.externalAgents [{project, agent}] (+
    // maxExternalAgents): a project sees specific external agents; those
    // agents may only answer (directional).
    ("agent-external-grants", "agentExternalGrants"),
    // cutex MCP tools are filtered by role (docs/agent-tool-security.md).
    ("agent-tool-roles", "agentToolRoles"),
    ("agent-short-ids", "agentShortIds"),
    // GET /v2/approvals, POST /v2/approvals/{id}/approve|deny and the
    // cutex/approvals/changed host event.
    ("owner-approvals", "ownerApprovals"),
    // 2026-10-17: Management serve watches the approval store, so approvals
    // requested by the Agent Bus (services, agent profiles) and consumed by a
    // retried call reach the host stream live as cutex/approvals/changed
    // (reasons requested | approved | denied | consumed | expired).
    ("approvals-live", "approvalsLive"),
    // 2026-10-17: after the owner approves / denies a request the agent must
    // retry itself (managed_service, service_lifetime, agent_profile,
    // browser_open), the requester gets a short cutex message (alarm
    // scheduler, owner "cutex.approvals", after_turn) saying to retry now /
    // not to retry. The tool still answers approval_required until approved.
    ("approval-decision-notice", "approvalDecisionNotice"),
    // 2026-10-17: a gated service / agent launch holds the agent's tool call
    // up to 120 s for the owner (opted-in façades: Codex launches, Claude
    // adapters declaring features.approvalWait). Approve → the call proceeds
    // (no retry, no notice); deny → denied; timeout → approval_required as
    // before. While a call holds, the record carries `wait {until, end}` and
    // cutex/approvals/changed reports `waiting` / `wait_ended`.
    ("approval-wait", "approvalWait"),
    // 2026-10-18: a decided approval records `decidedBy {by: owner,
    // connection, peer?, client?}`; POST approve|deny accepts `client {app,
    // version, device, surface, input}` (unverified client facts) and logs a
    // `cutex-approval-decided` line (r243 acceptance: an untraceable denial).
    ("approval-decided-by", "approvalDecidedBy"),
    // 2026-10-18: session resources (GET /v2/sessions items, retired ones
    // included) carry runtime.pendingServerRequests [{id, method}]: native
    // server requests of the current runtime generation still unanswered
    // (recorded from serverRequest, cleared by serverRequest/resolved, the
    // end of their turn or a disconnect), so a client connecting later sees
    // them. The full messages stay in the session bootstrap.
    ("session-pending-server-requests", "sessionPendingServerRequests"),
    // Profile nativeSubagents (Codex) in /v2/profiles detail + PATCH.
    ("profile-native-subagents", "profileNativeSubagents"),
    // Profile agentUseRequiresApproval: agents launching agents on it wait
    // for an owner approval.
    ("profile-agent-approval", "profileAgentApproval"),
    // Claude profile agentTurnQuestions ("decide" | "wait", default decide)
    // in /v2/profiles items + detail + PATCH: AskUserQuestion in a turn not
    // started by the owner answers at once instead of blocking.
    ("profile-agent-turn-questions", "profileAgentTurnQuestions"),
    // Claude tools: /v2/profiles claudeAllowWrite / claudeAllowExec (items,
    // editable.claudeTools, PATCH) for new sessions, and the session's own
    // runtimeDefaults claudeAllowWrite / claudeAllowExec, changed with
    // cutex/session/defaults/update and applied at the next start.
    ("claude-tools-settings", "claudeToolsSettings"),
    // /v2/profiles planLabel follows the signed-in ChatGPT account's plan
    // (planLabelSource "account" | "manual" | null; detail planLabelManual)
    // and PATCH planLabel (Codex, <= 40 chars, null clears) edits the
    // stored label. Order since r221: profile-plan-label-override; Claude
    // profiles: profile-plan-label-claude.
    ("profile-plan-label", "profilePlanLabel"),
    // Codex profile modelReasoningSummary ("auto" | "concise" | "detailed" |
    // "none" | null = not set, model default) in /v2/profiles items + detail
    // config + PATCH (null clears); detail.editable.modelReasoningSummary is
    // false on DeepSeek profiles (their preset removes the key) and a PATCH
    // setting it there is refused.
    ("profile-reasoning-summary", "profileReasoningSummary"),
    // Codex profile updatePlanEnabled (true | false | null = not set; the
    // native `update_plan` tool is registered only when true) in
    // /v2/profiles items + detail config + PATCH (null clears); stored as
    // `[tools.update_plan] enabled` and forwarded to native launches.
    // detail.editable.updatePlanEnabled: Codex profiles (incl. DeepSeek).
    ("profile-update-plan", "profileUpdatePlan"),
    // Bootstrap cutexState.latestPlan and GET /v2/sessions/{id}/history
    // cutexState.latestPlan: {threadId, turnId, explanation, plan: [{step,
    // status: pending|inProgress|completed}], updatedAt (RFC 3339)} | null —
    // the newest update_plan call the native accepted ("Plan updated"),
    // derived from the session's current rollout (revert-consistent).
    ("session-latest-plan", "sessionLatestPlan"),
    // Claude sessions: native.schema {protocol: claude-app-server-subset,
    // capabilities.features} lists the Codex v2 surfaces the adapter serves
    // (unsupported methods fail with -32601 data.unsupported); Claude
    // approvals / AskUserQuestion reach Management clients as Codex server
    // requests; cutex/userInput/submit `auto` on a busy Claude agent queues;
    // offline-history and offline-submit cover Claude sessions.
    ("claude-runtime-features", "claudeRuntimeFeatures"),
    // Management client tokens (`cxm1.…`, issued with `cutex management
    // client-token issue`): root-equivalent on Management, never on the Agent
    // Bus; GET /v2/client-tokens, DELETE /v2/client-tokens/{id}.
    ("management-client-tokens", "managementClientTokens"),
    // POST /v2/sessions/{id}/attachments (raw image body) → host-local
    // `localImage` path; settings `attachments {maxBytes, retentionDays}`.
    ("session-attachments", "sessionAttachments"),
    // GET /v2/fs/dirs?path= : host directory browsing (directories only).
    ("fs-dir-browse", "fsDirBrowse"),
    // GET /v2/fs/file?path=<absolute>&maxBytes= : host file read (owner
    // only: root credential or a client token), workspace fs/file body.
    ("fs-file-read", "fsFileRead"),
    // Asks (委托, cutex-desktop docs/cutex-agent-asks.md P1): /v2/asks,
    // /v2/asks/{id}, /v2/asks/{id}/answer, cutex/ask/create|stop|close|retry|
    // respond, events cutex/ask/created|stateChanged|answered|closed|question|
    // progress|budgetExceeded; agent tools agent_list/ask/wait/respond/reply/
    // tell, ask_close, ask_read.
    ("agent-asks", "agentAsks"),
    // ask_delivery approvals in /v2/approvals (P1: decided by the owner).
    ("ask-gate", "askGate"),
    // GET /v2/artifacts/{id}: daemon-owned handoff / answer artifacts.
    ("ask-artifacts", "askArtifacts"),
    // Bootstrap (and GET /v2/sessions/{id}/history) cutexState.latestTokenUsage:
    // the newest native token_count of the session's current rollout
    // (revert-consistent) in the app-server `thread/tokenUsage/updated`
    // params shape {threadId, turnId, tokenUsage: {total, last,
    // modelContextWindow}} plus updatedAt (RFC 3339) | null (no usage yet).
    // Claude sessions (no rollout): the newest such notification the adapter
    // sent for the bound thread, kept by the daemon across restarts
    // (`claude_token_usage`; dropped when a revert removes its turn). Live
    // notifications supersede it.
    ("bootstrap-token-usage", "bootstrapTokenUsage"),
    // GET /v2/sessions/{id}/bootstrap?turnLimit=N (1..=200): a connected
    // session's native.result.thread.turns holds only the newest N turns
    // (oldest first, native thread/turns/list) and the bootstrap carries
    // `historyPage {limit, cursor, nextCursor, hasMore}` (null while offline).
    // Older turns: GET /v2/sessions/{id}/history?cursor=<nextCursor>, live or
    // offline. Without turnLimit the bootstrap is unchanged (every turn).
    ("history-pages", "historyPages"),
    ("history-turn-read", "historyTurnRead"),
    ("history-turn-ordinals", "historyTurnOrdinals"),
    // Agent messaging v2 (r208): note/wake deliveryKind, AgentParty sender/recipient,
    // messageDelivered on both streams (Codex + Claude), bootstrap agentBusSentMessages,
    // view `cutex.agent-message.v1` body, cutex/agentBus/wake, agent_tell wake.
    ("agent-messages-v2", "agentMessagesV2"),
    // Delegation (委托) views `cutex.delegation-ask.v1` / `cutex.delegation-results.v1`,
    // GET /v2/asks involving/limit/hasMore, AskSummary attempt/answerPreview (r208).
    ("delegation-views", "delegationViews"),
    // Owner decision 2026-10-08 (r208): cutex/turn/stop and queue pause never
    // pause delegation (委托). Owner-only cutex/asks/pause|resume {} →
    // {asksPaused, changed}; the owner's next cutex/userInput/submit
    // (started/steered/queued) also clears it. queue/list result and
    // bootstrap cutexState carry asksPaused {since, by, origin} | null; event
    // cutex/asks/pauseChanged {asksPaused, reason: paused|resumed|ownerMessage}.
    ("asks-pause-v2", "asksPauseV2"),
    // Mobile API phase A (cutex-desktop docs/research/cutex-mobile-api.md):
    // config `management_listen` / repeated `--bind` incl. `tailscale`;
    // non-loopback listeners accept only client / device tokens (401
    // remote_credential_refused for root); keep-alive (30 s idle) and a
    // 256-connection cap (503 too_many_connections).
    ("remote-listeners", "remoteListeners"),
    // POST /v2/pairing-codes (loopback, root or client token), public
    // POST /v2/pair → device token (kind "device"); client-token list adds
    // kind/deviceName/platform/lastAddress; revoke closes the token's SSE
    // streams after `event: cutex/device/revoked`.
    ("device-pairing", "devicePairing"),
    // CORS allow-list (`management_cors_origins`, default localhost origins
    // of Capacitor shells), OPTIONS preflight on every listener.
    ("cors", "cors"),
    // A subscriber that falls behind gets `cutex_management_stream_error`
    // {code: "lagged", details.lastDeliveredCursor} before the stream closes.
    ("stream-lagged", "streamLagged"),
    // GET /v2/summary (agents: status, needs, preview, lastTurn, unreadTurns)
    // and GET /v2/summary/events (SSE, cursor = journal seq, 7-day retention,
    // 409 cursor_expired).
    ("host-summary", "hostSummary"),
    // ?compact=1 on bootstrap / history: tool item strings > 2 KB cut, item
    // gets truncated: true (full text = same request without compact).
    ("compact-history", "compactHistory"),
    // Accept-Encoding: gzip → Content-Encoding: gzip on JSON responses
    // >= 1 KB (not SSE).
    ("gzip", "gzip"),
    // `cutex management schema export --out <dir>`: JSON Schema (2020-12)
    // of sessions, bootstrap, events, approvals, asks, notifications,
    // summary, pair, runtime-packages + index.json.
    ("schema-export", "schemaExport"),
    // /v2/settings managementListen (restart required) and
    // managementCorsOrigins (applied at once); writes on loopback only.
    ("listen-settings", "listenSettings"),
    // POST /v2/summary/read {agentId}: clears the agent's unreadTurns and
    // acknowledges its notification reminder (no reminderId needed).
    ("summary-read", "summaryRead"),
    // Root-only runtime package upgrade of explicit-launch agents:
    // GET /v2/runtime-packages (selected bundle + outdated agents + active /
    // last run), POST /v2/runtime-packages/runs {agentIds | all} (202, one
    // run at a time: 409 run_in_progress), GET /v2/runtime-packages/runs
    // [?limit], GET /v2/runtime-packages/runs/{runId}, POST
    // /v2/runtime-packages/runs/{runId}/revert {agentIds?} (in-daemon, no
    // service stop) and the cutex/runtimePackages/runUpdated host event
    // {runId, kind, status, revertOf, startedAt, finishedAt, currentAgentId,
    // counts, item?} (only the changed item). Schema export runtime-packages.
    ("runtime-package-upgrade", "runtimePackageUpgrade"),
    // Agent → owner messages (cutex-desktop docs/research/owner-notify.md,
    // docs/research/cutex-mobile-api.md §10):
    // agent tool owner_notify; GET /v2/owner-notifications; session methods
    // cutex/ownerNotify/ack|reply|dismiss; events cutex/ownerNotify/created|
    // updated; bootstrap cutexState.ownerNotifications; /v2/settings
    // ownerNotify; summary needs ownerMessage / ownerReply and
    // cutex/summary/messageToOwner.
    ("owner-messages", "ownerMessages"),
    // Owner-notify records carry origin agent | system (system = cutex's own
    // notices: delivery failures, delegation reminders); root-only session
    // method cutex/ownerNotify/delete {notificationId} removes a record
    // (event cutex/ownerNotify/deleted {notificationId, agentId});
    // cutex/ownerNotify/reply answers 409 notification_not_replyable for
    // records without needsReply or with origin system.
    ("owner-notify-delete", "ownerNotifyDelete"),
    // Root-only stuck Agent Bus messages (docs/agent-bus-operations.md):
    // GET /v2/agent-bus/stuck-messages?minAgeSecs=&agentId=&limit=
    // (pending / stale_held records and un-ACKed live-queue entries with
    // reasons, age, live placement), POST
    // /v2/agent-bus/stuck-messages/discard {messageId, reason,
    // expectedSemanticSha256?, allowProtected?} (terminal
    // owner_message_discarded + live withdraw + audit fact; idempotent).
    ("agent-bus-stuck-messages", "agentBusStuckMessages"),
    ("health-check", "healthCheck"),
    // Input overflow (cutex-desktop docs/research/input-overflow-documents.md):
    // delegation / result / bus texts are fitted to the receiver's byte
    // policy (render levels; long parts → structured overlength notices with
    // a preview and a delegation_read / read_overlength_content call, §0.4;
    // messages spill to documents at production or delivery time);
    // GET /v2/documents[?source=], /v2/documents/usage, /v2/documents/{id};
    // /v2/settings documents; host event cutex/ask/deliveryFailed and
    // quarantined bus messages with code event_too_large /
    // documents_quota_full also create an ownerMessage need.
    ("input-overflow-documents", "inputOverflowDocuments"),
    // Closed delegations and finished agent messages are archived after
    // documents.closedRetentionDays (default 30) and kept
    // documents.archiveRetentionDays (default 365):
    // GET /v2/record-archive?kind=&month=&involving=&q=&limit=&offset=,
    // GET /v2/record-archive/{recordId}; host event cutex/documents/sweep.
    ("record-archive", "recordArchive"),
    // Root-only human Job Service bridge (Linux only: no runner elsewhere):
    // POST /v2/job-service/management-query (list; rows carry `command`),
    // GET /v2/job-service/capabilities, POST /v2/job-service/cancel
    // {jobId, expectedRevision}, POST /v2/job-service/output
    // {jobId, stream, offset?, maxBytes?}. 501 job_service_operation_unsupported
    // when the deployed Job Service predates cancel/output.
    // Independent instances (CUTEX_HOME, docs/INSTANCES.md): capabilities
    // carry `instance` {name, isolated} (null for the default instance).
    ("instance-home", "instanceHome"),
    // /v2/settings sessionStore {receiptRetentionDays} (default 7, 0-365):
    // completed explicit-launch receipts nothing needs are pruned at start
    // and every 6 h; a pruned action id answers explicit_launch_action_pruned.
    ("session-store-retention", "sessionStoreRetention"),
    // Notification levels (cutex docs/NOTIFICATION-LEVELS.md): GET|PATCH
    // /v2/notification-levels (event registry, event → level and level →
    // methods presets + changed cells, project mutes), POST
    // /v2/notification-levels/evaluate; /v2/settings notificationLevels;
    // owner-notify records carry notificationLevel / notificationEvent /
    // actions / systemKind; summary needs[] items carry level (kind
    // delegationFailed only with a non-legacy event preset). Absent settings
    // = legacy presets (r216 behaviour).
    ("notification-levels", "notificationLevels"),
    // Profiles: appearance.providerCategory (manual, null = unset) and
    // effectiveProviderCategory (root and appearance; manual else inferred)
    // in kimi|glm|zai|claude|deepseek|codex|other (legacy chatgpt = codex);
    // PATCH /v2/profiles/{p} {providerCategory: <value>|null}.
    ("profile-provider-category-v2", "profileProviderCategoryV2"),
    // Usage cost estimates from a price library (LiteLLM / models.dev,
    // cached daily) under the manual table: /v2/settings usage
    // {priceSource, catalogRefreshHours, catalogUrl}; /v2/usage rows
    // priceSources / unpricedRequests, unpricedModels, pricing; GET
    // /v2/usage/prices pricing + ?library=; POST /v2/usage/prices/refresh;
    // manual entries cacheWriteInput / aliases (docs/usage-pricing.md).
    ("usage-price-catalog", "usagePriceCatalog"),
    // Display currency USD|AUD|CNY with exchange rates: /v2/settings usage
    // {displayCurrency, exchangeRates, exchangeRateAutoFetch}; manual
    // entries may set currency; reports carry currency + pricing.exchangeRates.
    ("usage-currency", "usageCurrency"),
    // Extra usage sources (usage/external.rs, docs/usage-pricing.md §Extra
    // usage sources): token usage of Claude Code / Codex runs outside cutex.
    // GET|POST /v2/usage/sources, GET|PATCH|DELETE /v2/usage/sources/{id}
    // (?purge=true deletes its rows), POST /v2/usage/sources/{id}/rescan
    // (202); a path overlapping a directory cutex records is 409
    // usage_source_overlaps_cutex. GET /v2/usage gains ?source=cutex|external|<id>,
    // groupBy `source` (key source / sourceLabel / sourceKind / sourceBilling);
    // imported rows carry `source` in the ledger.
    ("usage-external-sources", "usageExternalSources"),
    // Pricing model map (docs/usage-pricing.md §Pricing model map): GET|PUT
    // /v2/usage/prices modelMap {"<from>": "<to>"} (≤ 256, keys ignore case,
    // one hop; a PUT without modelMap keeps the stored map); GET /v2/usage
    // rows / totals pricedAs [..], unpricedModels[].pricedAs.
    ("usage-model-map", "usageModelMap"),
    // GET /v2/runtimes (root incl. client tokens): read-only view of the
    // native version registry entries and Claude runtimes with the agents
    // pinned to each, plus the cutex release (schema export runtimes).
    ("runtimes-view", "runtimesView"),
    // Agent harness (cutex-desktop docs/research/agent-harness.md): every
    // launch starts with a `# Cutex` section (environment, labelled inputs,
    // cutex tools, output, this agent); Claude agents get it through the
    // Claude Code preset system prompt (append). /v2/settings agentHarness
    // {enabled} (new runtimes only); GET /v2/sessions/{id}/harness (root)
    // returns the rendered text. Model-facing delegation markers are
    // English: [delegation <id> · attempt N], [delegation results],
    // [delegation reminder], [delegation <id> · revision vN].
    ("agent-harness", "agentHarness"),
    // Projects without a Director: POST /v2/agent-management/project-mutations
    // kinds create_project {presentation, workspace_root?, director_cutex_session_id?},
    // assign_director {cutex_session_id}, clear_director; project summaries /
    // workspaces omit director_cutex_session_id / director when there is none
    // (docs/agent-management-provider.md §Projects without a Director).
    ("project-optional-director", "projectOptionalDirector"),
    // r219 removed the legacy Task Service (docs/r219-removals.md): no
    // session-tasks / task-detail; /v2/task-service/*, /v2/tasks/{id},
    // /v2/sessions/{id}/tasks and /v2/projects/{id}/tasks answer 410
    // `tool_removed`; no cutex/taskService/* events. Clients drop their task
    // views (old transcripts keep their task delivery rows).
    ("task-service-removed", "taskServiceRemoved"),
    // Claude accounts (cutex-desktop docs/research/claude-multi-account.md):
    // GET/POST /v2/claude/accounts, GET/PATCH/DELETE /v2/claude/accounts/{id}
    // (?deleteHome=true), POST …/{id}/status, POST …/{id}/logout (root incl.
    // client tokens; apiKey write-only; no login over HTTP: items carry the
    // terminal loginCommand); profiles {claudeAccount: "default"|<id>}
    // (POST / PATCH /v2/profiles); Claude session resources claudeAccount
    // {id, label, kind, isDefault}; a profile switch across accounts is 409
    // claude_account_mismatch; Claude quota per account (source "claude" |
    // "claudeApi", nativeQuota.claudeAccount / apiUsage); schema export
    // claude-accounts.
    ("claude-accounts", "claudeAccounts"),
    // r220 (r217h1 / r217h2 hotfixes): a wake (after_turn) or delegation
    // results for a busy Codex recipient that takes native `soon` wait up to
    // /v2/settings agentBus.wakeBusyWaitSecs (default 120; 0 = next step at
    // once; null = never early: wait for the turn to end) for the running
    // turn to end, then are read at its next step (no turn of their own). An
    // idle recipient still gets a turn at once; Claude runtimes keep waiting
    // for the turn end. Read per delivery (no restart).
    ("wake-busy-wait", "wakeBusyWait"),
    ("owner-rotation", "ownerRotation"), // /v2/director-rotations (owner_rotation_api.rs), mutation rotate_director, event cutex/ownerRotation/updated
    // planLabel order: a manual planLabel (PATCH) overrides the signed-in
    // ChatGPT account's plan (source "manual"); without one the account's
    // live plan shows ("account"), then the legacy plan_type. Live plan: a
    // native rate-limit planType observed after the login token was minted
    // wins over the token claim (the login view's planType is the same
    // value). detail.planLabelManual leaves out the legacy plan_type behind
    // a live plan (null = show the account's plan). cutex/profile/quota/updated
    // (source native) carries planChanged when the observed plan changed.
    ("profile-plan-label-override", "profilePlanLabelOverride"),
    // Artifact store (artifact_store.rs): /v2/artifact-store routes, MCP
    // artifact_publish / artifact_list / artifact_read, project auto capture,
    // events cutex/artifact/published|updated, settings artifacts.*.
    ("artifacts", "artifacts"),
    // MCP artifact_publish {show: true}: after the publish, host event
    // cutex/artifact/showRequested {artifactId, version, ref, scope, name,
    // kind, mediaType, cutexSessionId, displayName, hostId, newVersion,
    // requestedAt} (artifacts/show.rs; per session burst 3, then 1 per 5 s;
    // a limited publish answers shown: false, reason rate_limited).
    ("artifact-show", "artifactShow"),
    // r224: a Claude account quota read whose stored login token expired
    // (or expires within 60 s) makes no request: GET /v2/profiles/{p}/quota
    // nativeQuota.credentialsExpired = true + credentialsExpiredAt (unix
    // seconds), lastError null, consecutiveFailures not counted, the last
    // windows and fetchedAt kept. Claude Code refreshes the token the next
    // time a session on that account runs (cutex never refreshes it). A 401 /
    // 403 with a valid token stays lastError with a re-login hint.
    ("claude-quota-stale-credentials", "claudeQuotaStaleCredentials"),
    // r224: GET /v2/host (host_info.rs) gains instanceId (same as here,
    // <cutex home>/instance-id), arch, osVersion (short human string or null)
    // and cutexBuild (same object as here); schema export `host`.
    ("host-identity", "hostIdentity"),
    // r225: agent alarms for the owner (alarms_api.rs): GET /v2/alarms
    // ?sessionId=&state=pending|all&limit= (rows set by or waking that agent,
    // relation outgoing|incoming|self; nextAt), GET /v2/alarms/{id} (full
    // message), DELETE /v2/alarms/{id} (owner cancel; 409
    // alarm_already_queued); host event cutex/alarms/changed {reason, id,
    // alarm, ownerCutexSessionId, targetCutexSessionId, hostId} from a
    // Management watcher of the alarm store; schema export `alarms`.
    ("alarms-api", "alarmsApi"),
    // Owner archive for user skills of the shared CODEX_HOME
    // (skills_archive.rs): GET /v2/codex/skills/archive, POST
    // /v2/codex/skills/archive {requestId, path} (moves the skill entry to
    // <cutex home>/archive/skills/<id>/skill + record.json; 409
    // skill_not_archivable {reason system|plugin|not_user_skill}),
    // POST /v2/codex/skills/archive/{id}/restore {requestId} (409
    // skill_name_taken); idempotent by requestId; schema export
    // `skills-archive`.
    ("skills-archive", "skillsArchive"),
    // Workflows W1 (desktop docs/research/workflows-20261013.md): agent tools
    // workflow_start / workflow_read / workflow_list / workflow_stop (an
    // agent-started run waits for the owner's approval, kind workflow_run);
    // GET /v2/workflows/runs?state=&sessionId=&projectId=&limit=,
    // GET /v2/workflows/runs/{id}[?include=steps,log,result,script],
    // …/{id}/script|result|log|steps/{key}, POST /v2/workflows/runs (owner
    // start), POST …/{id}/stop|resume, DELETE …/{id}; host events
    // cutex/workflow/run/changed, cutex/workflow/step/changed,
    // cutex/workflow/log; settings block `workflows` in config.json.
    ("workflows", "workflows"),
    // Runs survive a daemon restart (live steps re-attach; otherwise
    // stopped(interrupted)) and resume from a finished run's journal
    // (resume_from_run_id / POST …/{id}/resume).
    ("workflow-resume", "workflowResume"),
    // Durable runtime online intent (session/runtime_intent.rs): set when a
    // runtime goes online, cleared only by a requested offline / close /
    // archive. A Management start that finds intended runtimes offline
    // records an unclean stop: GET /v2/runtimes/restore-candidates
    // {hostId, uncleanSince, reason host_rebooted|runtimes_exited, items:
    // [{cutexSessionId, displayName, lastOnlineAt}]}, DELETE dismisses
    // ({dismissed}); restore = cutex/runtime/online; schema export
    // runtime-restore.
    ("runtime-restore-candidates", "runtimeRestoreCandidates"),
    // r233: Claude quota auto-continue (quota_continue.rs): a Claude turn
    // that failed on an exhausted usage limit gets one owner-origin
    // continuation at the limit's reset (+ 30-119 s jitter), per agent,
    // default on; GET /v2/quota-auto-continue[?sessionId=], PATCH
    // /v2/quota-auto-continue/{sessionId} {enabled}, DELETE
    // /v2/quota-auto-continue/{sessionId}/pending; host event
    // cutex/quotaAutoContinue/changed; schema export `quota-auto-continue`.
    ("claude-quota-auto-continue", "claudeQuotaAutoContinue"),
    // r233: a closed native input gate (Claude adapter halted after an
    // interrupted / failed turn, Codex N2 gate) that refused a delivery is
    // a pause the owner sees: asksPaused.origin `runtimeGate` (asks held
    // heldByRuntime(gate_closed), not retried), pauseChanged reasons
    // runtimeGate / runtimeRestart / turnStarted; released by
    // cutex/asks/resume, cutex/userInput/queue/resume or the owner's next
    // message (all reopen the gate); the owner's own Claude turn/start
    // refused by the halted adapter reopens the gate and is sent once more.
    ("runtime-gate-pause", "runtimeGatePause"),
    ("peers", "peers"),
    // Independent wire DTOs and negotiated features (P0.1).
    ("peers-v2", "peersV2"),
    ("cross-host-asks", "crossHostAsks"),
    // agent_tell to name@host: an FYI note to an agent a paired host exports
    // (POST /peer/v1/tells, optional hello feature tells.v1; notes only, no
    // wake / attachments / revisions; receipts runtime/peers/tell-receipts.json).
    ("cross-host-tell", "crossHostTell"),
    // agent_tell to name@host with wake:true (hello feature tells.v2,
    // schemaVersion 2): allowed only by the receiving peer's per-peer
    // inbound.tellWake (default off; review required = refused) → 403
    // wake_not_allowed, nothing delivered; inbound.tellWakesPerHour (60)
    // beyond which a wake arrives as a note (wakeDowngraded, reason
    // rate_limited); local bus control type cutex.peer.tell.v2, after_turn;
    // agent_list otherHosts[].tellWake.
    ("cross-host-tell-wake", "crossHostTellWake"),
    // P1 users and grants: /v2/users, token → user bindings, default-deny
    // access gate for non-owner tokens, `user` in /v2/capabilities.
    ("users", "users"),
    // Self-service user profiles: GET|PATCH /v2/users/me {displayName?,
    // avatar?: {contentType, data}|null} for the caller's own record (owner or
    // a bound user), GET /v2/users/{id}/avatar (ETag; owner sees all, a user
    // the owner, itself and users sharing a visible agent); displayName /
    // avatarEtag in /v2/users items, /v2/capabilities user and actingUser.
    // Stored in runtime/users/profiles-v1.json + avatars/ (never config.json).
    ("users-self-profile", "usersSelfProfile"),
    // Person identity (P2): GET /v2/identity/challenge + POST
    // /v2/identity/claim (Ed25519 over a one-use 30 s nonce; any
    // authenticated principal, own credential only; a known key auto-binds a
    // pending token, the first device's / root's key goes on owner, a bound
    // token is never rebound); personKeys / tokenPersons in GET /v2/users;
    // PUT /v2/users/{id}/person-keys (owner); peer pairing carries the owner
    // keys. Stored in runtime/users/persons-v1.json (users-v1.json unchanged).
    ("person-identity", "personIdentity"),
    // r236: DELETE /v2/users/token-persons/{tokenId}/previous (owner): the
    // owner acknowledges a device's key change; clears that token's
    // person.previousPublicKey in GET /v2/users → {tokenId, cleared,
    // previousPublicKey: null, personsRevision}; audit
    // tokenPersonChangeAcknowledged.
    ("person-identity-ack", "personIdentityAck"),
    // Peers sync the owner's profile (crate::peers::profiles): a local user
    // bound to a paired peer owner's person key gets that owner's directly-
    // set displayName / avatar (GET /peer/v1/owner-profile[/avatar], hello
    // feature owner-profile.v1, ownerProfile.etag in hello). Values set or
    // cleared here via PATCH /v2/users/me always win. GET /v2/users items
    // gain profileSource; host event cutex/users/changed {userId, reason
    // peerProfile, peerId}. Peers disabled: nothing is fetched or written.
    ("peer-owner-profile", "peerOwnerProfile"),
    // r236 P2 access for non-owner users (cutex-desktop
    // docs/research/user-isolation-p2-access-20261015.md): read-only
    // {scope}/fs/dir|file|search|watch and {scope}/git/status|diff for a
    // granted project (/v2/projects/{id}) or a visible agent's cwd
    // (/v2/sessions/{id}) with the read grant (never a filesystem root, the
    // home or the cutex home: 403 workspace_root_too_broad; terminals stay
    // the owner's); with send: POST /v2/sessions/{id}/attachments and
    // localImage inputs for attachments the same token uploaded to that
    // session (403 attachment_not_owned otherwise), and cutex/ask/stop|close|
    // retry|respond|resume|reopen when the ask (or the question's ask) is
    // visible and the path session is its requester or target (404
    // ask_not_found / question_not_found otherwise).
    ("users-p2-access", "usersP2Access"),
    // POST /v2/human/agents accepts Claude subscription profiles: the agent is
    // created like agent_create / quick chats (the Claude runtime selected for
    // new sessions; the profile's Claude model / effort / write / exec rules,
    // launch.model / reasoningEffort override), imported into the roster and
    // optionally joined to the Project. launch sandbox / approvalPolicy /
    // networkAccess / writableRoots are refused for Claude (stage validation,
    // code unsupported_launch_option). Without this feature the route answers
    // unsupported_profile for Claude profiles.
    ("human-agent-claude", "humanAgentClaude"),
    // POST /v2/human/agents without cwd and projectId gives the agent a fresh
    // owner-private scratch workspace <config dir>/agents-scratch/<yyyymmdd>-
    // <8 hex> (0700 / protected DACL) as its cwd; a requestId retry replays
    // the answer (no second directory). Without this feature the route
    // answers cwd_required. A Project without workspace_root still answers
    // workspace_root_required.
    ("human-agent-scratch-cwd", "humanAgentScratchCwd"),
    // Claude profile write / exec rules are tri-state, unset = on: /v2/profiles
    // detail claudeAllowWriteExplicit / claudeAllowWriteDefault (same for
    // Exec), PATCH claudeAllowWrite / claudeAllowExec true | false | null
    // (null = unset); claudeAllowWrite / claudeAllowExec are the effective
    // values. Claude sessions follow their profile's current rule at every
    // start unless the rule was set for the session (cutex/session/defaults/
    // update); runtimeDefaults shows the effective values. One-time
    // migration claude-tool-defaults cleared the stored profile values.
    ("claude-tool-defaults", "claudeToolDefaults"),
    // PATCH /v2/profiles/{p} `model` / `modelReasoningEffort` on a Claude
    // profile set the profile's Claude default model / effort (`model` any
    // id or alias without spaces, null = built-in `sonnet`; effort
    // low..max, null = model default) instead of answering 400. The Claude
    // profile item `model` is the effective new-session default
    // (newSessionDefaults[profile].model, else the profile's own); detail
    // gains claudeProfileModel / claudeNewSessionModel /
    // claudeBuiltinDefaultModel and editable.claudeDefaultModel. A PATCH
    // `model` drops a disagreeing newSessionDefaults model of that profile,
    // so the written value is what new sessions get.
    ("claude-profile-default-model", "claudeProfileDefaultModel"),
    // The legacy IM exposure flag (session `exposed_to_backend`, agent_create
    // `expose_to_im`) no longer hides persistent agents: GET /v2/sessions,
    // exact-ID routes, summary and Users P2 scopes include every active
    // persistent session (`visible` true); local-only sessions keep the old
    // flag rule (includeHidden still shows them to root). agent_create drops
    // expose_to_im from its schema (still accepted, ignored). One-time
    // migration archive/im-exposure-retired/ sets the flag on existing
    // persistent sessions (older binaries list them).
    ("im-exposure-retired", "imExposureRetired"),
    // Owner-granted browser leases, cxa1-only tools and targeted device SSE.
    ("browser-relay-v1", "browserRelayV1"),
    // Root + loopback owner recovery for stuck browser idempotency rows.
    ("browser-idempotency-clear", "browserIdempotencyClear"),
    // Codex profile webSearch (native `web_search` mode: "disabled" |
    // "cached" | "indexed" | "live" | null = not set, native default) in
    // /v2/profiles items + detail config + PATCH (null clears); stored as the
    // profile-scoped root key and forwarded to native launches.
    // detail.editable.webSearch: Codex profiles. API-key preset `gemini`
    // ("Gemini (OpenAI-compatible relay)", base URL required) writes
    // "disabled" (Gemini refuses built-in tools mixed with function calls);
    // discovered gemini-* catalog rows get Google's documented metadata.
    ("profile-web-search", "profileWebSearch"),
    // Codex profile toolSchemaCompat (native `tool_schema_compat`: "off" |
    // "strict" | null = not set, off) in /v2/profiles items + detail config +
    // PATCH (null clears); stored as the profile-scoped root key and forwarded
    // to native launches (r53+ natives act on it, older ones ignore it).
    // "strict" lowers every tool-parameter enum to one shared `type` right
    // before a request is sent (Google function declarations refuse mixed /
    // typeless / nullable enums). detail.editable.toolSchemaCompat: Codex
    // profiles. API-key preset `gemini` writes "strict".
    ("profile-tool-schema-compat", "profileToolSchemaCompat"),
    // Data-driven API-key presets: every preset with a catalog writes it at
    // creation (deepseek, zai, zhipu, moonshot / moonshot-cn, minimax /
    // minimax-cn, xai); zai / zhipu use the Responses endpoints (/api/v1);
    // DeepSeek defaults to deepseek-flash with web_search disabled. Preset
    // items gain `modelCatalog` / `webSearch`; known-model metadata fills
    // unset catalog fields by slug; OpenRouter discovery reads context /
    // modalities / reasoning; auto quota readers for the Zhipu Coding Plan
    // (5 h + week) and OpenRouter credits; /v2/health warns about GLM
    // profiles on /api/coding/paas/v4 (finding
    // profile_chat_completions_endpoint, report-only).
    ("profile-presets-v2", "profilePresetsV2"),
    // Native r53 (0.162.1) integration: Codex profile
    // providerInstructionsField (true | false | null = not set, native
    // default false) in /v2/profiles detail config + PATCH, stored as the
    // custom provider's `capabilities.instructions_field` and forwarded to
    // native launches (r53+: base instructions in the top-level Responses
    // `instructions` field instead of a `developer` message; older natives
    // ignore it). Every cutex-built native launch pins
    // `features.instant_interrupt=false`; agent message phase
    // `partial_answer` is activity class progress.
    ("native-r53", "nativeR53"),
    // profile-search-tool: Codex API-key profile `searchTool` (bool, default
    // off; /v2/profiles items + PATCH, detail.editable.searchTool = Codex
    // API-key profile). On: agents (and quick chats) get the cutex tool
    // `web_search` = one separate non-streaming Responses request to the
    // same profile's provider / model with only the built-in
    // `{"type":"web_search"}` tool, answer text + url_citation sources back,
    // per-agent rate limit 20 / h, provider timeout 90 s, token usage in the
    // usage ledger. Owner switch only: no preset turns it on.
    ("profile-search-tool", "profileSearchTool"),
    // cutex/session/metadata/update avatar gains optional `color` (#rrggbb)
    // for emoji / icon avatars: an emoji AND a circle colour together.
    ("agent-avatar-color", "agentAvatarColor"),
    // r236: browser_open (a page in the agent login pool) needs no owner
    // approval for an agent whose session runs with full access (native
    // sandbox danger-full-access + approval never, the desktop's 完全访问;
    // live native thread settings first, the stored launch permissions
    // otherwise), even with browser.agentOpenApproval = "ask".
    // browser_claim (an owner tab, owner logins) still always asks.
    ("browser-open-full-access", "browserOpenFullAccess"),
    // profile-generic-switches: no rule keyed on a relay / base-URL domain,
    // provider name, profile name or id. (1) Codex profile switch
    // `privateChatgptAuthFile` (bool, default off; /v2/profiles items +
    // PATCH, detail.editable.privateChatgptAuthFile = Codex): explicit agent
    // launches use the host's private native ChatGPT auth.json with the
    // profile's own model / effort (both required); Human New refuses such a
    // profile. (2) Quota presets are explicit: quota-config `preset` =
    // `custom` | `usage_endpoint` (<base URL>/usage, rate_limits[0]) |
    // `deepseek_balance` | `moonshot_balance` | `moonshot_cn_balance` |
    // `zhipu_coding_plan` | `openrouter_key` (`presetOptions` lists them);
    // `auto` is refused on PUT and reported only for a profile the startup
    // migration could not resolve (presetError). /v2/profile-presets items
    // gain `quotaReader` (`autoQuota` = it is set). Startup migration
    // (archive/profile-generic-switches/) converts the legacy state.
    ("profile-generic-switches", "profileGenericSwitches"),
    // Backend auto-update (desktop docs/research/backend-auto-update-rfc-20261014.md,
    // upgrade_api.rs, docs/backend-upgrade.md): owner-only GET /v2/upgrade,
    // POST /v2/upgrade/check|stage|apply|cancel|rollback, GET
    // /v2/upgrade/runs[/{id}], GET|PATCH /v2/upgrade/settings; host event
    // cutex/upgrade/updated; /v2/host serviceManager + runtimes. Mutations
    // answer 409 upgrade_blocked {reason: unmanaged_install} unless the
    // daemon runs from a managed install (install.json).
    ("backend-upgrade", "backendUpgrade"),
    // Backend upgrade run records (`$defs/upgradeRun`, GET /v2/upgrade `run`,
    // GET /v2/upgrade/runs[/{id}], `cutex upgrade history --json`) carry
    // `nativeVersion` (the native the target release carries),
    // `previousNativeVersion` and `runtimeChange` (`native` | `package` |
    // `none`), recorded when the run is prepared; null on older records.
    ("upgrade-run-native", "upgradeRunNative"),
    // GET /v2/upgrade `installed` carries `notes` ({en, zh-CN, …} as in the
    // release manifest) and `publishedAt`, read from the installed release's
    // committed manifest; null when unknown (no manifest, older layouts).
    ("upgrade-installed-notes", "upgradeInstalledNotes"),
    // Optional components (components/README.md, components_api.rs):
    // owner-only GET /v2/components, POST /v2/components/{name}/install|remove
    // (empty body, 202 {operation}); host event cutex/components/updated;
    // backend updates keep installed components in step. Static per build:
    // component state is only in GET /v2/components.
    ("components", "components"),
    // create_project {create_workspace_root: true} (project-mutations): a
    // missing workspace_root is created (missing tail only, no `.` / `..`,
    // as the daemon user) instead of refused with workspace_root_not_found;
    // a file in the way is workspace_root_not_directory, any other failure
    // workspace_root_create_failed.
    ("project-create-workspace-root", "projectCreateWorkspaceRoot"),
    // DELETE /v2/profiles/{p}: profile_in_use counts only this host's
    // sessions (records of another host name that host's profiles); with
    // ?archived=detach archived references no longer refuse: their stored
    // profile is cleared (restore uses the default profile) and the 200
    // body lists them in `detachedSessions`. Active ones still refuse.
    ("profile-delete-detach-archived", "profileDeleteDetachArchived"),
    // Fast mode per session (Codex): the record keeps the native service tier
    // (`priority` = Fast, `default`, `flex`) mirrored from
    // thread/settings/updated and sends it as `serviceTier` on every thread
    // start / resume (the native drops it on resume). runtimeDefaults
    // `serviceTier` (null = profile config); cutex/session/defaults/update
    // accepts it (`fast` is stored as `priority`; Claude sessions refuse).
    // Clients toggle a running thread with native thread/settings/update
    // {serviceTier} on a model whose model/list serviceTiers offer it.
    ("session-fast-mode", "sessionFastMode"),
    // 2026-10-15: new agents default to the composer's auto mode
    // (workspace-write + on-request + network on) when the agent new-thread
    // defaults are not stored (/v2/settings newThreadDefaultsBuiltin.agent).
    // One-time decision per home at the first cutex start of this release
    // (archive/fresh-agent-permissions/): a home that existed before (config,
    // accounts or session store present) gets the old danger-full-access +
    // never + network off stored explicitly; a fresh home stores nothing.
    ("fresh-agent-auto-permissions", "freshAgentAutoPermissions"),
    // 2026-10-15: notification-levels presets: `legacy` is no longer offered
    // (eventPresets / methodPresets, PATCH refuses it as unknown) and absent
    // means `default` for both tables; presets are stored explicitly. One-time
    // migration archive/notification-presets-no-legacy/ stores `default` for
    // a table that was absent or `legacy` (the owner's cells are kept). The
    // owner_notify delivery / bigAlertSound settings no longer drive any
    // notification. effective.legacyNeedsYouRequiresChannel is always false.
    ("notification-presets-no-legacy", "notificationPresetsNoLegacy"),
    // 2026-10-15: PATCH /v2/profiles/{p} planLabel also on Claude profiles
    // (<= 40 chars; null clears plan_label only: a Claude profile's legacy
    // plan_type is never shown). Claude items show the manual label
    // (planLabelSource "manual", detail.planLabelManual,
    // detail.editable.planLabel true); there is no live account plan. Older
    // hosts answer 400 "planLabel applies to Codex profiles only".
    ("profile-plan-label-claude", "profilePlanLabelClaude"),
    // 2026-10-15: PATCH /v2/client-tokens/{id} {name: string|null} (owner;
    // trimmed, 1..=60 chars, no control characters; null / blank clears)
    // sets the owner-given display name `ownerName` on a client / device
    // token record (tokens.json); GET /v2/client-tokens and GET /v2/users
    // tokens carry `ownerName` (null = none). The label never changes.
    ("client-token-rename", "clientTokenRename"),
    // 2026-10-15: GET /v2/capabilities `user.token` = the client / device
    // token the request authenticated with: {id, label, deviceName,
    // ownerName, platform} for owners, members and pending devices; null for
    // the root credential (no client token).
    ("user-own-token", "userOwnToken"),
    // 2026-10-15: GET /v2/users/me/devices → {hostId, userId, devices[]}
    // (the client tokens bound to the caller's user, /v2/users token items +
    // `current`) and DELETE /v2/users/me/devices/{tokenId} → {tokenId,
    // revoked: true, current} (revokes one of them; another user's token or
    // an unknown id: 404 client_token_not_found). Owner (root or an
    // owner-bound token) = the owner user's devices; a pending device: 403
    // user_pending.
    ("user-own-devices", "userOwnDevices"),
    // 2026-10-15: extra usage source kind `zcode` (usage/external_zcode.rs,
    // docs/usage-pricing.md §6): POST /v2/usage/sources {kind: "zcode", path:
    // <ZCode home, ~/.zcode>}; the scanner reads only the token / id columns
    // of completed rows of <path>/cli/db/db.sqlite `model_usage` over a
    // read-only connection. GET /v2/usage/sources kinds / defaults list it.
    ("usage-source-zcode", "usageSourceZcode"),
    // 2026-10-15: host event cutex/sessions/changed {added, removed, changed,
    // projectsChanged, hostId} under cutexSessionId `cutex.host` (owner only)
    // from a Management watcher of the session and Agent Management stores
    // (sessions_watch.rs): writes of other processes (agent_create in the
    // Agent Bus service) now reach live event streams.
    ("sessions-changed-event", "sessionsChangedEvent"),
    // 2026-10-15 (owner decision): agent_create / member_replace `task` is a
    // delegation from the calling agent to the new agent (asks/create_task.rs):
    // the agent starts idle, then the task goes through agent_delegate, so its
    // questions, progress and result return to the creator. The result gains
    // `task_mode: "delegation"` and `delegation` (the agent_delegate reply:
    // delegation id, state, created, detail; or {ok: false, code, detail,
    // note} when the agent exists but the task was not delegated). Argument
    // `task_mode: "message"` keeps the plain start message (`first_message`).
    // Human-reviewed bootstrap intents keep their reviewed start message.
    ("agent-create-task-delegation", "agentCreateTaskDelegation"),
    // 2026-10-16 (owner decision): Claude Code safety checks under full
    // access (docs/safety-grants.md, safety_checks.rs). The adapter declines
    // them at once and reports a safety block; cutex keeps it as an owner
    // notice (origin system, systemKind safetyBlock, field safetyBlock;
    // event system.safetyBlock, needsYou by default). Root-only session
    // methods cutex/safetyCheck/grant {blockId} (one exact retry, used once,
    // 30 minutes; the agent gets the owner input 主人已批准一次：…) and
    // cutex/safetyCheck/dismiss {blockId}; GET /v2/safety-checks
    // ?state=open|all&agentId=&limit=. Live via cutex/ownerNotify/created|updated.
    ("claude-safety-grants", "claudeSafetyGrants"),
    // 2026-10-16: Windows sandbox readiness. Native `windowsSandbox/readiness`
    // (no params; `{status: ready|notConfigured|updateRequired}`) on a
    // session (POST /v2/sessions/{id}/app-server/requests: that runtime's native
    // home) and on POST /v2/codex/requests (the shared home new agents adopt),
    // plus `windowsSandbox/setupStart` with exactly `{mode: "unelevated"}`
    // (no cwd; elevated setup = UAC, local sandbox users, firewall rules:
    // refused, 400). Owner only. The host route answers after native's
    // `windowsSandbox/setupCompleted` (`result.completed {mode, success,
    // error}`, null on timeout); the session route answers `{started}` and the
    // completion arrives as that session's native event. Running threads keep
    // the sandbox level they started with: the agent needs a relaunch.
    ("windows-sandbox-readiness", "windowsSandboxReadiness"),
    // 2026-10-16 (owner decision): the desktop's 「自动」 mode is
    // workspace-write + on-request + the native automatic reviewer. POST
    // /v2/human/agents and /v2/human/sessions `launch.approvalsReviewer`
    // ("user" | "auto_review"; legacy "guardian_subagent" = auto_review) is
    // stored on the session record, sent as native `approvalsReviewer` on
    // thread/start and every start / resume (`-c approvals_reviewer` for owner
    // launches), and mirrored from thread/settings/updated so a running
    // change survives relaunch. /v2/settings newThreadDefaults.<kind> gains
    // `approvalsReviewer` (built-in `auto_review` for both kinds; null =
    // built-in). Without this feature `launch` refuses the field (unknown
    // field) and new threads keep the profile's reviewer (native `user`).
    ("launch-approvals-reviewer", "launchApprovalsReviewer"),
    // Browser relay (desktop 0.1.3): the desktop may report its precise
    // refusal reasons — external_pages_off, external_proxy_unavailable,
    // private_address_blocked, reserved_address, protected_endpoint,
    // remote_loopback — instead of external_pages_unavailable (each with a
    // fixed agent hint).
    ("browser-refusal-reasons", "browserRefusalReasons"),
    // Browser relay grant source {kind:"popup", openerLeaseId}: a tab the
    // leased agent's own click opened is granted to the same agent without a
    // new owner approval, only while the opener lease is active for that agent
    // on the same device connection; it inherits the opener lease's origin.
    ("browser-popup-grant", "browserPopupGrant"),
    // 2026-10-18: a browser_act wheel scroll result may carry
    // `scroll {dx, dy, atEdge}`: CSS px the scrolled container moved and
    // whether it now sits at the edge in the requested direction (e.g. the
    // bottom for `down`). The desktop sends it only when advertised.
    ("browser-scroll-outcome", "browserScrollOutcome"),
    // 2026-10-16: Kimi Code membership plan. Quota reader `kimi_code_plan`
    // (quota-config `presetOptions`): GET <profile base URL>/usages on the
    // Kimi Code API (api.kimi.com / api.kimi.ai `/coding/v1`, Bearer key),
    // 5 h window `limits[0]`; quota mode `used_ratio` (used share 0..1) and
    // reader config `optional` (a window never read is not reported).
    // API-key presets `kimi-code` (api.kimi.ai) / `kimi-code-cn`
    // (api.kimi.com): family `kimi-code`, Kimi Code catalog (k3, k3-256k,
    // kimi-for-coding, kimi-for-coding-highspeed), web_search live; a new
    // profile starts with the reader plus optional `week` (legacy plans) and
    // `month` (monthly total) extras.
    ("kimi-code-plan", "kimiCodePlan"),
    // 2026-10-16: copy a profile to another host (owner only, POST only).
    // POST /v2/profiles/{p}/export {includeCredentials?: bool} -> {bundle,
    // credentialsIncluded, credentialKind}: bundle schemaVersion 1 (settings,
    // config.toml profile part, effective model catalog + overlay). With
    // includeCredentials (loopback connection only, else 403
    // `credentials_loopback_only`) an API-key profile's key and settings
    // secrets are included; a ChatGPT login or Claude account credential never
    // is. Each export / import is audited without values.
    // POST /v2/profiles/import {bundle, name} -> 201 profile detail +
    // credentialsCopied + warnings [{code: needsLogin | claudeAccountRebound |
    // privateAuthReset | runtimeDockerImage | proxyCleared | proxyCopied |
    // secretFieldsCleared, ...}]; never overwrites (409 profile_name_conflict);
    // 400 unsupported_bundle for a bundle this cutex does not read.
    ("profile-transfer", "profileTransfer"),
    // 2026-10-17: network access and extra writable roots of a running
    // workspace-write Codex session (sandbox_extras.rs). Owner only.
    // GET /v2/sessions/{id}/sandbox-extras -> {cutexSessionId, sandbox,
    // networkAccess, writableRoots, editable, reason}; PATCH with
    // {networkAccess?, writableRoots?} stores them on the session record
    // (kept across runtime restart, relaunch and daemon restart) and sends
    // native thread/settings/update when the runtime is connected; the answer
    // adds `changed` and `live {status: applied | offline | failed,
    // diagnostic?}` (applied = from the next turn). Refusals: 409
    // sandbox_not_workspace_write | unsupported_runtime (Claude) |
    // session_retired; 400 invalid_writable_root (relative, missing, not a
    // directory, > 32) | writable_root_in_cutex_home.
    ("session-sandbox-extras", "sessionSandboxExtras"),
    // 2026-10-17: persistent "always allow" rules of Claude agents, per
    // workspace (claude_permission_rules.rs; runtime/claude/permission_rules.rs).
    // GET|PUT /v2/sessions/{id}/claude-permission-rules -> {cutexSessionId,
    // workspace, revision, updatedAt, allow, supported} (+ PUT `sessions:
    // [{cutexSessionId, status: live | nextStart | unsupported}]`); PUT body
    // {allow, expectedRevision?}; 409 unsupported_runtime / revision_conflict,
    // 400 invalid_rule. An approval answered with `{"decision":
    // {"acceptWithClaudeRules": {"rules": [...]}}}` (rules offered in the
    // request's `claudePermissionRules`) adds them. Owner only.
    ("claude-permission-rules", "claudePermissionRules"),
    // 2026-10-17 (owner decision, claude-agent-gaps Q1 / Q3): Claude Code's
    // own settings sources and the host's MCP layers for Claude agents
    // (runtime/claude/code_sources.rs, profiles/claude_code.rs). Profiles
    // (Claude): `claudeSettingSources` (effective, ["project","user"] subset;
    // null on Codex), `claudeAiConnectors`; detail
    // `claudeSettingSourcesExplicit` (null = default) / `…Default` ([]) /
    // `…Options`; PATCH `claudeSettingSources` (list or null) and
    // `claudeAiConnectors` (bool). Existing profiles keep no sources; Claude
    // profiles created from now on start with ["project"]. `user` = the
    // Claude account home, never ~/.claude. Claude agents also get the
    // merged global / project / agent MCP servers (stdio + http; secrets in
    // the adapter env only). GET /v2/sessions/{id}/claude-code -> {next,
    // launched, servers [{name, layers, transport, status included |
    // disabled | skipped, reason, ignoredFields}], adapterSupports,
    // restartNeeded}; 409 unsupported_runtime for Codex agents. Applies at
    // the next runtime launch; adapter-gated (features.json settingSources /
    // mcpServers / claudeAiConnectors).
    ("claude-code-sources", "claudeCodeSources"),
    // 2026-10-17: Claude P0. GET /v2/sessions/{id}/skills (root) -> the
    // Codex skills/list shape for a Claude agent's managed roots (workspace
    // .agents/skills + .claude/skills, account cutex-skills, host
    // ~/.cutex/claude-skills), read without the runtime; Codex agents 409
    // unsupported_runtime. Clients send the listed paths as `skill` inputs.
    ("claude-session-skills", "claudeSessionSkills"),
    // 2026-10-17: cutex/session/history/revert accepts `rewindFiles: true`
    // for Claude agents (the adapter restores files from Claude Code's
    // checkpoint of the first removed turn; Codex refuses with
    // rewind_files_unsupported). Claude file checkpointing now defaults on
    // (profile claudeFileCheckpointingDefault true).
    ("claude-rewind-files", "claudeRewindFiles"),
    // 2026-10-17: POST /v2/human/agents and /v2/human/sessions accept
    // `launch.claudePermissionMode` (default | acceptEdits | plan | auto |
    // bypassPermissions) for Claude profiles: the agent starts in that Claude
    // Code permission mode (sent once at its first runtime start; plan = plan
    // mode over the adapter's default). Codex profiles and modes the selected
    // Claude runtime does not offer answer 400 unsupported_launch_option.
    // Profiles (Claude) list the offered modes as `claudeLaunchPermissionModes`.
    ("claude-launch-permission-mode", "claudeLaunchPermissionMode"),
    // 2026-10-17: owner-only import of Claude Code skills into the host's
    // shared ~/.cutex/claude-skills (management/v2/claude_skills_import.rs).
    // GET /v2/claude/skills/import[?source=<abs dir>] -> {source,
    // defaultSource, sourceExists, target, skills [{name, title, description,
    // bytes, files, exists, importable, reason?}], truncated}; POST {source?,
    // names, overwrite?} -> {imported, skipped [{name, reason}], failed}.
    // Default source = the host user's ~/.claude/skills. Copies only (never
    // touches the source); symlinks leading out of the source are refused.
    ("claude-skills-import", "claudeSkillsImport"),
    // 2026-10-17: Claude P2. A Claude agent's foreground subagent transcripts
    // (adapter features.subagentTranscripts): native `thread/read` on the
    // `claude:<session>:task:<id>` reference of a subAgentActivity item passes
    // the sub-agent lineage check (the adapter answers `parentThreadId`), and
    // the adapter's live frames for it are published stream-only on the
    // parent session (native threadId intact) once the parent's activity item
    // named it. Without this capability reads still work, frames do not.
    ("claude-subagent-transcripts", "claudeSubagentTranscripts"),
    // 2026-10-17: native `thread/searchOccurrences` {threadId, searchTerm,
    // cursor?, limit? 1..200} is forwarded for the bound thread (read grant;
    // never a sub-agent thread; `thread/search` stays denied). Codex natives
    // 0.160+ and Claude adapters (features.threadSearch) answer {data
    // [{turnId, itemId, snippet, snippetMatchRange {start, end} (UTF-16),
    // turnCursor}], nextCursor}; older natives answer method-not-found.
    ("thread-search-occurrences", "threadSearchOccurrences"),
    // 2026-10-17: host proxy modes. /v2/settings `proxy` = {mode: system | none
    // | manual, url (credentials redacted), urlHasCredentials, noProxy,
    // loopbackNoProxy (always bypassed, not editable), system {url, source:
    // environment | windowsSystem | macosSystem} | null (what "follow system"
    // resolves to now; Windows: env, then registry ProxyServer / ProxyOverride;
    // macOS: env, then `scutil --proxy` HTTPS / HTTP + ExceptionsList; no PAC),
    // editable, warning}; PATCH {mode, url?, noProxy?} (omitted url keeps the
    // stored one). Also the proxy of cutex's own requests (upgrades,
    // components, LiteLLM catalog, webhooks); loopback always direct; SOCKS
    // only reaches runtimes. A 407 reads "the proxy requires authentication".
    // Changing it needs the root credential: a client / device token gets 403
    // proxy_owner_credential_required. `forceHttpTransport` is retired (400).
    ("proxy-modes", "proxyModes"),
    // 2026-10-17: /v2/profiles items `proxy` = {mode: inherit | none | manual,
    // url (redacted), urlHasCredentials, noProxy, effective {mode, url,
    // source}, editable}; PATCH `proxy` {mode, url?, noProxy?} | null
    // (= inherit). Used by the Profile's runtimes (Codex and Claude, new
    // runtimes only), quota readers, model discovery and web_search. Same
    // root-credential rule as proxy-modes.
    ("profile-proxy", "profileProxy"),
    // 2026-10-17: agent service_register / service_update validate the whole
    // request before raising any owner approval (an invalid request never
    // reaches the owner). Static roots: inside the caller's cwd / project
    // workspace (or its own cutex-managed scratch cwd under agents-scratch /
    // chats) need nothing; any other existing root outside the cutex home
    // raises an approval (kind static_root, or folded into the
    // service_lifetime approval of a persistent request; details.staticRoot
    // = the canonical root, allowedRoots); the cutex home, filesystem roots
    // and the user home (or anything containing it or the cutex home) stay
    // 403 static_root_forbidden, whose detail now lists the allowed roots.
    ("static-root-approval", "staticRootApproval"),
    // 2026-10-17: agent tool service_restart {slug}: the owning agent restarts
    // its running managed service with the unchanged command (no approval;
    // external / static 409 not_managed, owner-stopped 409 service_stopped).
    // The Bus records `restartRequestedAt` on the registry record and the
    // supervisor restarts it like POST /v2/services/{slug}/restart.
    // service_update no longer asks again when the effective command, cwd,
    // env and runtime are unchanged.
    ("services-agent-restart", "servicesAgentRestart"),
    // 2026-10-18: session resource activity.lastConversationAt (sessionActivity
    // version 3, also activity.lastTurnStartedAt): the last real conversation
    // activity (turn started, output, tool call, file change, a turn ended
    // other than interrupted), persisted in session-activity-state.json.
    // Runtime start / stop / restart, reconnects and session metadata never
    // move it. /v2/summary agents[].lastActivityAt now follows it (then a
    // non-interrupted lastTurn end, then createdAt) instead of the runtime's
    // lastEventAt / updatedAt.
    ("session-conversation-activity", "sessionConversationActivity"),
    // 2026-10-18 (owner Q26): the desktop's "upgrade runtime" action for a
    // Claude agent, the HTTP form of `cutex human claude repin <id>`
    // (management/v2/claude_runtime_api.rs, rules in runtime/claude_pin.rs).
    // GET /v2/sessions/{id}/claude-runtime (root) -> {current {release,
    // missing}, pending {release, requestedAt} | null, selected {release} |
    // null, selectedError, direction upgrade | downgrade | change | none,
    // upgradeAvailable, dryRun {outcome applied | deferred | unchanged |
    // skipped, reason, restartRequired} | null}; POST
    // /v2/sessions/{id}/claude-runtime/repin {target?} (owner only) ->
    // {result {outcome, reason, restartRequired, previous, target}, runtime}.
    // A running agent is never restarted: deferred = applies at its next
    // fresh runtime start. 409 claude_repin_refused (details.reason, nothing
    // changed), claude_runtime_changed, claude_runtime_not_installed,
    // unsupported_runtime, remote_session. Schema export `claude-runtime`.
    ("claude-runtime-repin", "claudeRuntimeRepin"),
    // 2026-10-18: the repin offer without a per-agent request. Claude session
    // resources carry runtime.claudeRuntime {release, pendingRelease | null}
    // (absent on other sessions; in single sessions, list items and archived
    // ones) and GET /v2/sessions carries claudeRuntime {selectedRelease |
    // null}, the release for new Claude sessions. cutex/sessions/changed
    // names a session whose pin or pending repin changed (applied, deferred,
    // applied at start, dropped) and sets claudeRuntimeChanged when the
    // selected release changed (`cutex human claude install`), so a client
    // that reloads the list on that event stays current.
    ("session-claude-runtime", "sessionClaudeRuntime"),
    // 2026-10-18: Profile looks on the host, the same on every device
    // (crate::profiles::avatar). GET|PUT|DELETE /v2/profiles/{p}/avatar
    // (PUT {contentType, data}: square PNG / JPEG / WebP <= 48 KiB, 16..=512
    // px, like user avatars) and /v2/profiles/{p}/intro-portrait (<= 2 MiB,
    // each side <= 1024 px); GET answers the image with an ETag
    // (If-None-Match -> 304). Profile items carry avatarEtag /
    // introPortraitEtag; appearance.avatarThemes (non-empty unique known
    // ids, detail.avatarThemeOptions; null = default by model family) and
    // appearance.introGreeting (<= 300 chars), both PATCHable (null clears).
    // Keyed by profile id; removed with the profile. Images in
    // runtime/profile-avatars/ (never config.json / accounts.json).
    ("profile-avatar", "profileAvatar"),
    // 2026-10-19: import Profiles from CC Switch (farion1231/cc-switch; owner
    // only, crate::profiles::cc_switch, read-only). GET
    // /v2/profile-imports/cc-switch -> {found, source {path, format
    // sqlite|json, schemaVersion} | null, error {code, message} | null,
    // providers [{id, app claude|codex, name, baseUrl, model, current, hasKey,
    // supported, reason, existingProfile}]} (never a key; schema > 20 ->
    // error unsupported_version). POST {items: [{id, app, name?}]} (1-50) ->
    // {created [{id, app, profile}], failed [{id, app, code: provider_gone |
    // unsupported | no_key | profile_exists | invalid | store_unavailable,
    // message}]}; created through the POST /v2/profiles path (Codex -> apiKey
    // profile; Claude -> Claude API account + claude profile).
    ("profile-import-cc-switch", "profileImportCcSwitch"),
    // Host owner in person (cutex-desktop docs/research/host-ownership-20261019.md
    // §1): GET /v2/host `owner` {displayName, fingerprint, confirmed, source}
    // for every principal (members and pending devices included);
    // PUT /v2/users/owner-person {publicKey} and DELETE /v2/users/owner-person
    // → {owner} (owner-local only, see owner-local-only: paired devices'
    // tokens and remote listeners get 403 owner_person_local_only); persons-v1.json
    // ownerPerson {publicKey, setAt, source}; audit ownerPersonSet; CLI
    // `cutex management owner show|set-person|clear-person`.
    ("host-owner-person", "hostOwnerPerson"),
    // Owner's paired devices, limit 1 (cutex-desktop
    // docs/research/host-ownership-20261019.md §5): a pairing code for the
    // owner lives 5 minutes (others 10) and says `ownerDevice: true`; every
    // token newly bound to the owner (pairing, assignment, person-key
    // auto-binding, the CLI = via "ssh") is a new owner device: host event
    // cutex/access/ownerDeviceAdded {tokenId, label, deviceName, platform,
    // scope, at, via}; GET /v2/users/owner/new-devices → {devices[]} (owner
    // principals) and POST /v2/users/owner/new-devices/{tokenId}/ack →
    // {tokenId, acknowledged}; users-v1.json newOwnerDevices.
    ("owner-device-notice", "ownerDeviceNotice"),
    // Limit 2: root-identity actions only owner-local ("本机和 SSH 都算"): a
    // loopback listener with the root credential or with an owner token
    // issued by `cutex management client-token issue` on the host (token
    // `issuedVia: "hostCli"`; an SSH-tunnelled desktop), or the CLI; never a
    // paired device's token (`issuedVia: "pairing"`). Actions: setting the
    // owner person (403 owner_person_local_only), turning
    // ownerRequiresApproval off, creating pairing codes, pairing / enabling
    // peers (POST /v2/peers, POST /v2/peers/invites, PATCH /v2/peers/{id}
    // enabled true, PATCH /v2/settings peers.enabled true), reading
    // config.json through /v2/fs/file; else 403 owner_local_only {action,
    // clientToken, issuedVia}. /v2/capabilities user.ownerLocal says whether
    // this connection counts; token items carry issuedVia.
    ("owner-local-only", "ownerLocalOnly"),
    // Limit 3: per-device scope "full" | "chat" | "read" on client / device
    // tokens (pairing-code `scope`, `client-token issue --scope`, PATCH
    // /v2/client-tokens/{id} {scope}); effective rights = user's rights ∩
    // scope (access::device_scope_gate; 403 device_scope_denied {scope,
    // required}); token items and /v2/capabilities user.token carry `scope`,
    // user.scope = this connection's scope.
    ("device-scope", "deviceScope"),
    // Remote close of a desktop client (cutex-desktop
    // docs/research/remote-close-desktop-20261019.md; clients_api.rs):
    // POST /v2/clients/presence {installId, instanceId, appName, appVersion,
    // deviceName, platform, allowRemoteClose} -> {instanceId,
    // heartbeatSeconds: 60, state, closeRequest} (heartbeat; in memory,
    // dropped after 3 missed heartbeats; any principal, pending included);
    // GET /v2/clients -> {clients[]} (owner with scope full: all, others
    // their own user's); POST /v2/clients/{instanceId}/close {graceSeconds?}
    // -> 202 {instanceId, state} (owner or same user, scope full; 403
    // client_close_forbidden, 404 client_not_found, 409
    // remote_close_disabled; audited); POST /v2/clients/{instanceId}/closing
    // {outcome: closing|cancelled} (the registering credential only); host
    // events cutex/client/closeRequested {instanceId, requestedBy
    // {deviceName, userId, local}, graceSeconds, at} and
    // cutex/client/stateChanged {instanceId, state}.
    ("client-remote-close", "clientRemoteClose"),
    // 2026-10-20: per-user home page settings on the host (cutex-desktop
    // docs/research/home-page-20261019.md §1; management::user_home):
    // GET /v2/users/me/home -> {revision, home: {...}|null, coverEtag,
    // updatedAt}; PUT {revision, home} (optimistic concurrency, 409
    // home_revision_conflict {revision}; home <= 64 KiB JSON, known keys
    // layout / blocks / bio / sidebarEntry / showWhenEmpty / homeHost / pins
    // checked, unknown keys kept; 400 invalid_home, 413 home_too_large);
    // GET|PUT|DELETE /v2/users/me/home/cover (PUT {contentType, data}: PNG /
    // JPEG / WebP <= 2 MiB, each side <= 4096 px; GET with ETag). Own record
    // only (owner or a bound user; pending 403 access_pending); device scope
    // read may only read. Stored in runtime/users/home/.
    ("user-home", "userHome"),
    #[cfg(target_os = "linux")]
    ("job-service", "jobService"),
];

pub fn capabilities_resource() -> Value {
    capabilities_with_jobs(super::human_jobs::cached_capabilities())
}

fn capabilities_with_jobs(jobs: super::human_jobs::JobCapabilities) -> Value {
    let mut features = serde_json::Map::new();
    for (_, key) in MANAGEMENT_FEATURES {
        features.insert((*key).to_string(), Value::Bool(true));
    }
    let mut feature_list = MANAGEMENT_FEATURES.iter().map(|(name, _)| *name).collect::<Vec<_>>();
    if cfg!(target_os = "linux") && jobs.external {
        features.insert("jobExternal".into(), Value::Bool(true));
        feature_list.push("job-external");
    }
    // Verified mail only triggers scheduler verification: without the optional
    // `nci-qstat` build nothing can complete an NCI job, so the source is not advertised.
    if cfg!(all(target_os = "linux", feature = "nci-qstat")) && jobs.nci {
        features.insert("jobSourceNci".into(), Value::Bool(true));
        feature_list.push("job-source-nci");
    }
    #[cfg(all(target_os = "linux", feature = "nci-qstat"))]
    {
        features.insert("nciVerification".into(), Value::Bool(true));
        feature_list.push("nci-verification");
        // `POST /v2/nci-verification/key` (host-generated qstat key, never returned),
        // `GET /v2/nci-verification/helper`, `useGeneratedKey` in the config.
        features.insert("nciHostKeygen".into(), Value::Bool(true));
        feature_list.push("nci-host-keygen");
    }
    #[cfg(target_os = "linux")]
    {
        features.insert("inboundMail".into(), Value::Bool(true));
        feature_list.push("inbound-mail");
        // `GET /v2/inbound-mail` reports `address` / `nciMailTo*` and the config
        // accepts `address`; the effective address is what `prepare_remote` hands agents.
        features.insert("nciMailAddress".into(), Value::Bool(true));
        feature_list.push("nci-mail-address");
    }
    json!({
        "contractVersion": 2,
        // Build identity (build_info.rs): {version (Cargo), release, commit,
        // builtAt, profile}. Replaces cutexVersion / gitSha (r224).
        "cutexBuild": crate::build_info::build_json(),
        "hostId": crate::platform::host::current_host_name(),
        // Random per cutex home; clients key a daemon by it (hostId is the
        // machine name and is shared by every cutex home on the machine).
        "instanceId": crate::management::instance::instance_id(),
        "instance": crate::config::instance::current_name()
            .map(|name| json!({ "name": name, "isolated": true })),
        "cutexMethodRegistryVersion": super::session::CUTEX_METHOD_REGISTRY_VERSION,
        "cutexMethods": super::session::CUTEX_METHOD_REGISTRY_METHODS,
        "features": features,
        "featureList": feature_list,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_advertise_registry_and_features() {
        let value = capabilities_resource();
        assert_eq!(value["contractVersion"], 2);
        assert_eq!(value["cutexBuild"], crate::build_info::build_json());
        assert_eq!(value["cutexBuild"]["version"], env!("CARGO_PKG_VERSION"));
        assert!(value.get("cutexVersion").is_none() && value.get("gitSha").is_none());
        assert!(value["cutexMethods"]
            .as_array()
            .unwrap()
            .iter()
            .any(|method| method == "cutex/session/metadata/update"));
        for method in ["cutex/session/delete", "cutex/session/fork"] {
            assert!(value["cutexMethods"].as_array().unwrap().iter().any(|m| m == method));
        }
        for key in [
            "sessionConversationActivity",
            "proxyModes",
            "profileProxy",
            "includeHidden",
            "sessionMetadata",
            "agentAvatarColor",
            "personIdentity",
            "usersSelfProfile",
            "projectWorkspaceRoot",
            "humanCreateAgent",
            "humanAgentClaude",
            "humanAgentScratchCwd",
            "claudeToolDefaults",
            "imExposureRetired",
            "profileQuota",
            "profilesApi",
            "settingsApi",
            "userInstructions",
            "profileSwitch",
            "sessionHistoryRevert",
            "sessionFilesRevert",
            "profileModels",
            "projectMemory",
            "usageLedger",
            "workspaceFs",
            "workspaceGit",
            "terminal",
            "codexConfig",
            "codexPlugins",
            "codexConfigOverlay",
            "scopedMcp",
            "subagentThreads",
            "offlineHistory",
            "historyPages",
            "offlineSubmit",
            "profileLogin",
            "services",
            "servicesV2",
            "servicesProxy",
            "servicesLifetime",
            "servicesPersistentApproval",
            "sessionDelete",
            "sessionDeleteEmptyOnArchive",
            "sessionFork",
            "notes",
            "sideChat",
            "profileCreate",
            "profileDuplicate",
            "profileDelete",
            "profileRename",
            "profileSwitchFamily",
            "agentBusPerAgentAuth",
            "agentProjectBoundary",
            "agentExternalGrants",
            "agentToolRoles",
            "ownerApprovals",
            "approvalsLive",
            "approvalDecisionNotice",
            "profileNativeSubagents",
            "profileAgentApproval",
            "profileAgentTurnQuestions",
            "profileReasoningSummary",
            "profileUpdatePlan",
            "profileWebSearch",
            "profileToolSchemaCompat",
            "profilePresetsV2",
            "nativeR53",
            "profileSearchTool",
            "sessionLatestPlan",
            "quickChatDefaultProfile",
            "claudeRuntimeFeatures",
            "managementClientTokens",
            "sessionAttachments",
            "fsDirBrowse",
            "agentAsks",
            "askGate",
            "askArtifacts",
            "bootstrapTokenUsage",
            "agentMessagesV2",
            "delegationViews",
            "runtimePackageUpgrade",
            "ownerMessages",
            "ownerNotifyDelete",
            "agentBusStuckMessages",
            "queueIdleStart",
            "queueReorder",
            "admissionOrder",
            "inputOverflowDocuments",
            "recordArchive",
            "sessionStoreRetention",
            "notificationLevels",
            "profileProviderCategoryV2",
            "usagePriceCatalog",
            "usageCurrency",
            "usageExternalSources",
            "usageModelMap",
            "profileRenameLive",
            "runtimesView",
            "agentHarness",
            "projectOptionalDirector",
            "taskServiceRemoved",
            "claudeAccounts",
            "wakeBusyWait",
            "profilePlanLabel",
            "profilePlanLabelOverride",
            "artifacts",
            "artifactShow",
            "claudeQuotaStaleCredentials",
            "peers",
            "crossHostAsks",
            "crossHostTell",
            "crossHostTellWake",
            "peersV2",
            "browserRelayV1",
            "skillsArchive",
            "projectCreateWorkspaceRoot",
            "profileDeleteDetachArchived",
            "sessionFastMode",
            "freshAgentAutoPermissions",
            "notificationPresetsNoLegacy",
            "profilePlanLabelClaude",
            "clientTokenRename",
            "userOwnToken",
            "userOwnDevices",
            "usageSourceZcode",
            "windowsSandboxReadiness",
            "peerOwnerProfile",
            "sessionSandboxExtras",
            "profileAvatar",
            "profileImportCcSwitch",
            "hostOwnerPerson",
            "ownerDeviceNotice",
            "ownerLocalOnly",
            "deviceScope",
            "clientRemoteClose",
            "userHome",
        ] {
            assert_eq!(value["features"][key], true, "{key}");
        }
        for removed in ["sessionTasks", "taskDetail"] {
            assert!(value["features"].get(removed).is_none(), "{removed}");
        }
        let list = value["featureList"].as_array().unwrap();
        assert!(list.iter().any(|name| name == "task-service-removed"));
        assert!(list.iter().any(|name| name == "wake-busy-wait"));
        for name in ["notes", "side-chat", "queue-reorder", "admission-order", "stop-holds-ridealongs", "admission-owner-yield", "admission-ridealong-detail"] {
            assert!(list.iter().any(|listed| listed == name), "{name}");
        }
        assert!(list.iter().any(|name| name == "artifacts"));
        assert!(list.iter().any(|name| name == crate::artifacts::show::CAPABILITY));
        assert!(list.iter().any(|name| name == "browser-relay-v1"));
        assert!(list.iter().any(|name| name == "browser-idempotency-clear"));
        assert!(!list.iter().any(|name| name == "session-tasks" || name == "task-detail"));
    }

    #[test]
    fn nci_verification_requires_the_optional_build_feature() {
        let value = capabilities_with_jobs(crate::management::v2::human_jobs::JobCapabilities::default());
        let enabled = cfg!(all(target_os = "linux", feature = "nci-qstat"));
        assert_eq!(value["features"]["nciVerification"].as_bool().unwrap_or(false), enabled);
        assert_eq!(value["featureList"].as_array().unwrap().iter().any(|v|v == "nci-verification"), enabled);
        assert_eq!(value["featureList"].as_array().unwrap().iter().any(|v|v == "nci-host-keygen"), enabled);
    }

    #[test]
    fn inbound_mail_is_discoverable_without_configuration() {
        let value = capabilities_with_jobs(crate::management::v2::human_jobs::JobCapabilities::default());
        assert_eq!(value["features"]["inboundMail"].as_bool().unwrap_or(false), cfg!(target_os = "linux"));
        assert_eq!(value["featureList"].as_array().unwrap().iter().any(|v| v == "inbound-mail"), cfg!(target_os = "linux"));
    }

    #[test]
    fn external_source_capability_requires_an_enabled_linux_runner() {
        for external in [false, true] {
            for nci in [false, true] {
                let value = capabilities_with_jobs(crate::management::v2::human_jobs::JobCapabilities { external, nci, notify_file: false });
                let features = value["featureList"].as_array().unwrap();
                assert_eq!(features.iter().any(|v| v == "job-external"), external && cfg!(target_os = "linux"));
                assert_eq!(value["features"]["jobExternal"].as_bool().unwrap_or(false), external && cfg!(target_os = "linux"));
                // Mail cannot complete NCI jobs without the optional verifier build.
                let nci_source = nci && cfg!(all(target_os = "linux", feature = "nci-qstat"));
                assert_eq!(features.iter().any(|v| v == "job-source-nci"), nci_source);
                assert_eq!(value["features"]["jobSourceNci"].as_bool().unwrap_or(false), nci_source);
            }
        }
    }

    #[test]
    fn job_service_feature_is_advertised_only_where_a_runner_exists() {
        let value = capabilities_resource();
        let listed = value["featureList"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "job-service");
        assert_eq!(listed, cfg!(target_os = "linux"));
        assert_eq!(
            value["features"]["jobService"].as_bool().unwrap_or(false),
            cfg!(target_os = "linux")
        );
    }
}
