# Changelog

## Unreleased

## 0.1.2 (2026-10-17)

Codex's automatic reviewer for 「自动」, Windows sandbox readiness, Profile transfer between hosts, and
fixes for Windows API-key Profiles, forked agents and artifact previews. No new cute-codex native:
new agents get this release's tools with the host's current native. 「自动」使用 Codex 自动审批员；
Windows 沙箱就绪检查；Profile 可在主机之间复制；修复 Windows API-key Profile、分叉智能体删除和成果预览。

**Permissions and approvals**

- Automatic approval review for new threads (capability `launch-approvals-reviewer`):
  `launch.approvalsReviewer` (`user` | `auto_review`) on `POST /v2/human/agents` /
  `/v2/human/sessions`, `newThreadDefaults.<kind>.approvalsReviewer` (built-in `auto_review` for agents
  and quick chats), stored on the session, sent on every thread start / resume and kept in sync with
  `thread/settings/updated`. The desktop's 「自动」 mode now means workspace-write + on-request +
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
  command in 自动 / workspace-write. Elevated setup (UAC, local sandbox users, firewall rules) stays a
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
  account is still refused. 修复 Windows 上保存 API key Profile 后无法创建智能体（auth.json 权限），已有安装自动修复。
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

## 0.1.1 (2026-10-16)

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
