# Changelog

## 0.2.0 "Beautiful World" (2026-10-29)

- Improves persistent agent communication, delegation and project services.
- Adds account-aware Profiles and runtime variants, native runtime 0.164.0-r58,
  and Claude runtime claude-r32 with bundled Python and pinned dependencies.
- Kimi and DSH are experimental and are not included in the 0.2.0 binaries; later public update.
- Upgrading from 0.1.5 changes durable agent communication to format v2.
  This is a one-way update: older releases require an offline restoration;
  automatic rollback across the migration is unavailable.

- 改进持久化智能体通信、委托和项目服务。
- 支持按账户管理的 Profile 和运行时变体，提供原生运行时 0.164.0-r58，
  Claude 运行时 claude-r32 自带 Python 和固定依赖。
- Kimi 和 DSH 为实验功能，不包含在 0.2.0 二进制发行中，随后续公开更新提供。
- 从 0.1.5 升级会将持久化智能体通信改为格式 v2。
  这是不可自动回退的更新；使用旧版本前须先离线恢复数据。

- Kimi kimi-r2 known limits: Linux lacks the node-pty binding, so interactive terminal tools are unavailable; offline Glob/Grep require system ripgrep (`rg`).

<!-- Each entry: English text, then a Chinese text. The English part contains no Chinese characters:
     name UI labels by their English strings (Approvals, "Auto"). -->

## r262

- Completed asks can move transactionally into cold history in the existing
  database, keeping admission and scheduling focused on active records. History
  remains readable. The persisted cutoff does not advance automatically. This
  introduces asks format 2: rollback to r259/r260/r261 requires stopping every
  writer and running the outgoing binary's asks-archive-restore command first.
  已结束的 asks 可事务性归档到原数据库，输入准入和调度只处理热数据，历史仍可读取；截止时间固定保存，不自动推进。asks 格式升为 2，回退 r259/r260/r261 前须停下全部写入者并用新版执行恢复命令。

- DSH is an optional installed component. Capabilities reflect installation;
  standard host kits omit its npm payload and Node preparation. Explicit install
  and installed-only updates preserve existing sessions, versions and history.
  Native r52.4 and Claude claude-r29 are unchanged.
  DSH 改为可选安装组件，能力随安装状态显示；标准安装包不再携带 DSH npm 内容或准备 Node。显式安装与已安装组件更新保留原有会话、版本和历史；Native r52.4、Claude claude-r29 不变。

## r261

- Concurrent ask-store readers share one cold-generation refresh instead of rebuilding the
  retained state independently. Input admission lease checks use a shared snapshot without
  copying the whole ask store, retaining the existing hot-only recovery behavior.
  并发读取 asks 时，同一代数据只重建一次，减少大历史带来的 CPU 和写锁等待；输入准入只读共享快照，保留 hot-only 恢复行为。

- Newly created Gemini profiles leave strict tool-schema compatibility off by default; existing
  profile settings are preserved. Recognized terminal Google/Gemini tool-schema refusals carry
  the additive `cutexCode: "tool_schema_rejected"` presentation hint in events and history.
  新建 Gemini Profile 默认不启用严格工具 schema 模式，已有 Profile 保持原设置；已识别的 Google/Gemini 工具 schema 拒绝带上桌面可识别的提示码。

- Approval Rules lists reuse a bounded in-memory projection, refreshed by approval writes and
  invalidated by file identity or change metadata. The list response and rule expiry behavior
  are preserved, with no persistent format migration.
  自动审批规则列表使用有界内存快照，审批写入后刷新，文件变更时失效；返回格式和规则到期行为保持一致，无需迁移存储格式。

## Unreleased

- Claude runtime claude-r29 (adapter 0.8.3) preserves streamed item identity through corrected
  full-text terminal events. It ships a pinned standalone Python and, for the r260 release build,
  Anthropic's unmodified SDK-matched Claude Code CLI by default. The reviewed build switch remains:
  `CUTEX_CLAUDE_BUNDLE_CLI=0` produces a CLI-free archive, while an omitted value or `1` bundles the
  hash-pinned platform wheel. Generated notices keep the Claude Code binary under Anthropic's
  Commercial Terms and legal/compliance documentation. `PATCH /v2/settings` with
  `{"claudeCli":"bundled"}` or `{"claudeCli":"system"}` selects the source for new Claude sessions,
  and the `claude-runtime` row from
  `GET /v2/components` reports only the selected source and bounded bundled/system versions through
  its public `claudeCli` extension; diagnostic executable paths remain under `detail`.
  Claude 运行时 claude-r29（适配器 0.8.3）修复流式条目在完整终态中的身份与内容；它自带独立 Python，r260 发布包默认也带上与 SDK 匹配、未经修改的 Claude Code CLI，仍可用 `CUTEX_CLAUDE_BUNDLE_CLI=0` 关闭。设置可为新会话选择内置或系统 CLI，组件状态按桌面协议报告来源与版本。

- Claude auto-compaction per profile (capability `claude-compaction`, cutex-desktop docs/design 036): the profile
  item carries `claudeCompaction: {window, percent} | null` (detail: `editable.claudeCompaction`,
  `claudeCompactionError` for a file the launch would refuse) and `PATCH /v2/profiles/{p}` takes
  `claudeCompaction: {window: 100000..1000000, percent: 1..100} | null`. Stored in the existing
  `profiles/<id>/claude-runtime.json` (same file shape as before, so older binaries keep reading it; null removes the
  file and Claude Code decides again). Applies at each agent's next runtime start. Per-model values are not offered:
  a model switch inside a running Claude agent keeps the launch environment.
  Claude 自动压缩可以在 Profile 设置里改（窗口 / 百分比），下次启动运行时生效。

- Browser round trips (cutex-desktop docs/design 033): `browser_navigate` and `browser_open` take an optional
  `snapshot: true` and then also return the page's `browser_read` snapshot (fresh refs) in the same result; new
  `browser_visit {title, guestId, urls[1..10]}` navigates one leased tab to each URL in turn and reads it (snapshots
  share a 128 KiB budget, cut ones are marked `truncated`; stops at the first failure; only the last page's refs stay
  valid). Both are composed in the MCP façade from the existing wire commands - each step is an ordinary
  `/v2/browser/commands` request with its own request id derived from the outer call, so per-tab admission, the
  network policy (N2), owner approvals and leases apply to every step exactly as before; Management and the desktop
  never see a new command (the façade option and the composite tool fail Management's wire validation).
  浏览器：打开 / 跳转可以顺带返回页面快照；新工具 browser_visit 一次按顺序读多个网址，减少模型往返。

- agent_create states an effort fallback (capability `agent-create-effort-fallback`): when the
  profile default reasoning effort is not offered by the model and another level is used, the reply
  carries `effort_fallback {model, requested, used, reason, note}` and `POST /v2/human/agents`
  answers `effortFallback`. Owner abandon of an unfinished agent_create (capability
  `agent-create-abandon`): `POST /v2/agent-management/create-abandon {action_id, dry_run?}` (root)
  and `cutex human management agent-create-abandon <action_id> [--dry-run]` release the name, end the
  action as `create_abandoned` and remove only what it made (its durable session through the session
  delete path, else its native thread history moved to `runtime/session-deletes/`); completed,
  running and non-create actions are refused; repeating replays the receipt
  (`runtime/agent-management/v1/abandons/<action>.json`).
  新建时默认推理强度被换成模型支持的一档会在结果里写明；主人可用 `cutex human management agent-create-abandon` 放弃一次没完成的新建（释放名字、清理这次新建建出的会话，保留恢复副本）。

- Approvals at the next step (docs design 023, owner decision 2026-10-22). The approval decision notice (owner
  `cutex.approvals`, a cutex system notice) is now read by a BUSY requester at its next step with no wait, on Codex and
  on Claude adapters that offer `soon` (older adapters: after the turn, as before); idle requesters get it at once.
  The agent's own alarms stay after-turn. Marked by the alarm message id prefix `alarm-approval-` (an older release
  delivers such a notice as an ordinary after-turn alarm: rollback-safe, no format change); the alarm store checks
  the owner behind the prefix. Due alarms are now sent in the order they fell due (decision order), one notice per
  approval and outcome as before. Management shows `nextStep` for them (`afterTurnOnly` no longer set).
  Owner-notify replies and the owner's wake (`cutex/agentBus/wake`) are submitted `auto` also while the agent is busy
  (read at the next step like typed input; before: queued after the turn). A delegation the owner refused in review
  (`rejected` state item) reaches a busy requester at its next step at once instead of after the 120 s busy wait.
  审批结果现在会在忙着的智能体的下一步送到（不再等回合结束；智能体自己设的提醒照旧回合后）；主人通知的回复和主人唤醒也在下一步插入；委托审核被拒不再等 120 秒。

- Linux managed installs: stopping or restarting the Agent Bus no longer kills the agent runtimes it started (Claude
  adapters with Claude Code and their tools, Codex app-servers live in its cgroup). `cutex service install` writes
  `KillMode=process` (+ `TimeoutStopSec=30`) into `cutex-agent-bus.service` as it always did for the Management API;
  an existing bus unit written by `cutex service install` without any `KillMode` is patched in place (only those
  lines; `systemctl --user daemon-reload`) right before the upgrade runner's stop and before `cutex human service
  stop|restart` of the bus, so the first upgrade to this release already keeps the runtimes. The owner's own units
  (no marker) and units that set a `KillMode` are left alone.
  Linux 托管安装：停止或重启 Agent Bus 不再结束它启动的智能体运行时（以前所有由总线启动的 Claude 智能体在每次升级时都会被结束）；旧的总线单元在下一次停止前自动补上 `KillMode=process`。
- Claude background subagents: a finished background subagent / command now really wakes its agent with the
  "Claude Task" message. In r247 the Agent Bus and Management observers of a Claude adapter never asked for the
  adapter's `claude/*` notifications (`initialize` capability `claudeExtensions`), so the adapter never sent them
  `claude/task/updated` and no notice was queued (seen on hachimi, agent r23-live-check). The observers now ask for
  them and opt out of the high-volume ones (`claude/runtime/event`, `claude/runtime/statusChanged`,
  `claude/model/observed`, `claude/input/contextPersisted`, `claude/input/statusChanged`); every other `claude/*`
  frame is ignored before any store is touched. No adapter change (claude-r23 stays).
  Claude 后台子代理结束后，智能体现在会真正收到“Claude Task”消息（r247 中观察连接没有订阅适配器的 `claude/*` 通知，消息从未发出）。
- Test only: the service-tile fetch test accepts a timeout as well as a refusal for a closed loopback port on
  Windows (Windows retries the SYN for about 2 s).
- Usage-limit reset credits (capability `profile-reset-credits`): `GET /v2/profiles/{p}/reset-credits
  [?refresh=true]` shows how many resets the profile's account holds, when each expires and its rows
  (read on request only, cached 60 s per account); `POST /v2/profiles/{p}/reset-credits/consume
  {idempotencyKey, creditId?}` uses one (owner click only; the same key never uses a second one; 409
  `reset_credit_consume_in_progress` while one runs; on `reset` the quota is re-read). Codex profiles
  with a ChatGPT login use the app-server's `account/rateLimits/read` / `account/rateLimitResetCredit/
  consume` on the short-lived per-profile app-server; Claude subscription accounts use Claude Code's
  limit resets (`/api/oauth/usage?cedar_ember=1`, `POST /api/organizations/{org}/reset_rate_limits`,
  organization from the account's `.claude.json`). GLM / Z.ai profiles answer `supported: false`,
  `unsupportedReason: zcodeOnly`: their resets live on the ZCode service and need a ZCode login.
  额度重置券（能力 `profile-reset-credits`）：可查看配置所用账号的重置券数量和到期时间，并在主人点击确认后使用一张；支持 ChatGPT 登录的 Codex 配置和 Claude 订阅账号；GLM / Z.ai 的重置券只在 ZCode 服务里，需要 ZCode 登录，暂不支持。

- Claude background subagents (Claude adapter claude-r23, 0.8.0; capability `claude-background-tasks`):
  `Agent` / `Task` with `run_in_background` (or without it) runs on after its turn, so the agent reads
  the owner's next message at once instead of waiting for the subagent. Its tools are admitted by its
  agent id; cutex tools, nested subagents, questions and approvals are refused in it (full access: a
  safety check becomes a safety block on the origin turn). Its transcript stays open until the task
  ends. A finished, failed, stopped or lost one (and a main-agent background shell command, at most one
  notice a minute) reaches the agent as ONE Agent Bus message from "Claude Task" (control type
  `cutex.claude.task.v1`, view `cutex.claude-task.v1`, stable id `ctask-…`, so a repeated notice is
  queued once): read at a busy agent's next tool step at once, a new visible turn when idle. Its usage
  is added to the turn that started it. The session native request `claude/tasks/stop {threadId,
  taskId}` stops a running one; interrupting the turn does not. Claude Code's own invisible
  notification turn stays without tools (components/claude-runtime/docs/BUSY_DELIVERY.md).
  Claude 后台子智能体（适配器 claude-r23 / 0.8.0，能力 `claude-background-tasks`）：后台子智能体在父回合结束后继续运行，主智能体可立即读取主人的新消息；跑完、失败、被停止或因重启丢失时，cutex 以「Claude Task」给该智能体发一条消息（按任务去重；忙时下一步读入，空闲时开可见回合）；子智能体内不能用 cutex 工具、嵌套子智能体、提问或审批；`claude/tasks/stop` 可停止正在运行的后台子智能体。
- `cutex component install` repairs an incomplete component version dir instead of refusing it
  forever: a `<install base>/components/<name>/<version>/` without `receipt.json` (an unpack that
  never finished, or only a `prepared/needs-user.json`) or missing a required or receipted file is
  moved aside to `<version>.incomplete-<UTC timestamp>` and the version is downloaded and unpacked
  again (result `repaired`; `--dry-run` reports it). The Claude runtime records a user step only in
  an unpacked version dir, its status names an incomplete version as `degraded` with the repair
  command, and its install dry run no longer fails for a version not downloaded yet. A release that
  ships a component for other platforms only now answers `component_no_platform_package` (this
  platform, the packaged ones, `--from-source`) instead of "update cutex"; darwin-arm64 Claude
  runtime installs are source installs (releases package linux-x64 and windows-x64).
  组件安装不再因不完整的版本目录（缺 `receipt.json` 或必需文件）而一直失败：该目录被移到 `<版本>.incomplete-<时间>` 后重新下载解包；发布未提供本平台安装包时报 `component_no_platform_package` 并提示 `--from-source`（darwin-arm64 的 Claude 运行时仅支持源码安装）。
- A one-time safety grant (Approve once on a Claude safety check) reaches a busy agent at its running
  turn's next step (`cutex/userInput/submit` `auto`, steered as a busy wake past its wait is), no
  longer queued as "after this turn": the agent that hit the check usually still waits for it in that
  turn. Idle agents still get a new turn, offline ones are started.
  主人「批准一次」安全检查后，「主人已批准一次：…」对忙碌中的智能体在当前回合的下一步插入（与忙碌唤醒相同），不再显示为「本回合结束后送达」；空闲时照旧立即开新回合。
- Claude adapter claude-r22 (0.7.9): `thread/tokenUsage/updated` after each assistant message with new
  usage while a turn runs, not only at its end, keeping the thread's last known `modelContextWindow`
  (never null between steps).
  Claude 适配器 claude-r22（0.7.9）：回合进行中每条带用量的助手消息后即更新上下文用量，并沿用该会话最近已知的上下文窗口（中途不再变为空）。
- Bootstrap and history `cutexState.latestPlan` for Claude sessions (capability `session-latest-plan`):
  the newest `turn/plan/updated` checklist (TodoWrite / Task tools) of the bound thread, kept by the
  daemon across restarts and dropped when a revert removes its turn. It was always null.
  Claude 会话的 bootstrap / 历史 `cutexState.latestPlan` 不再总是 null：守护进程保存当前线程最新的任务清单（TodoWrite / Task 工具），重启后仍在，撤回其回合时清除。
- Home page pins can point at a client page (capability `user-home-page-pins`): `kind: "page"` with
  `key` `<pageId>:<argument>` (for example `project-view:<projectId>`, the desktop's Project view of
  a project; page id `[a-z][a-z0-9._-]{0,63}`, argument not empty). Other keys are refused with 400
  `invalid_home`; the `users` schema export (`homePin`) and docs/remote-clients.md describe it.
  主页固定项新增 `page` 类型（能力 `user-home-page-pins`）：`key` 为 `<pageId>:<参数>`（如桌面端某项目的 Project 视图 `project-view:<projectId>`），格式不符返回 400 `invalid_home`。
- `cutex human session delete <id> [--yes] [--json]` permanently deletes a session through the
  running Management API, like the desktop's "Delete…" (`cutex/session/delete`, as the owner with the
  local credential; the id is a cutex_session_id, Codex session id or unique name; on a terminal it
  asks first, otherwise `--yes` is required). It prints what was deleted and the recovery directory
  under `runtime/session-deletes/`. Old local explicit-launch sessions (e.g. pinned to a pruned r177
  bundle, or a native without `thread/delete`) are deleted too: their rollout moves into the recovery
  directory; a fork that still reads the history refuses the delete, copies left behind by earlier
  forks move along.
  新增 `cutex human session delete <id> [--yes] [--json]`：通过运行中的管理 API 永久删除会话，与桌面端「删除…」相同；终端上先确认，非交互时须加 `--yes`；输出删除内容和恢复目录。旧的本地显式启动会话（如固定在已清理的 r177 包上）也能删除：其历史文件移入恢复目录。
- `cutex human session unmanage` writes only its own record (no more "session store revision
  conflict" while the daemon runs) and refuses where the change would not hold: a session with a
  running runtime (its Agent Bus registration turned it back into an Agent within seconds), a durable
  Agent on the Agent Management roster (archive or delete it instead) and an already local session
  (unmanage never removed a record; use `session delete`).
  `cutex human session unmanage` 只写本会话的记录（守护进程运行时不再出现 revision conflict），并拒绝不会生效的情况：运行时仍在运行（Agent Bus 注册会在数秒内把它改回智能体）、Agent Management 名册中的持久智能体（请归档或删除）、已是本地会话（unmanage 从不删除记录，请用 `session delete`）。
- Service tiles for the home page (capability `service-tiles`): `service_register` / `service_update`
  and `POST|PATCH /v2/services` take an optional static `tile` template and / or `tilePath` (a path
  inside the service, default `/.well-known/cutex-tile.json`). `GET /v2/services/{slug}/tile` answers
  `{tile, fetchedAt, error, source}`; the daemon fetches the template from the service (2 s, at most
  16 KiB, redirects only inside the service), validates it strictly (unknown fields dropped, no URLs
  or `javascript:` / `data:` paths) and caches it for `refreshSeconds` (at least 60).
  `GET /v2/services/{slug}/tile/image` serves the tile image (PNG, JPEG or WebP, at most 512 KiB; no
  SVG). Same access as the services routes (owner; read-scoped devices may read).
  服务磁贴（主页用，能力 `service-tiles`）：注册 / 更新服务时可给静态 `tile` 模板和 / 或 `tilePath`（服务内路径，默认 `/.well-known/cutex-tile.json`）；`GET /v2/services/{slug}/tile` 返回磁贴，守护进程从服务取回（2 秒、16 KiB 上限、只跟随服务内跳转），严格校验并按 `refreshSeconds`（至少 60 秒）缓存；`GET /v2/services/{slug}/tile/image` 提供磁贴图片（PNG / JPEG / WebP，512 KiB 以内，不支持 SVG）。

## 0.1.5 "First Take" (2026-10-20)

Owner devices: every new device bound to the owner is listed and announced until the owner
acknowledges it, actions that change who controls the host work only on the host itself or over SSH,
and each device can be narrowed to chat or read; the host records the owner in person. Claude agents
get Side chats and forks while busy, read waiting notes at the start of every turn, keep context usage
after a reconnect and pick up a changed system prompt on resume. A cross-host `agent_tell` can wake the
other agent when its host allows it. Per-user home pages and desktop presence with remote close live on
the host; Profiles import from CC Switch; Windows runs every agent runtime in its own Job object. A
rollback to an older cutex never gives a narrowed device more rights. Pairs with desktop 0.1.5.
Components: claude-runtime claude-r21 (adapter 0.7.8); job-service stays 0.1.1; no new cute-codex
native (stays 0.160.0-r52.3).
主人的设备：每台新的主人设备都会列出并通知，直到主人确认；改变主机归属的操作只能在主机本机或通过 SSH 做；每台设备可限制为只聊天或只查看；主机记录「主人本人」。Claude 智能体支持侧聊和忙碌时分叉，每一轮开头读入待读留言，重新连接后保留上下文用量，恢复会话时使用更新后的系统提示。主机允许时，跨主机留言可以唤醒对方的智能体。按用户保存的主页、桌面在线状态与远程关闭保存在主机上；可从 CC Switch 导入 Profile；Windows 上每个智能体运行时放进自己的 Job 对象。回退到旧版 cutex 不会让受限设备获得更多权限。配合桌面端 0.1.5。组件：claude-runtime claude-r21（适配器 0.7.8）；job-service 仍为 0.1.1；没有新的 cute-codex 原生运行时（仍为 0.160.0-r52.3）。

**Owner devices and access**

- New owner devices are visible (cutex-desktop docs/research/host-ownership-20261019.md section 5; a
  device bound to the owner has the owner's rights, with three limits; capability
  `owner-device-notice`). A pairing code for the owner lives 5 minutes (others 10), is single use and
  says `ownerDevice: true`. Every token newly bound to the owner (pairing, assignment of a pending
  device, person-key auto-binding, `client-token issue` / `users assign` on the host) is listed by
  `GET /v2/users/owner/new-devices` (`via`: `pairing` | `assigned` | `personKey` | `ssh`) until an
  owner principal acknowledges it (`POST /v2/users/owner/new-devices/{tokenId}/ack`), and announced
  once as host event `cutex/access/ownerDeviceAdded` (CLI-added ones on the daemon's next request).
  Stored in `users-v1.json` `newOwnerDevices`.
  每台新的主人设备都会列出并通知，直到主人确认；主人设备的配对码 5 分钟有效、只能用一次。
- Root-identity actions only on the host itself or over SSH ("owner-local", capability
  `owner-local-only`): a loopback listener with the root credential, or with an owner token issued by
  `cutex management client-token issue` on the host (the SSH-tunnelled desktop), or the CLI. Tokens
  now record `issuedVia`: `hostCli` | `pairing` (token items, `user.token.issuedVia`); tokens of older
  builds are inferred from `kind` (exact: only `client-token issue` ever created client records, only
  `POST /v2/pair` device records), so existing SSH hosts need no re-pairing. A paired device's token
  never counts, even owner-bound on loopback. Owner-local only: setting the owner person, turning
  "new devices need approval" off, creating pairing codes, pairing with or enabling a peer
  (`POST /v2/peers`, `POST /v2/peers/invites`, `PATCH /v2/peers/{id}` `enabled: true`,
  `PATCH /v2/settings` `peers.enabled: true`) and reading `config.json` through `GET /v2/fs/file`;
  otherwise 403 `owner_local_only {action, clientToken, issuedVia}` (`owner_person_local_only` for the
  owner person). `GET /v2/capabilities` `user.ownerLocal` says whether the connection counts. Turning
  approval on, disabling peers and the older loopback-only settings stay open to owner devices on
  loopback.
  关闭「新设备需要批准」、和其他主机配对或启用对端、读取主人凭据、生成配对码只能在主机本机或通过 SSH 做（配对设备的 token 即使归主人也不算）。
- Per-device scope `full` (default) | `chat` | `read` on client / device tokens (capability
  `device-scope`): pairing-code `scope`, `client-token issue --scope`, `cutex management pair
  --scope`, `PATCH /v2/client-tokens/{id} {scope}`. Effective rights = the user's rights intersected
  with the scope, enforced in one gate (403 `device_scope_denied {scope, required}`): `read` = GET
  routes, event streams and read-only methods; `chat` adds input, queue, interrupt / stop,
  attachments and answering an agent's question, never approvals or any other write. Token items,
  `user.token` and `user.scope` of `/v2/capabilities` carry it; `tokens.json` stores it only when
  narrowed.
  每台设备可设为完全控制、只聊天和查看、只查看。
- Host owner in person: `GET /v2/host` gains `owner {displayName, fingerprint, confirmed, source}`
  for everyone connected, members and pending devices included. The first desktop key claimed with
  the root credential (or the first paired device's key) is the owner person; later keys added to
  the owner never take over. Only the host itself can change it: `PUT|DELETE /v2/users/owner-person`
  (owner-local: the root credential, or an owner token issued by `client-token issue` on the host
  i.e. an SSH-tunnelled desktop, on a loopback listener; paired devices' tokens and remote listeners
  get 403 `owner_person_local_only`) or `cutex management owner show|set-person|clear-person`. Older
  hosts are inferred from the only owner key or the users audit log (capability `host-owner-person`;
  docs/remote-clients.md). `cutex management client-token issue` prints `userId` and `pending`;
  `cutex management pair` takes `--user <id>`.
  主机记录「主人本人」：所有已连接的人（包括成员和待分配设备）都能在 `/v2/host` 看到主人的名字和指纹；第一次用主人凭据认领的个人密钥自动成为主人本人，只能在本机或通过 SSH 修改；配对命令可以直接指定用户。

**Claude agents**

- Side chats and busy forks for Claude agents (claude-runtime claude-r21, adapter features `sideChat`
  and `forkWhileBusy`; docs/SIDE-CHATS.md "Claude agents", components/claude-runtime/README.md). The
  `/v2/sessions/{id}/side-chats` routes now serve a Claude agent whose adapter declares `sideChat`:
  cutex relays them to the running adapter (`claude/side/open|list|read|send|interrupt|respond|close`),
  which keeps the Side state in memory and answers the same snapshot (`sessionId` and
  `runtimeGeneration` added by cutex). The Side runs on an official-SDK fork (`fork_session`) of the
  agent's native session through the selected finished turn, in a hidden thread with its own Claude
  Code process and the agent's settings, permission mode and system prompt; the Side instructions and
  boundary go before its first message. Its events and approvals stay in the Side chat; closing it
  deletes the hidden thread and its native copy, and stopping the agent closes its Side chats. Older
  adapters are refused as before (`409 side_runtime_unsupported`), a stopped agent with `409
  side_runtime_offline`; `native_transcript_behind` (the kept turn not on disk yet) becomes `409
  side_boundary_changed`. Forking (`cutex/session/fork`) a busy Claude agent (a running turn, queued
  inputs or notes) no longer fails: the fork is made from its transcript without closing or leasing
  the source runtime, leaves the running turn and the queue out, and the receipt says
  `sourceRuntimeUntouched: true`; an idle agent is forked as before. No migration; Side state is lost
  when the adapter restarts and a leftover hidden thread is deleted on the next Side request.
  Claude 智能体也能用侧聊了（看得到主对话到选定回合为止的内容，但不写回主对话），正在工作的 Claude 智能体也能直接分叉，不用先停下。
- Claude sessions now have `cutexState.latestTokenUsage` in `GET /v2/sessions/{id}/bootstrap` and
  `GET /v2/sessions/{id}/history` (capability `bootstrap-token-usage`). It was always null for them:
  the value is derived from the native rollout, which Claude sessions do not have. The daemon now
  keeps the newest `thread/tokenUsage/updated` the adapter sent for the session's bound thread
  (`runtime/management-v2/claude-token-usage/`, survives daemon restarts; dropped when a revert
  removes its turn or the session is deleted) and serves it in the same shape, so a phone or a
  restarted desktop shows context usage of Claude agents right away. Codex sessions are unchanged.
  Claude 代理重新连接或重启后，上下文用量不再显示为空，会立即显示最近一次的用量。

**Claude runtime (claude-r20, claude-r21)**

- claude-runtime claude-r21: a changed system prompt (the cutex harness, runtime limits, instructions,
  auto mode note) reaches resumed sessions. Claude Code replays the prompt recorded at a session's
  first request (`systemPromptSnapshot`); the adapter now remembers a digest of what the session
  recorded and resumes with the snapshot off when the text changed. Unchanged prompts keep the cached
  record.
  Claude 运行时 claude-r21：改过的系统提示（cutex 说明、运行限制、指令）在会话恢复后生效，不再沿用第一次请求时记录的旧提示。
- claude-runtime claude-r21: notes other agents left (passive notes, cross-host tells) are read at the
  start of the next owner turn too, including the turn the owner's Wake (`cutex/agentBus/wake`) starts:
  they are in its first model request, before the owner's text, each as its own text block (the
  owner's `@path` mentions still expand, the notes' do not). Before, an owner turn skipped them, so
  they arrived only after its first tool call and a turn without tool calls never saw them. Each note
  is still read once (components/claude-runtime/docs/BUSY_DELIVERY.md "Passive notes at turn start").
  Claude 运行时 claude-r21：主人发消息或点「唤醒」开始的一轮，开头就带上待读留言，不再等到第一次工具调用之后。
- claude-runtime claude-r21: a cutex input that arrives while Claude Code runs a turn it started by
  itself (a background command's or subagent's task notification) waits for that turn to end (quiet
  3 s, at most 120 s) instead of being submitted at once; before, the rest of that turn's messages and
  its result were taken as the new turn's reply.
  Claude 运行时 claude-r21：Claude Code 自己开的一轮（后台命令跑完的通知）还没结束时，cutex 送来的新消息先等它结束，不再把那一轮的回复算到新消息头上。
- claude-runtime claude-r21 (adapter 0.7.8): a subagent call without `run_in_background: false` is
  refused. Newer Claude Code runs a subagent in the background when the argument is left out, so the
  old check (refuse only `run_in_background: true`) let background subagents start; they lost every
  tool when the turn ended. The runtime limits told to the model say to pass `run_in_background: false`.
  Claude 运行时 claude-r21：子智能体必须明确设置 run_in_background 为 false，不写也会被拒绝；以前不写时会悄悄在后台运行，回合结束后失去工具。
- claude-runtime claude-r20 (adapter 0.7.7): Claude Code messages up to 64 MiB are read (SDK
  `max_buffer_size`; the default was 1 MiB). A tool result over 1 MiB, such as a screenshot or a large
  file read, ended the turn with "SDK receive failed (CLIJSONDecodeError)" (seen on a Windows host with
  a 1.26 MB screenshot).
  Claude 运行时 claude-r20：单条消息上限从 1 MiB 提高到 64 MiB，截图或大文件不再让这一轮以 CLIJSONDecodeError 中断。

**Cross-host peers**

- Cross-host wake (capability `cross-host-tell-wake`; cutex-desktop docs/research/peer-tell-wake-20261019.md
  section 10; docs/PEERS.md "Cross-host wake"): `agent_tell to="name@host" wake=true` can start the
  remote agent's turn when that host allows it. Per peer, new policy fields `inbound.tellWake` (default
  false; older configs read as false) and `inbound.tellWakesPerHour` (default 60, 1-100000), both
  accepted by `PATCH /v2/peers/{id}` `policy.inbound`. The receiver refuses a wake (403
  `wake_not_allowed` with `reason` `policy` | `review` | `agent_disabled`; nothing delivered, no
  receipt) unless `tellWake` is on, the target is enabled and the peer's review setting does not
  require review; past the hourly cap the wake is delivered as a note with
  `wakeDowngraded: true, reason: "rate_limited"`. No hop or cooldown control. Wire: optional hello
  feature `tells.v2`; a wake is schemaVersion 2 with `wake: true` (notes stay version 1), sent only to a
  peer that negotiated `tells.v2`, otherwise (or on an older receiver's 400) `remote_unsupported`.
  Results carry `deliveryKind` `wake` | `note`; `agent_list` `otherHosts[].tellWake` shows version
  support. A wake is queued with the new local control type `cutex.peer.tell.v2` and after-turn
  delivery (busy wait applies; Codex and Claude both read it); its model text says an agent on another
  host started the turn, and the recipient's message entries and events show `deliveryKind: "wake"`.
  Older binaries ignore the new policy fields (wakes are then refused); a rollback turns wakes still
  waiting into notes (see "Upgrade and components").
  跨主机留言可以唤醒对方的智能体：每台对端主机新增「允许唤醒」开关（默认关）和每小时唤醒上限（默认 60 次）；未允许或对方要求审核时拒绝且什么都不发，超过上限时改作留言送达。
- A cross-host note (`agent_tell to="name@host"`) waiting for its recipient is now visible to the
  recipient's owner: the bootstrap `agentBusMessages` entry and a live `cutex/agentBus/messageQueued`
  event carry its body, `deliveryKind: "note"`, the `name@host` sender and
  `remote {host, peerId, senderId}`; `messageDelivered` / `messageFailed` follow when it is read in or
  fails, and `cutex/agentBus/wake` counts it in `pendingNotes`. Before, it had no live event and the
  snapshot entry had no body (docs/PEERS.md "Cross-host tells").
  跨主机留言在对方的待读留言里可见：带正文、发送方「名字@主机」和来源主机，读入后消失。

**Users and desktop clients**

- Per-user home page settings on the host (capability `user-home`; cutex-desktop
  docs/research/home-page-20261019.md section 1; docs/remote-clients.md "Home page"): `GET
  /v2/users/me/home` answers `{revision, home: {...}|null, coverEtag, updatedAt}`; `PUT
  /v2/users/me/home {revision, home}` replaces it with optimistic concurrency (409
  `home_revision_conflict` with the current `revision`). `home` is the client's JSON object (at most
  64 KiB, 413 `home_too_large`); the known keys `layout`, `blocks [{id, visible}]`, `bio` (at most 200
  characters), `sidebarEntry`, `showWhenEmpty`, `homeHost` and `pins [{hostId, kind:
  service|artifact|agent|url, key, title?, size?: s|m|w|l}]` (at most 64) are checked (400
  `invalid_home`), unknown keys are kept. Cover image: `GET|PUT|DELETE /v2/users/me/home/cover` (PUT
  `{contentType, data}`, PNG / JPEG / WebP at most 2 MiB, each side at most 4096 px; GET with ETag;
  400 `invalid_cover`, 413 `cover_too_large`); `coverEtag` in the home answer. Own record only (owner
  or a bound user; pending devices 403 `access_pending`); device scope `read` may only read, `chat`
  and `full` may write. Stored per user in `runtime/users/home/`; removed with the user. Schema
  export `users` (`homeView`, `homePut`, `homeCoverPut`).
  主页设置存在主机上、按用户分开：同一个人的各台设备看到同一个主页（版式、区块、简介、开关、固定项和封面图），带版本号防止两台设备互相覆盖。
- Remote close of a desktop client (capability `client-remote-close`; cutex-desktop
  docs/research/remote-close-desktop-20261019.md; docs/remote-clients.md "Desktop presence and remote
  close"): desktops report presence with `POST /v2/clients/presence` (repeat = heartbeat every 60 s;
  kept only in memory, dropped after 180 s without one; any principal, pending and narrowed devices
  included; 409 `client_instance_conflict` for an instance id of another credential). `GET
  /v2/clients` lists them (an owner with scope `full` sees all, others their own user's, a pending
  device only its own). `POST /v2/clients/{instanceId}/close {graceSeconds?}` (default 15) answers
  202 for an owner or the same user with scope `full`; else 403 `client_close_forbidden {reason:
  pending|scope|otherUser}`, 404 `client_not_found`, 409 `remote_close_disabled`; audited; host event
  `cutex/client/closeRequested {instanceId, requestedBy {deviceName, userId, local}, graceSeconds,
  at}`. The instance reports `POST /v2/clients/{instanceId}/closing {outcome: closing|cancelled}`
  (only its registering credential), host event `cutex/client/stateChanged {instanceId, state}`.
  Members' event streams now carry the `cutex/client/*` events of their own instances (and accept
  `cutexSessionId=cutex.host`); the presence answer also carries `state` and `closeRequest` for
  clients without an event stream. Schema export `clients`. Only the desktop app quits; the daemon and
  agents are untouched.
  主人可以从别的设备远程关闭忘了关的 cutex 桌面（只关桌面 app，不动守护进程和智能体）：桌面向主机报到和心跳，主人可关任何桌面，其他用户只能关自己的；桌面可关掉「允许远程关闭」。

**Profiles**

- Import Profiles from CC Switch: `GET /v2/profile-imports/cc-switch` lists the Claude Code and
  Codex providers of a CC Switch install on this host (read-only, never a key; `existingProfile`
  names a matching Profile), and `POST` creates Profiles from chosen rows: Codex rows become API-key
  Profiles, Claude rows a Claude API account plus a Claude Profile (capability
  `profile-import-cc-switch`; docs/management/profile-import-cc-switch.md).
  可以从 CC Switch 导入 Profile：列出本机 CC Switch 的 Claude Code 与 Codex 供应商（只读，不返回密钥），并为选中的条目创建 Profile。

**Platforms**

- Windows: every managed Agent runtime (Codex app-server, Claude adapter) runs in its own Job object
  (`Global\cutex-agent-...`, kill on close, no breakaway). Stopping, restarting or archiving an Agent
  terminates the whole job, and a runtime that exits or crashes takes its descendants (Claude Code,
  shells, dev servers, builds) with it; before, they survived as orphans. The job's last handle lives
  in the runtime itself, so a management restart still leaves runtimes running for adoption. The
  runtime environment reports `CUTEX_MANAGED_AGENT_ISOLATION=windows_job_object:<job>` (was
  `direct:windows_no_systemd`); docs/agent-management-provider.md "Managed Agent process boundary".
  在 Windows 上，每个智能体的运行时放进自己的 Job 对象，停止、重启或归档智能体会结束它启动的全部子进程（Claude Code、命令、开发服务器、构建），不再留下孤儿进程；守护进程重启不影响正在运行的智能体。

**Upgrade and components**

- A rollback to an older cutex never widens access and never leaves a wake stuck. The rollback step
  that rewrites records for the older release (services stopped; each changed record copied to the
  run's `displaced/older-stores/` first) now also holds every client / device token whose scope is
  not `full`: a release without device scopes refuses it (401) instead of giving it the user's full
  rights, and 0.1.5 reads it again with its scope after a roll-forward, without re-pairing
  (`client-tokens-held.json`: ids, labels and scopes, never a secret). Cross-host wakes still waiting
  and not yet handed to a runtime become cross-host notes, which an older release delivers; it would
  otherwise never deliver them and hold the owner's input and new delegations to that agent
  (`peer-wakes.json`). Unacknowledged new owner device notices, which an older release drops, are
  copied aside (`new-owner-devices.json`). Each step now runs only when the rollback target is older
  than the release that introduced its records (or is not a plain release label), so a rollback to
  0.1.4 keeps `static_root` approvals and pending Claude launch permission modes, which 0.1.4 reads.
  Left as they are, since an older release only ignores them: the owner person (if an older release
  drops it, 0.1.5 reads it again from the users audit log), peer `tellWake` (dropped: wakes stay off),
  home pages, Claude token usage and Side chat state.
  回退到旧版 cutex 时，限制了范围的设备 token 会被暂停（旧版拒绝它，而不是给它完全权限；再升级回来后自动恢复原来的范围，无需重新配对），尚未送达的跨主机唤醒改为留言送达，未确认的新主人设备通知另存一份；回退到 0.1.4 时保留它能读取的 static_root 审批和 Claude 启动权限模式。
- Backend updates no longer wait forever on a stalled connection: resolving the release host, sending
  the request and waiting for the response are now bounded (20 s / 60 s / 60 s; the body download keeps
  its 30 min). In Windows Sandbox `cutex upgrade check` hung for over 10 minutes; it now fails with
  "cannot reach …" and can be retried.
  后端更新遇到卡住的网络连接时不再无限等待：解析、发送请求和等待响应都有了超时，失败后可以重试。
- Components: claude-runtime claude-r21 (adapter 0.7.8; the entries above; claude-r20 was never
  published); job-service stays 0.1.1. No new cute-codex native (stays 0.160.0-r52.3).
  组件：Claude 运行时更新为 claude-r21（适配器 0.7.8）；任务服务组件仍为 0.1.1；没有新的原生运行时（仍为 0.160.0-r52.3）。

## 0.1.4 "First Take" (2026-10-19)

Claude agents: launch permission modes (including "Auto", which covers Claude Code's own tool
permissions and never decides cutex approvals), context windows and effort levels from the Claude
runtime's reference catalog, repin to the host's selected Claude runtime over the API, skills
import, subagent transcripts, file rewind and permission rules. Profile looks (pictures, themes,
greeting) live on the host; approvals record who decided, hold the agent's call while the owner
decides and say when it was approved after waiting, and a static root outside the workspace asks
the owner instead of failing; proxy modes; pending server requests and conversation activity on
sessions; the browser scroll outcome; agents restart their own services; upgrades show the installed
release's notes; and a rollback to an older cutex no longer leaves it unable to read its stores.
Pairs with desktop 0.1.4. Components: claude-runtime claude-r19 and job-service 0.1.1 (licence files
only); no new cute-codex native (stays 0.160.0-r52.3).
Claude 智能体：启动时可选权限模式（含「自动」，它只管 Claude Code 自己的工具权限，不替你决定 cutex 审批）、
上下文窗口与推理强度取自 Claude 运行时的参考目录、可通过 API 换到主机选定的 Claude 运行时、导入技能、查看子代理
过程、回退文件、权限规则。Profile 形象（头像、主题、问候语）保存在主机上；审批记录谁做的决定，等待期间挂起
智能体的调用、等待后批准会附说明，工作区外的静态根目录改为请主人审批；代理模式；会话上的待答请求与对话活动；
浏览器滚动结果；智能体可重启自己的服务；升级页显示已安装版本的说明；回退到旧版不再导致无法读取存储。
配合桌面端 0.1.4。组件：claude-runtime claude-r19、job-service 0.1.1（只加了许可证文件）；没有新的
cute-codex 原生运行时（仍为 0.160.0-r52.3）。

**Claude agents**

- Claude agents can be created in a chosen Claude Code permission mode (capability
  `claude-launch-permission-mode`): `POST /v2/human/agents` and `/v2/human/sessions` accept
  `launch.claudePermissionMode` (`default` | `acceptEdits` | `plan` | `auto` | `bypassPermissions`) for
  Claude profiles. cutex keeps it on the new session and sends it once, as `thread/settings/update`, at
  the first runtime start (before the native session connects); `plan` turns plan mode on over the
  adapter's default mode. Codex profiles, and modes the selected Claude runtime does not list in
  `features.json` `claudePermissionModes`, answer 400 `unsupported_launch_option`. `/v2/profiles` items
  of Claude profiles gain `claudeLaunchPermissionModes` (the offered modes; null on Codex). A session
  record holding a not-yet-applied mode (`initial_permission_mode`, only between creation and the first
  start) is not readable by an older cutex (a rollback clears it, see below).
  新建 Claude agent / 快速对话时可以选择 Claude Code 的权限模式。
- claude-runtime claude-r19 (adapter 0.7.6): a thread whose Claude permission mode is "auto" when
  its session starts gets one more runtime-limits sentence in the system prompt: the mode only
  covers Claude Code's own tool permissions and does not decide cutex approvals (managed services,
  delegation reviews, browser grants and other cutex tool approvals still go to the owner).
  The adapter's reference model catalog gains `contextWindow` per model: the nominal window from
  Claude Code's own model registry (CLI 2.1.299: 1M for Opus 4.7 and later, Sonnet 5 / 5.5, Fable 5 /
  5.1 and Haiku 5.5; 200K for Opus 4.5 / 4.6, Sonnet 4.5 / 4.6 and Haiku 4.5; 1M for the `[1m]`
  aliases; null for the other aliases, which resolve per account). `thread/settings/update` now
  accepts the effort levels `model/list` advertises: the live handshake's levels when it names the
  model, else the reference catalog's. Before, a pinned model ID (not in Claude Code's handshake
  list) or an offline thread only accepted its current effort, so the desktop's effort menu failed
  with "This model does not advertise the selected effort". The refusal now names the model, the
  requested effort and the offered levels.
  claude-r19：「自动」模式的系统提示说明它不决定 cutex 审批；参考模型目录带上各模型的上下文窗口；推理强度
  菜单接受 model/list 提供的所有档位，不再报「This model does not advertise the selected effort」。
- Claude Profile model lists (`GET /v2/profiles/{p}/models`, `modelsSource.kind: claudeReference`):
  rows carry the selected Claude adapter's nominal `contextWindow` (`effective.contextWindowSource:
  "reference"`; null for aliases resolved per account, and with adapters before claude-r19) and no
  auto-compact limit (`autoCompactTokenLimit: null`, `effective.autoCompactTokenLimitSource:
  "notApplicable"`): Claude Code decides compaction itself. Before, the Codex rule could invent
  "90 % of the window". The view gains `autoCompactApplies` (false for Claude profiles).
  Claude Profile 的模型列表显示参考上下文窗口，不再套用 Codex 的自动压缩上限（压缩由 Claude Code 自己决定）。
- Claude agents can be moved to the host's selected Claude runtime over Management v2 (capability
  `claude-runtime-repin`), the HTTP form of `cutex human claude repin <id>`:
  `GET /v2/sessions/{id}/claude-runtime` reports the pinned runtime, the selected one, a pending
  change, `upgradeAvailable` and what a repin now would do (`dryRun`); owner-only
  `POST /v2/sessions/{id}/claude-runtime/repin {target?}` performs it with the same rules as the
  CLI. An offline agent moves at once; a running agent is never restarted, the change applies at its
  next fresh runtime start (`restartRequired`). Refusals (another Claude config home, a schema-2
  adapter state on a pre-r8 target, a lifecycle operation in progress) answer 409
  `claude_repin_refused` and change nothing. Schema export `claude-runtime`.
  Claude 智能体可通过 API 换到主机选定的 Claude 运行时；离线的立即生效，运行中的在下次全新启动时生效。
- Claude session resources carry their runtime pin (capability `session-claude-runtime`):
  `runtime.claudeRuntime {release, pendingRelease}` (release labels such as `claude-r17`;
  `pendingRelease` is a repin waiting for the agent's next fresh runtime start, else null; absent on
  other sessions), and `GET /v2/sessions` carries `claudeRuntime {selectedRelease}`, the release for
  new Claude sessions (null when none is usable). A client can decide whether to offer the runtime
  upgrade from the agent list alone, without `GET /v2/sessions/{id}/claude-runtime` per agent.
  `cutex/sessions/changed` now lists a session whose pin or pending repin changed (repin applied or
  deferred, applied at start) and sets `claudeRuntimeChanged: true` when the selected Claude runtime
  changed (`cutex human claude install`). The sessions schema now allows a null
  `native.schema.sha256` (Claude sessions without a protocol file).
  Claude 会话带上所用运行时版本与待生效的更换，客户端从智能体列表即可判断是否提示升级运行时。
- Import Claude Code skills into the host's shared Claude skills directory (capability
  `claude-skills-import`, owner only): `GET /v2/claude/skills/import[?source=<abs dir>]` lists the
  skills of a source directory (default: the host user's `~/.claude/skills`) with name, front-matter
  title / description, size, whether one of that name already exists in `~/.cutex/claude-skills` and
  whether it can be imported; `POST /v2/claude/skills/import` `{source?, names, overwrite?}` copies the
  chosen ones (`{imported, skipped, failed}`). Copies only, the source is never changed; existing skills
  are skipped unless `overwrite` (then replaced as a whole); symlinks never lead out of the source (such a
  skill directory is refused, such a link inside a skill is left out); each skill at most 16 MiB / 2000
  files / 16 levels, `SKILL.md` at most 48 KiB; copies are staged in the target and renamed into place.
  从 `~/.claude/skills` 导入技能到主机共享的 `~/.cutex/claude-skills`（复制，不改动源目录）。
- Claude agents: the skills a Claude agent loads are listed for the desktop's skill picker
  (`GET /v2/sessions/{id}/skills`, capability `claude-session-skills`; works while the runtime is
  offline), and a new host-level directory `~/.cutex/claude-skills` is shared by every Claude agent on
  the host (next to the workspace `.agents/skills` / `.claude/skills` and the account's `cutex-skills`).
  Claude 智能体的技能可在桌面技能选择器中列出和选用；新增主机级共享技能目录 `~/.cutex/claude-skills`。
- Claude agents: a foreground subagent's work can be opened. The Claude runtime keeps a bounded
  transcript of each Agent / Task subagent (its messages, thinking and tool calls; at most 400
  entries per subagent and 100 subagents per agent, kept across restarts) and answers native
  `thread/read` on the subagent reference its `subAgentActivity` items carry
  (`claude:<session>:task:<id>`, `parentThreadId` = the agent's thread; adapter feature
  `subagentTranscripts`). New capability `claude-subagent-transcripts`: the runtime's live frames for
  such a subagent are published stream-only on the parent session (they never touch its record,
  activity, usage or queue). Background subagents stay off. New capability
  `thread-search-occurrences`: native `thread/searchOccurrences` (`{threadId, searchTerm, cursor?,
  limit? 1..200}`) is forwarded for the bound thread only (read grant), for Claude agents and Codex
  natives 0.160+; the home-wide `thread/search` stays denied.
  Claude 智能体的前台子代理过程可以打开查看（有上限的记录，重启后仍在）；新增对话内搜索（Claude 智能体与
  新版 Codex 原生）。
- Claude agents: a history revert can also restore files (`cutex/session/history/revert`
  `rewindFiles: true`, capability `claude-rewind-files`), and Claude Code file checkpointing is now on
  by default for Claude profiles (applies from the agent's next start).
  回退 Claude agent 的对话时可同时回退文件；Claude Profile 默认开启文件检查点（下次启动运行时生效）。
- Claude agents: permission modes, project allow rules and same-turn steering (adapter feature flags
  `claudePermissionModes`, `livePermissionMode`, `permissionRules`, `turnSteer`). The adapter now
  changes the permission mode and plan mode during a running turn (a model / effort change still
  waits for idle). New capability `claude-permission-rules`: `GET|PUT
  /v2/sessions/{id}/claude-permission-rules` (owner only; `{allow, expectedRevision?}`, 409
  `revision_conflict` / `unsupported_runtime`, 400 `invalid_rule`). cutex keeps the rules per working
  directory in `~/.cutex/claude-permission-rules.json` (never in the repository), writes them to the
  session's `permission-rules.json` at launch, and passes them to the SDK as flag settings
  `permissions.allow`. A change reaches a running agent through `claude/permissionRules/reload`
  (`apply_flag_settings`), otherwise its next start. An approval answered
  `acceptWithClaudeRules` (offered from Claude's suggestions, at most 8) is applied for the session and
  saved into the store. `turn/steer` on a busy subscription agent queues the input as a `soon` busy
  delivery injected at the next main-agent PostToolUse, falling back to the next queued turn; the
  user message appears in the running turn (`steered: true`). cutex then no longer routes such input
  to the Claude queue (`input.sameTurnSteering`).
  Claude 智能体：运行中可切换权限模式与计划模式；按工作目录保存的项目允许规则（保存在 cutex 主目录，不写入仓库）；
  运行中发送的消息在同一回合内送达。
- Claude agents can read Claude Code's own settings and use your MCP servers. A Claude Profile has two
  new switches (Profile → Behaviour): the project's Claude Code settings (CLAUDE.md, `.claude/` rules,
  subagents, commands, skills, hooks, permissions of the agent's workspace) and the account's user
  settings (the Claude account's own folder, never `~/.claude`). Existing Profiles keep both off; Claude
  Profiles created from now on start with the project settings on. Claude agents now also get the MCP
  servers of the global / project / agent layers (stdio and streamable HTTP; secrets passed only in the
  runtime environment), plus an off-by-default switch for the login's claude.ai connectors.
  `GET /v2/sessions/{id}/claude-code` shows what an agent got and whether a restart is needed (capability
  `claude-code-sources`; needs the Claude runtime from this release).
  Claude 智能体可以读取 Claude Code 自己的设置并使用你配置的 MCP：Claude Profile 新增「读取项目的 Claude Code
  设置」「读取账号的用户设置」开关（已有 Profile 默认关，新建的 Claude Profile 默认开项目设置），全局 / 项目 /
  agent 三层 MCP 服务器也给 Claude 智能体；claude.ai 连接器单独开关、默认关；下次启动生效。
- Claude runtime: fast mode for Opus 4.8 / Opus 5 models (the desktop's Fast toggle; reconnects the
  same session), and a turn that fails on Claude Code's transient OAuth refresh lock before any output
  is sent again automatically after 20 s, then 45 s, instead of failing.
  Claude 运行时支持 Fast 模式；遇到 Claude Code 登录令牌刷新锁的瞬时失败时自动重发，不再直接失败。

**Claude runtime fixes (claude-r16 to claude-r18)**

- claude-runtime claude-r18 (adapter 0.7.5), r243 acceptance follow-ups: in "Ask first" mode a
  cutex tool call (for example `agent_delegate`) waits for the owner on an ordinary approval card
  that names the tool and its arguments, with Allow, Allow for this session, Decline and Cancel turn.
  It used to be a form whose "Allow this invocation" checkbox started unticked, so accepting the card
  without ticking it refused the call at once with "Not approved". Read-only cutex tools (for example
  `agent_list`) no longer ask, like Claude Code's own read tools. With command execution off the
  PowerShell tool is turned off too, and the system prompt says there is no shell tool and project
  hooks do not run (Claude Code's own environment section still names the OS shell). Claude Code's
  placeholder reply "No response requested." after an interrupted turn is never shown as Claude's
  message.
  「每次询问」模式下 cutex 工具调用改用写明工具名和参数的普通审批卡片并等待 owner 决定，只读工具不再询问；
  关闭执行命令时同时关掉 PowerShell 并在系统提示里说明。
- claude-runtime claude-r18 (adapter 0.7.5): a message sent while a Claude turn runs and then
  interrupt & send reaches the model once. An owner interrupt now drops the turn's unread steers when
  it is requested, so a tool call before the abort lands, or a turn that completes anyway, no longer
  delivers the text besides the desktop's resend (claude-r16 delivered it as the new turn's message
  and again as "The user sent this message while you were working" context); a failed interrupt keeps
  them waiting, and steers with images are dropped the same way. The system prompt now tells Claude
  that background subagents are refused (use foreground ones) and that a turn woken by a finished
  background shell command cannot use tools, so it checks the result in the next user turn.
  中断并发送时，运行中发出的消息只送达一次；系统提示词说明不支持后台子代理、后台命令完成通知唤醒的轮次不能用工具。
- claude-runtime claude-r17 (adapter 0.7.4): with a Claude profile's command execution off, Claude
  Code hooks no longer run. The adapter's flag settings carry `disableAllHooks: true`, so hooks from
  the project / user settings sources (`.claude/settings.json`, the account home), skill or agent
  frontmatter and plugins, and a statusLine command, are skipped; before, they ran as the OS user
  even with Bash off. The adapter's own SDK callback hooks keep running (checked in Claude Code
  2.1.289). With execution on, hooks behave as before. Claude Code's built-in auto memory is now off
  for every Claude agent, not only when a settings source is on: memory stays cutex's project memory.
  Claude Profile 关闭命令执行时，Claude Code 的 hooks 不再运行；Claude 自带的自动记忆对所有 Claude 智能体关闭。
- claude-runtime claude-r16 (adapter 0.7.3): declares `features.approvalWait`; with the descriptor's
  `approvalWaitSecs` it gives the façade the hold and the calling turn, and runs `service_register`,
  `service_update`, `agent_create`, `member_replace` with a 150 s timeout instead of 30 s;
  `delegation_wait` always gets 150 s (its Bus wait is up to 120 s and used to be cut at 30 s).
  claude-r16 支持审批等待：相关工具调用超时延长到 150 秒，`delegation_wait` 不再在 30 秒时被截断。
- claude-runtime claude-r16: restarting or stopping a Claude runtime while a turn runs (Restart
  runtime, Offline, profile switch, archive) now interrupts that turn first, as cutex does for a
  Codex runtime, so it ends `interrupted` with its open tool calls closed before the runtime goes
  away (before: `failed`, "Runtime closed before terminal result"). Only a confirmed interrupt claims
  that; if the native runtime does not drain it in time, the turn still ends `failed` (outcome
  uncertain). A turn the previous adapter process left running (killed, crashed, host restart) is
  settled on the next start as `interrupted` (was `failed`), every open item of it closed (not only
  command, file, MCP and dynamic tool calls), and the settling `item/completed` / `turn/completed`
  events are sent to each client that resumes the thread until its next turn starts: nobody was
  connected when they were written, so clients kept showing the turn and its tool as running. The
  offline history reader (a stopped agent) reads such a turn as `interrupted`, like cute-codex for a
  thread not loaded.
  Claude 智能体运行中重启运行时，正在进行的回合现在显示为已中断，工具不再一直转圈。
- claude-runtime claude-r16: the last text of a Claude turn now reaches live clients as the final
  answer (`agentMessage.phase` `final_answer`): it completed as commentary, and only the saved copy
  was marked final, so a desktop or delegation watching live saw commentary until a reload. The
  terminal result now sends the same item again with `item/completed`.
  Claude 回合的最终回复现在实时标记为最终答案，无需重新加载。
- claude-runtime claude-r16: an owner message steered into a running Claude turn (`turn/steer`) that
  the turn never read is now dropped when the turn ends interrupted or failed, as cute-codex drops
  unread steers on an abort; the desktop's "interrupt & send" resends it. Before, it stayed queued
  and was injected into the next, unrelated turn at its first tool call: after Esc with a waiting
  message the model got it twice and the conversation showed it twice. A turn that completes still
  runs an unread steer as the next turn.
  运行中发送给 Claude 的消息在回合被中断后不再被重复送达。
- claude-runtime claude-r16: permission denials the Claude adapter answers itself no longer tell the
  model "The user did not approve this tool call" when no owner answered: a request that matches no
  live tool call (turn ended, runtime restarting) or that nobody answered (no owner client connected)
  now says so and that nobody refused it; only the owner's own Decline or Cancel says the owner
  declined. Claude Code's own "The user doesn't want to proceed with this tool use…" is not adapter
  text: Claude Code writes it for every tool call of a batch when its turn is aborted (an approval
  answered Cancel, or Claude Code's auto mode classifier notice it cannot show to an SDK host).
  Claude 适配器自己拒绝工具调用时，不再声称是用户拒绝的。

**Profiles**

- Profile looks live on the host (capability `profile-avatar`), so every device that connects shows
  the same ones. `GET|PUT|DELETE /v2/profiles/{p}/avatar` stores a Profile picture (PUT
  `{contentType, data}`: a square PNG / JPEG / WebP of at most 48 KiB, 16..=512 px, the same rules
  as user avatars) and `GET|PUT|DELETE /v2/profiles/{p}/intro-portrait` the intro portrait (at most
  2 MiB, any aspect, each side at most 1024 px); GET answers the image with an ETag
  (`If-None-Match` gives 304). Profile items carry `avatarEtag` / `introPortraitEtag`;
  `appearance.avatarThemes` (a non-empty list of unique known theme ids, `detail.avatarThemeOptions`;
  null = the client's default by model family) and `appearance.introGreeting` (at most 300
  characters) are PATCHable like `color` (null clears). Images are keyed by profile id (a rename
  keeps them), removed with the profile, not copied by duplicate / export, and stored under
  `runtime/profile-avatars/` (never `config.json` / `accounts.json`); themes and greeting are
  `accounts.json` `options.avatar_themes` / `options.intro_greeting`. No migration. Rollback: an
  older cutex ignores `runtime/profile-avatars/` and keeps the two options in its unknown-field
  `extra`, so a roll-forward finds them again; it does not serve or list them meanwhile.
  Profile 的头像、介绍立绘、头像主题和问候语保存在主机上，所有连接的设备看到的都一样；不随复制 / 导出带走。

**Approvals**

- Approval decisions are traceable (capability `approval-decided-by`): a decided owner approval
  records `decidedBy {by: "owner", connection: "loopback" | "remote", peer?, client?}`, where
  `client` is what the deciding client sends about itself in the new optional `client {app, version,
  device, surface, input}` field of `POST /v2/approvals/{id}/approve|deny`, and every decision writes
  one `cutex-approval-decided` line to the Management log. Older records without `decidedBy` read as
  before. Found while chasing an r243 acceptance denial nobody remembered making: only that endpoint
  (root credential) ever denies an approval.
  审批记录现在会记下是谁、从哪个客户端做的决定，管理日志里也有对应一行。
- Owner approvals hold the agent's tool call (approval-wait): a gated `service_register` /
  `service_update` (managed, persistent, outside static root) or an `agent_create` /
  `member_replace` on an approval-gated profile now waits up to 120 s for the owner's decision.
  Approved in the window, the call proceeds and returns its normal result (approval used once, no
  retry, no decision notice); denied, it answers `approval_denied`; otherwise it answers
  `approval_required` as before and the request stays on Approvals. Opt-in per caller, so pinned
  older façades / adapters answer at once: cutex sets `CUTEX_MCP_APPROVAL_WAIT_SECS` for Codex
  façades and for Claude adapters declaring `features.approvalWait` (claude-r16). The hold ends
  without using an approval when the caller's connection closes, its turn ends (Management sees
  `turn/completed`; Codex never cancels the MCP call on interrupt) or its runtime was replaced; the
  decision notice then follows as before. Approval records gain `wait {id, startedAt, until,
  threadId?, turnId?, endedAt?, end?}` and `cutex/approvals/changed` reports `waiting` /
  `wait_ended`. Capability `approval-wait`.
  审批时工具调用最多等待 120 秒：期间批准则直接继续执行并返回结果，拒绝则返回拒绝，超时才回到原来的
  `approval_required` 流程。
- Approved after waiting (Q21): when an agent's tool call held for the owner's decision
  (approval-wait: `service_register` / `service_update` gates, `agent_create` / `member_replace`
  with a profile that needs approval) and the owner approved it during the hold, the successful
  result now says so: `approval {decision: "approved", waitedSecs, approvalId, detail}` and a
  `detail` that starts with "The owner approved this request after N s; it has been carried out.",
  so the agent neither assumes it ran unreviewed nor retries it. Results that needed no approval,
  or used an approval granted before a retry, are unchanged. Agent Management results carry it only
  to façades announcing `approval-note=1` in `X-Cutex-Facade` (this release's façade), and only on
  a complete receipt; older façades get the old result.
  等待期间被批准的调用，成功结果会说明「主人在 N 秒后批准，已执行」，智能体不会误以为未经审核或重复执行。
- Owner approvals requested in the Agent Bus process (managed / persistent services, agent
  profiles) now reach clients live: Management serve watches `runtime/approvals/approvals.json` (a
  metadata check every 2 s) and publishes `cutex/approvals/changed` for every state it did not
  publish itself (reasons `requested | approved | denied | consumed | expired`). Before, those events
  were appended from the Bus process only and never reached Management's live subscribers, so the
  desktop showed no alert, badge or card until something else reloaded the list. Capability
  `approvals-live`.
  Agent Bus 里发起的审批（托管 / 常驻服务、智能体 Profile）现在实时推送到客户端，桌面端立即显示提醒和卡片。
- After the owner approves or denies a request the agent has to retry itself (`managed_service`,
  `service_lifetime`, `static_root`, `agent_profile`, `browser_open`), the requester gets a short
  cutex message ("approved apr-…: retry the same call now" / "denied …: do not retry"), queued
  through the alarm scheduler (owner `cutex.approvals`, `after_turn`, so a busy agent reads it after
  its turn). The tool still answers `approval_required` until approved; the refusal text now says
  the message will come. Capability `approval-decision-notice`.
  审批结果会以消息通知发起的智能体，不必轮询。
- A tool call repeating a request the owner already denied (same digest, denial still live)
  still answers `approval_denied` at once, but the detail now says it is that earlier decision:
  when the owner denied it, until when the same request stays denied (24 h after the decision),
  and to change the request (another slug, command or target) or ask the owner. New refusal
  fields `denied_at`, `denied_until`, `reused_denial: true`. A denial decided while the call
  waited keeps the old text.
  重复已被拒绝的同一请求时，拒绝说明会写明这是先前的决定、拒绝时间和有效期。

**Agent services**

- Agent services: `service_register` / `service_update` validate the whole request (slug, fields,
  ownership, port, root) before raising any owner approval, so the owner is never asked to approve a
  request that then fails (capability `static-root-approval`). Static roots: a root inside the
  caller's cwd or project workspace needs nothing; an agent whose cwd is its own cutex-managed
  scratch workspace (`~/.cutex/agents-scratch/<id>`, `~/.cutex/chats/<id>`, e.g. a Human agent
  created without cwd) may now serve from inside that cwd (the rest of the cutex home stays
  forbidden); any other existing directory or `.html` file outside the cutex home now raises an owner
  approval naming the canonical root (new approval kind `static_root`; folded into the one
  `service_lifetime` approval when the request is also persistent, `details.staticRoot` /
  `allowedRoots`) instead of a hard refusal. Still refused (403 `static_root_forbidden`): the cutex
  home, filesystem roots, the user home or any directory containing it or the cutex home; the detail
  now names the sent root and lists the caller's allowed roots. An agent's re-register of its own
  service with an unchanged, already approved root asks nothing again.
  静态服务根目录：先完整校验再请求审批；cwd / 项目工作区之外的目录改为请主人审批，不再直接拒绝；
  scratch 工作区里的 agent 可以发布自己 cwd 下的页面。
- Managed services follow the agent's live permissions (Q21): `service_register` /
  `service_update` decide whether a managed command is within the caller's envelope (full access:
  sandbox `danger-full-access` + approval `never`, no approval card) from the live native thread
  settings of its current runtime, like `browser_open`, instead of the permissions it was launched
  with; switching a running agent to Full access no longer leaves an approval card. Runtime
  managers write the permission fields of those settings to
  `runtime/live-thread-settings/<cutex session id>.json` for the Agent Bus; without a file for the
  current runtime generation and thread the stored launch permissions decide, as before.
  托管服务按智能体当前的实时权限判断是否需要审批；运行中切换到完全访问后不再弹出审批卡片。
- New agent tool `service_restart {slug}` (capability `services-agent-restart`): the owning agent
  restarts its running managed service with the unchanged command, cwd and env, e.g. after editing
  its code. No owner approval (the grant that let the command run covers its restarts); same
  ownership rule as `service_update` / `service_unregister`. External / static services answer
  409 `not_managed`, a service the owner stopped 409 `service_stopped` (only the owner starts it
  again). The Agent Bus records `restartRequestedAt` on the registry record; the supervisor in
  Management serve restarts it on its next pass through the owner's restart path (kill, wait for
  the port, start; restart budget reset). The call returns once the request is recorded.
  智能体可用 `service_restart` 重启自己的托管服务（命令不变，无需审批）。
- `service_update` no longer asks the owner again when the effective runtime, command, cwd and env
  (after the env merge) equal the stored record (as an identical re-register already did); any
  change of these is still gated.
  `service_update` 的运行时、命令、cwd 和环境变量与已存记录相同时不再重复请求审批。
- A service unregistered (or reclaimed) and registered again under the same slug no longer shows
  the old service's `lastTrafficAt`: traffic from before a record's `createdAt` is ignored, the
  supervisor drops the slug's traffic counters on removal, and a record replaced between two
  supervisor passes starts with a fresh process / health / traffic entry. Ephemeral idle reclaim
  is unchanged.
  同名服务注销后重新注册，不再继承旧服务的最近流量时间。

**Sessions and agents**

- Session resources (`GET /v2/sessions` items and single sessions) carry
  `runtime.pendingServerRequests: [{id, method}]` (capability `session-pending-server-requests`):
  the native requests of the current runtime generation (approvals, questions, ...) still waiting
  for an answer, oldest first, empty when none. A client that connects after a request was sent
  now sees it without replaying the event stream; the full messages stay in the session bootstrap.
  A request still pending when its turn completes is now marked resolved.
  会话带上仍在等待回答的原生请求（审批、提问等），后连接的客户端无需回放事件流即可看到。
- Session conversation activity (capability `session-conversation-activity`, `sessionActivity`
  version 3): session resources carry `activity.lastConversationAt`, the last real conversation
  activity (a turn started, agent output, a tool call, a file change, or a turn that ended other
  than interrupted), and `activity.lastTurnStartedAt`, both kept in `session-activity-state.json`. A
  runtime start, stop, restart or reconnect, the interrupt of a turn a restart left in progress, and
  session metadata / profile / settings changes never move it; stores written by older binaries
  derive it from their turn and item times until the next event. `/v2/summary`
  `agents[].lastActivityAt` now follows it (then a non-interrupted last turn end, then `createdAt`)
  instead of the runtime's `lastEventAt` / `updatedAt`, which a restart or a rename refreshed.
  会话新增 lastConversationAt（只算真实对话活动），摘要的 lastActivityAt 不再因运行时重启、重连或改名而刷新。
- An archived (closed) Agent's name can be used again: creating an Agent with it failed after the
  native thread existed (`session_adopt_persistence_unknown: agent name was concurrently assigned to
  another record`) and left the name reserved. Names stay unique among live Agents; restoring an
  archived Agent while a live one holds its name is refused with `agent_name_taken` (409).
  已归档智能体的名字可以重新使用；恢复归档智能体时若名字已被占用则拒绝。
- A `member_replace` that stopped before its successor existed (for example while checking the
  predecessor) no longer keeps the successor name reserved for good: the next request for that name
  in the same Project releases it (`reservation_released_pre_effect`) instead of failing with
  `unresolved_agent_reservation`.
  中途停止的 `member_replace` 不再永久占用继任者的名字。
- r244 acceptance (Windows): a Director's `member_runtime restart` and `member_replace` of a Claude
  member failed with `runtime recovery mismatch: cwd` / `managed Agent observation mismatch: cwd`.
  Claude agents stored the Win32 verbatim form of their directory (`\\?\F:\x`, from
  `canonicalize()`), the adapter reports that form in every settings update, and that text replaced
  the agent's managed cwd (`F:\x`). Managed cwd checks now treat a verbatim spelling as the same
  directory (also for the derived cwd group), settings updates no longer rewrite a managed cwd that
  names the same directory, and new Claude agents (and `human config` cwd changes) store the plain
  form. Claude permission rules saved under a verbatim workspace key are found under the plain one.
  Windows：修复 Director 重启或替换 Claude 成员时报 cwd 不一致（`\\?\` 路径形式）。
- An agent launch (`cutex run --agent`, session runtimes) no longer turns the Agent Bus on in a
  home whose `config.json` has `agent_bus_enabled: false`: the management check before the launch
  forced the switch on, so the agent-mode bus check always passed and the launch probed or
  started a bus anyway. Homes with the bus enabled behave as before.
  智能体启动不再强行打开已关闭的 Agent Bus 开关。

**Delegations and agent tools**

- Delegation review (`ask_delivery`): a Claude agent's tool switches count as capabilities. A
  Claude requester with command execution off is reviewed when it delegates to an agent that can run
  commands (every Codex agent; difference `commandExecution`), and one with neither file editing nor
  command execution when the target can write files (`fileWrite`). Before, its "Full access" /
  "Auto" label made it look like it could do everything, so such delegations went straight to the
  queue. Other agents' capability digests are unchanged.
  关闭执行命令的 Claude 智能体委托能执行命令的智能体时，现在会进入待审核。
- Delegation gate (`ask_delivery`): the target's own cutex-assigned scratch cwd
  (`<cutex home>/agents-scratch/<id>` or `<cutex home>/chats/<id>`, used by no other session) is no
  longer compared with the requester's writable roots, so two cwd-less agents of one project can
  delegate to each other without a review per ask. Every other root, including any other path in
  the cutex home, is compared as before (`docs/agent-tool-security.md` §7).
  两个无工作目录（使用私有 scratch 目录）的智能体之间的委托不再每次都需要审批。
- Delegation results that arrive together are listed in the order they arrived (by delivery time),
  in the coalesced message and in `delegation_wait`; before, a later result could come first.
  同一批送达的委托结果按到达顺序排列。
- `agent_list` stays small with a paired host (one with hundreds of offline agents made it about
  50K characters, over Claude Code's tool-result limit). Local agents: own project first, offline
  last, at most `limit` (default 50, max 200) per call with `next_cursor` / `cursor`, plus
  `agents_total` and `agents_offline`. `otherHosts[]`: per host `agentCount`, `offlineCount`,
  `listedCount`, at most 10 idle / busy agents and `more` (the call that lists the rest);
  `host: "<label>"` pages one host's agents, `include_offline: true` includes offline ones. Row
  fields are unchanged.
  `agent_list` 默认输出变得紧凑：其他主机的离线智能体只计数，可按主机分页查看。
- Browser scroll outcome (capability `browser-scroll-outcome`): a `browser_act` wheel scroll result
  may carry `scroll {dx, dy, atEdge}` from the desktop, the CSS px the scrolled container moved and
  whether it is now at the edge in the requested direction (for example the bottom after `down`).
  cutex validates the field strictly and passes it to the agent; desktops send it only to a host
  that advertises the capability.
  browser_act 的滚动结果可附带实际滚动距离和是否已到边缘（scroll {dx, dy, atEdge}）。
- MCP façade (`cutex-mcp`): `resources/list`, `resources/templates/list` and `prompts/list` now
  answer empty lists instead of the generic -32602 "Cutex rejected operation…" error (Codex's
  `list_mcp_resources` calls them without checking capabilities; `initialize` still advertises only
  `tools`). Other unknown request methods answer JSON-RPC -32601 "Method not found".
  MCP 外观对 resources / prompts 列表请求返回空列表，不再报错；其他未知方法返回「Method not found」。

**Proxy**

- Proxy settings v1 (capabilities `proxy-modes`, `profile-proxy`). `/v2/settings` `proxy` now has a
  `mode`: `system` (nothing stored: the daemon's environment, then on Windows the per-user system
  proxy from the registry `ProxyServer` / `ProxyOverride`; no PAC / WPAD — PAC users enter a manual
  URL), `none` or `manual` (URL with optional Basic `user:password@`, plus `noProxy`). The view adds
  `system` (what "follow system" resolves to right now and from where), `loopbackNoProxy` (always
  bypassed, not editable), `editable` and a `warning` for SOCKS URLs; credentials are never
  returned. It is the default every Profile inherits and, new, the proxy of cutex's own requests:
  backend upgrades and component downloads, the LiteLLM price catalog, webhooks and the official
  Claude usage reader (all through one helper; loopback always direct). `/v2/profiles` items carry
  `proxy` {`mode` inherit | none | manual, redacted URL, `effective` {mode, url, source}}; PATCH
  `proxy` {mode, url?, noProxy?} | null. A Profile's effective proxy now also reaches its Claude
  runtimes (the adapter and Claude CLI get `HTTPS_PROXY` / `NO_PROXY`; follow-system on Windows hands
  over the registry proxy, which the Claude CLI does not read itself), its quota readers, model
  discovery and `web_search`. An explicit "no proxy" sets `NO_PROXY=*,0.0.0.0/0,::/0` as well, so
  runtimes that fall back to the Windows system proxy when `HTTP(S)_PROXY` is empty connect directly.
  Only the root credential may change proxies (a client / device token gets 403
  `proxy_owner_credential_required`); a proxy that wants credentials reads "the proxy requires
  authentication (HTTP 407)". SOCKS URLs only reach the runtimes (cutex's own HTTP client has no
  SOCKS support and refuses them instead of silently going direct). Retired: `forceHttpTransport`
  (API 400), `--force-http` / `--proxy-force-http` and `CUTE_CODEX_FORCE_HTTP_TRANSPORT` (no native
  read it); stored `force_http_transport` keys are ignored and dropped on the next save. `cutex proxy
  show` redacts proxy credentials.
  代理设置 v1：主机代理可选跟随系统 / 不使用 / 手动，同时作为 Profile 的默认值和 cutex 自身下载（升级、组件、
  价格目录、webhook）的代理；Profile 可单独设置（继承 / 不使用 / 手动），并用于其 Claude 运行时、额度读取、
  模型发现和 web_search；只有主机自己的凭据能修改。
- Proxy settings: switching the host proxy (`/v2/settings` `proxy`) or a Profile's proxy
  (`/v2/profiles` `proxy`) to `mode: none` no longer discards the manual address. It is stored with
  the setting turned off (`enabled: false`, `url`, `noProxy`), still returned (redacted) in the
  view, and switching back to `manual` without a `url` restores it. While off it is never used:
  runtimes still get the explicit direct-connection variables and cutex's own requests go direct.
  `url: null` forgets it; `system` / `inherit` still store nothing.
  代理切换为“不使用”时保留之前填写的手动代理地址，切回“手动”即可恢复。
- Loopback traffic never goes through a proxy. cutex and cutex-mcp add `127.0.0.1,localhost,::1`
  to `NO_PROXY` / `no_proxy` at start (inherited by native runtimes, the Claude CLI and tools), and
  the environment cutex builds from a configured global / Profile proxy always carries them too
  (before, a proxy with no NO_PROXY sent the runtimes' own calls to cutex — the Claude MCP binding,
  streamable-HTTP MCP servers — and requests to a model server on 127.0.0.1 through the proxy).
  回环地址不再走代理：cutex 启动时把 127.0.0.1、localhost、::1 加入 NO_PROXY，配置的全局 / Profile 代理同样始终带上。

**Upgrade and components**

- Rollback to an older cutex no longer leaves it unable to read its stores. While the services are
  stopped, the rollback removes approval records of kind `static_root` (an older release reported
  the whole approval store as corrupt) and clears a Claude agent's pending launch permission mode
  (set by `claude-launch-permission-mode` until the agent's first start; an older release could not
  load any session). Both are copied to the run's `displaced/older-stores/` first. Upgrade backups
  now also cover `claude-permission-rules.json`, `claude-skills/`, `runtime/profile-avatars/` and
  `runtime/live-thread-settings/`.
  回退到旧版 cutex 时，会先移走旧版无法读取的 static_root 审批记录并清除 Claude 智能体待生效的启动权限模式
  （另存到本次运行的 displaced 目录），旧版不再因此无法读取审批或会话；升级备份也包含新增的存储。
- `GET /v2/upgrade` `installed` gains `notes` (`{en, zh-CN}` as in the release manifest) and
  `publishedAt`, read from the installed release's committed manifest
  (`releases/<release>/manifest.json` under the install base); null when unknown (unmanaged
  installs, a release without a manifest). Capability `upgrade-installed-notes`.
  已安装版本也带上发布说明与发布时间。
- Components: claude-runtime claude-r19 (adapter 0.7.6; the entries above) and job-service 0.1.1.
  job-service 0.1.1 changes no behaviour: its archive now ships LICENSE, NOTICE and
  THIRD_PARTY_NOTICES.md (Apache-2.0, like the rest of cutex), and the claude-runtime archive ships
  THIRD_PARTY_NOTICES.md for its vendored wheels. No new cute-codex native (stays 0.160.0-r52.3).
  组件：claude-runtime claude-r19、job-service 0.1.1（行为不变，只是压缩包附带许可证与第三方声明）；
  没有新的 cute-codex 原生运行时。

**Platforms**

- macOS (Apple silicon) as a backend host, MVP level: cutex builds and runs there with Codex
  agents on a Mac-built native (Seatbelt `workspace-write` sandbox) and Claude agents. Process
  identity comes from libproc instead of `/proc`; stop and restart signal the runtime's whole
  process family (process group, session, parent tree) since there is no cgroup; the follow-system
  proxy reads `scutil --proxy` (source `macosSystem`); new platform key `darwin-arm64` (manifests,
  components, upgrades; `claude-runtime` lists it); `cutex service install` gains a launchd backend
  (per-user agents `dev.cutex.agent-bus` / `dev.cutex.management-api`; generated and unit-tested,
  not yet exercised on a real Mac). Job Service, NCI and inbound mail stay Linux-only. No release
  artifacts for macOS yet; see `docs/macos/PROBE.md` and `docs/macos/MVP.md`. Linux and Windows
  behaviour is unchanged.
  macOS（Apple 芯片）可作为后端主机运行（MVP）：Codex 与 Claude 智能体均可在 Mac 上运行，尚无正式发布包。
- Test port guard (debug builds only): every process the test-home guard covers (cargo test
  harnesses, debug `cutex` with `CUTEX_TEST_FORBID_REAL_HOME`) and every debug process on a
  `CUTEX_TEST_PRIVATE_HOME` follows the instance port rules. It never resolves, accepts, connects
  to, binds or hands off a default-instance port (24270, 24260, 24670, 24660, 24250): an unset or
  stored default bus / management port resolves to 0, `ensure_agent_bus_config` gives the home a
  free bus port of its own and keeps it in `config.json`, the shared HTTP / bus connect helpers
  and the management listeners refuse those ports, and no Bridgeboard handoff is registered.
  Integration tests that ran `cutex run --agent` could reach or spawn the machine's live bus on
  24260 before. Release builds are unchanged.
  调试构建的测试进程不再连接、注册或占用默认的 24260 / 24270 等端口。

## 0.1.3 "First Take" (2026-10-17)

New cute-codex native 0.160.0-r52.3 (Windows: Store PowerShell works under the sandbox without
administrator rights), network access and extra writable folders changeable on a running session, agent
browser tools in independent instances, and usage fixes. 新的 cute-codex 原生运行时 0.160.0-r52.3（Windows
免管理员沙箱下商店版 PowerShell 可用）；运行中会话的联网与额外可写目录可以修改；独立实例里的智能体浏览器工具可用；
用量估价与按模型统计修复。

**Native runtime**

- New native cute-codex 0.160.0-r52.3: Windows sandbox falls back from Store PowerShell. Under the
  sandbox without administrator rights (restricted token), the Microsoft Store PowerShell cannot start
  (commands failed with exit -1); native now falls back to an installed `pwsh` or Windows PowerShell,
  as the elevated sandbox already did. The backend update stages the native; new agents use it, idle
  agents move over after the update. 新原生运行时 0.160.0-r52.3：Windows 免管理员沙箱下不再启动商店版
  PowerShell（之前命令以 exit -1 失败），改用已安装的 pwsh 或 Windows PowerShell。

**Permissions**

- Network access and extra writable roots of a running session (capability `session-sandbox-extras`,
  owner only): `GET|PATCH /v2/sessions/{id}/sandbox-extras` with `{networkAccess?, writableRoots?}`.
  The values are stored on the session record — every start / resume / relaunch builds the
  workspace-write sandbox from it, so they survive runtime restart, relaunch and daemon restart — and
  pushed to a connected runtime with native `thread/settings/update` (applies from the next turn;
  answer `live.status` = `applied` | `offline` | `failed`). Workspace-write Codex sessions only (409
  `sandbox_not_workspace_write` / `unsupported_runtime`); roots must be existing absolute directories
  on the host, at most 32, never the cutex home or inside it (400 `invalid_writable_root` /
  `writable_root_in_cutex_home`). 运行中会话的联网与额外可写目录可以修改并保存在会话上，重启后保留。

**Browser**

- Agent browser tools in an independent instance (`CUTEX_HOME`) or any non-default home: the cutex MCP
  tool server now gets its own daemon's Management URL at launch. Before, it used the default
  `~/.cutex` daemon's port, so every `browser_*` call was refused with `permission_denied` (and went to
  the wrong daemon). Existing agents pick it up at their next runtime restart. 独立实例里智能体的浏览器
  工具可以用了（之前连到默认 home 的 daemon，全部被拒）。

**Usage**

- Usage cost: models whose provider the price library has no name for (a Claude account routed to
  Z.ai, a custom provider id) are priced by the model's own family, e.g. `GLM-5.3` / `GLM-5.3-Flash`
  via `zai/…`. Before they showed as unpriced. 用量估价：Claude 账号走 Z.ai、自定义提供方名时，按模型名
  所属系列查价（GLM-5.3 等不再显示无价格）。
- Usage by model: one model reported in two spellings (the Claude runtime's `GLM-5.3`, Codex's
  `glm-5.3`) is one row in `GET /v2/usage?groupBy=model` and one `unpricedModels` entry, keyed by the
  lowercase id. Stored samples keep their spelling. 用量按模型：大小写不同的同一模型合并为一行（显示小写 id）。

## 0.1.2 "First Take" (2026-10-17)

Codex's automatic reviewer for "Auto", Windows sandbox readiness, Profile transfer between hosts, and
fixes for Windows API-key Profiles, forked agents and artifact previews. No new cute-codex native:
new agents get this release's tools with the host's current native. 「自动」使用 Codex 自动审批员；
Windows 沙箱就绪检查；Profile 可在主机之间复制；修复 Windows 上的 API-key Profile、分叉智能体删除和成果预览。

**Permissions and approvals**

- Automatic approval review for new threads (capability `launch-approvals-reviewer`):
  `launch.approvalsReviewer` (`user` | `auto_review`) on `POST /v2/human/agents` /
  `/v2/human/sessions`, `newThreadDefaults.<kind>.approvalsReviewer` (built-in `auto_review` for agents
  and quick chats), stored on the session, sent on every thread start / resume and kept in sync with
  `thread/settings/updated`. The desktop's "Auto" mode now means workspace-write + on-request +
  Codex's automatic reviewer. See [docs/FRESH-AGENT-PERMISSIONS.md](docs/FRESH-AGENT-PERMISSIONS.md).
  新线程默认使用 Codex 自动审批员。
- Automatic reviewer on API-key profiles without a model catalog on their own provider (relays): the
  reviewer now reviews with the agent's own model instead of the bundled OpenAI review model, which
  such endpoints do not serve (every escalation used to be denied). Launches pass a generated
  `reviewer-models-<sha256>.json` catalog. 无模型目录的 API-key Profile：自动审批员改用 agent 自身模型，不再全部拒绝。
- Network access and extra writable directories changed on a running thread (`thread/settings/update`
  with a workspace-write `sandboxPolicy`) are stored on the session like the approval policy and
  reviewer, so the next resume / relaunch keeps them. A named permission profile keeps defining its
  own. 运行中修改的联网与额外可写目录在恢复 / 重启后保留。
- Windows sandbox readiness (capability `windows-sandbox-readiness`): the owner can ask an agent's
  runtime, or the shared Codex home, whether a Windows sandbox is set up (native
  `windowsSandbox/readiness`), and set up the unelevated one (`windowsSandbox/setupStart`
  `{mode: "unelevated"}`; no administrator rights). Without a sandbox native asks before every
  command in Auto / workspace-write. Elevated setup (UAC, local sandbox users, firewall rules) stays a
  manual step on that host. Windows 沙箱就绪检查与免管理员设置。

**Profiles**

- Copy a profile to another host (capability `profile-transfer`): `POST /v2/profiles/{p}/export
  {includeCredentials?}` returns a bundle (schemaVersion 1: settings, the profile part of config.toml,
  the effective model catalog and overlay) and `POST /v2/profiles/import {bundle, name}` creates a NEW
  profile from it (never overwrites: 409 `profile_name_conflict`; 400 `unsupported_bundle` for a bundle
  this cutex does not read). Only an API-key profile's key can travel, only on request and only over a
  loopback connection (this machine or an SSH tunnel; otherwise 403 `credentials_loopback_only`);
  without it, proxy passwords, provider bearer tokens / HTTP headers and `shell_environment_policy.set`
  are dropped too. A ChatGPT login is never copied (log in again on the target), a Claude profile binds
  the target's Claude account with the same label and kind (else the default account), the private
  ChatGPT auth file switch is reset; the import answers `warnings` (`needsLogin`,
  `claudeAccountRebound`, `privateAuthReset`, `runtimeDockerImage`, `proxyCleared`, `proxyCopied`,
  `secretFieldsCleared`). Exports and imports are audited without values in
  `runtime/profile-transfer/audit.jsonl`. Profile 可复制到其他主机（只带设置，API key 可选，ChatGPT 需重新登录）。
- `cutex profile copy` no longer copies credentials: a ChatGPT login is never copied (two profiles
  sharing one rotating refresh token log each other out) and an API key only with the new
  `--with-api-key`. The copy's email / ChatGPT plan are cleared; log the copy in or set its key
  afterwards. `cutex profile copy` 不再复制登录凭据。
- Kimi Code membership plan (capability `kimi-code-plan`): new API-key presets `kimi-code`
  (api.kimi.ai) and `kimi-code-cn` (api.kimi.com) on the Kimi Code Responses API `/coding/v1`, with
  the Kimi Code model catalog (`kimi-for-coding` default, `k3-256k`, `k3`,
  `kimi-for-coding-highspeed`) and web search on. New quota reader `kimi_code_plan` reads
  `<base URL>/usages` (the endpoint behind the official Kimi CLI's `/usage`): the 5 h window, plus
  the weekly window (legacy plans) and the monthly total on profiles created from the presets.
  Quota configs gain mode `used_ratio` (used share 0..1) and `optional` (a window the response
  never carried is not shown). Unverified against a live plan. Kimi Code 会员套餐：预设与 5 小时 / 每周 / 本月额度读取。

**In-app browser (desktop 0.1.3)**

- Capability `browser-refusal-reasons`: the desktop's precise refusal reasons (`external_pages_off`,
  `external_proxy_unavailable`, `private_address_blocked`, `reserved_address`, `protected_endpoint`,
  `remote_loopback`) reach the agent, each with a fixed hint, instead of a generic failure; the
  `browser_open` description says public websites open while the owner allows them and LAN addresses
  never open for agents. 智能体得到内置浏览器的具体拒绝原因。
- Capability `browser-popup-grant`: a tab the leased agent's own click opened is granted to the same
  agent (grant source `{kind: "popup", openerLeaseId}`) without a new owner approval, only while the
  opener lease is active for that agent on the same device connection; it inherits the opener's
  origin. 智能体点开的新标签沿用原授权。

**Backend updates**

- Upgrade runs record whether they bring a new cute-codex native (capability `upgrade-run-native`):
  run records in `GET /v2/upgrade`, `GET /v2/upgrade/runs[/{id}]` and the new
  `cutex upgrade history --json` carry `nativeVersion`, `previousNativeVersion` and `runtimeChange`
  (`native` | `package` | `none`), so a finished run still says whether new agents get a new native
  runtime or only a new runtime package; older records show null. 升级记录注明是否带来新的 native 运行时。

**Fixes**

- Windows: an API-key Profile could not start agents after its settings were saved
  (`unsupported_profile: Validate profile credential file …\auth.json: object DACL is not
  service-private`). Owner-private atomic writes (used by `PATCH /v2/profiles/{p}`) created their temp
  file with the parent's inherited ACE and no protected DACL, which the launch credential check
  refuses. They now create it with the protected owner-only DACL. Existing installs recover on their
  own: the credential check re-protects a profile `auth.json` whose ACEs all grant only this account
  (e.g. one inherited `(I)(F)` ACE, also left by a native token refresh); a file shared with any other
  account is still refused. 修复 Windows 上保存 API key 的 Profile 后无法创建智能体（auth.json 权限），已有安装自动修复。
- Deleting an agent that had been forked was refused with `session_referenced`
  (`nativeForkReferences`), also after the fork itself was deleted: `cutex/session/fork` left a native
  copy of the fork behind in the source's home. The fork now deletes that copy once adopted, and a
  delete refused only by such left-behind copies (homes written before this fix included) deletes them
  (backed up under the delete's recovery `fork-copies/`) and retries. A fork that still lives on the
  same home's history keeps blocking, now with a fork-specific message. 删除被分叉过的智能体不再被已删除的分叉挡住。
- Artifacts published under a label name (no extension) got the wrong media type, so the desktop
  showed an HTML page as raw text and a PDF / JPEG as an unpreviewable `application/octet-stream`. The
  media type now comes from the source file's extension first (`/` and `\` paths alike), then the
  name's, then the content's signature (PDF, PNG, JPEG, GIF, WebP, an HTML document start). Stored
  versions are not rewritten (the desktop types generic ones by the source path).
  修复以无扩展名名称发布的成果媒体类型错误（HTML 显示为源码、PDF/JPG 无法预览）。
- A fresh home: the Windows sandbox status (and the other Codex settings reads) failed with
  `codex_app_server_unavailable` / `shared CODEX_HOME does not exist`. The shared `<home>/codex-home`
  is now created owner-private on demand when no runtime is selected (and by
  `cutex human install-runtime` without `--source-home`), so readiness answers `notConfigured`.
- Natives no longer inherit another cutex home's temporary directory: a cutex started from an agent
  shell (whose `TMPDIR` / `TEMP` / `TMP` are its home's `runtime/stock-tmp`), e.g. an independent
  instance, passed that directory on to its own natives. Such a `TMPDIR` is now replaced by this
  home's own (an explicit other `TMPDIR` stays supported).
- Workspace terminals (Linux, macOS): closing a terminal always ends it. A shell that survives the
  hangup is killed with its process group after 1.5 s, and a terminal no longer stays "running" while
  another process still holds its pty. 关闭终端一定会结束它。
- Static services: `not_supervised` names the runtime (static / external) that has no process to stop.

## 0.1.1 "First Take" (2026-10-16)

Optional components and a notification worker for managed installs. 可选组件；托管安装自带通知 worker。

- Optional components: cutex installs, updates and removes backend pieces on demand, on managed
  installs (`cutex component list|status|install|remove|prune`, `GET /v2/components`, capability
  `components`). Versions sit side by side; agents keep the version they started with. A backend
  update (`cutex upgrade` / the desktop) brings installed components to the versions of the new
  release and a rollback selects the previous ones; a component failure never fails the cutex
  update. See [components/README.md](components/README.md). 可选组件按需安装，并随后端更新。
  - `claude-runtime` (Linux, Windows): the Claude agent runtime (formerly `claude-adapter`), set up
    offline from the wheels in its archive. It needs Python 3.11+ and your own Claude Code CLI and
    login; until then `status` says `needsUser` with the steps.
  - `job-service` (Linux): long-running agent jobs (the desktop's Jobs page), run as the systemd user
    unit `cutex-job-service.service`; it never restarts while a job runs.
- Managed installs run the notification worker (webhooks, notification states) inside the
  Management API, so `cutex notify run` is no longer needed there; `cutex service status` reports
  `notificationWorker`. Hand-deployed hosts keep their own `cutex notify run`.
- Claude Profiles: setting `model` / `modelReasoningEffort` sets the default for new sessions
  (capability `claude-profile-default-model`).
- Windows: the logon task runs at normal priority (cutex and every agent ran below normal), and
  `cutex human service` starts and stops the managed install's services.
- Source installs (release label `dev-<commit>`): a rollback after a failed update, or to a `dev-*`
  release, no longer ends in "needs owner" although everything was restored.
- Lower idle CPU in the ask scheduler and the Agent Bus delivery sweep.
- NCI (Gadi) remote jobs work on Linux hosts with the `job-service` component: cutex tracks a PBS
  job to its end with `qstat` over a dedicated, restricted SSH key; PBS mail is optional and only
  makes it check sooner. Checked against a real Gadi account. See
  [docs/NCI-REMOTE-JOBS.md](docs/NCI-REMOTE-JOBS.md). NCI 远程作业可用（Linux 主机）。
- Claude agents with full access: a Claude Code safety check no longer waits for an approval. The
  tool call is declined with the reason so the agent can do it another way or stop and explain; the
  desktop shows a "needs you" item where the owner may allow that exact call once (30 minutes, owner
  only). Capability `claude-safety-grants`; Claude runtime `claude-r14`. See
  [docs/SAFETY-GRANTS.md](docs/SAFETY-GRANTS.md). 完全访问下的安全检查改为驳回并可由主人批准一次。
- Delegations: an ask attempt ends once, so a result is answered and delivered once; duplicates
  stored by earlier versions show as one. 委托结果不再重复投递。
- Paired peers share their owner's display name and avatar: a user on this host that holds a
  paired peer owner's person key shows that owner's name and avatar even when the person's desktop
  never connected here (capability `peer-owner-profile`). A name or avatar set or cleared here
  directly always wins. `GET /v2/users` items show `profileSource`; host event
  `cutex/users/changed`. See [docs/PEERS.md](docs/PEERS.md#owner-profiles). 对端同步主人的显示名与头像。

## 0.1.0 "First Take" (2026-10-15)

The first public release of cutex. 第一个公开版本。

- Agents on cute-codex or Claude Code, managed through the Management v2 HTTP / SSE API, a CLI and
  a session TUI.
- Collaboration: Agent Bus messages, delegations and questions between agents, projects with
  optional Directors, alarms, owner notifications and approvals, workflows, notes and side chats.
- Cross-host peers (off until paired), users and devices with their own tokens.
- Agent tools: services with an authenticated proxy, artifacts, the desktop's in-app browser.
- Profiles for ChatGPT, Claude and OpenAI-compatible APIs, with quota, plan, usage and cost.
- Signed release binaries for Linux x64 and Windows x64 (minisign, key built in). A managed install
  (`cutex service install`) updates itself from signed releases with `cutex upgrade` or the
  desktop's backend updates (`docs/backend-upgrade.md`); source builds follow the `release` branch.
