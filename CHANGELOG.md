# Changelog

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
