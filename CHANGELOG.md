# Changelog

## 0.2.1 "Beautiful World" (2026-11-06)

- Agents can change their formal names while preserving identity and name history.
- Persistent browser scripting sessions, user layout memory and service idle sleep.
- Durable session, communication and usage stores move to SQLite. This update requires explicit one-way confirmation; retain an offline backup before upgrading.
- Message delivery, queued delegations, busy/idle turn handling and startup adoption are more reliable.
- Native 0.164.0-r59; optional Claude claude-r40 with bundled Python, DSH dsh-r3 and Kimi kimi-r3. DSH/Kimi adapters use separately installed upstream CLIs. Existing agents keep their pinned runtimes until explicitly upgraded.

- 智能体可改名，身份与名字历史保持不变；提供浏览器脚本会话、个人布局记忆及服务空闲休眠。
- 会话、通信与用量的持久状态迁入 SQLite；本次更新需明确确认单向升级，升级前请保留离线备份。
- 改进消息投递、排队委托、忙碌/空闲回合处理及启动接管。
- 原生运行时 0.164.0-r59；可选组件 Claude claude-r40（自带 Python）、DSH dsh-r3、Kimi kimi-r3。DSH/Kimi 的上游 CLI 需另行安装。已有智能体保持原来的运行时，直到明确升级。

## 0.2.0 "Beautiful World" (2026-10-29)

- Improves persistent agent communication, delegation and project services.
- Adds account-aware Profiles and runtime variants, native runtime 0.164.0 (r58),
  and Claude runtime claude-r32 with bundled Python and pinned dependencies.
- Kimi and DSH are experimental and are not included in the 0.2.0 binaries; available through a later public component update.
- Upgrading from 0.1.5 changes durable agent communication to format v2.
  This is a one-way update: older releases require an offline restoration;
  automatic rollback across the migration is unavailable.

- 改进持久化智能体通信、委托和项目服务。
- 支持按账户管理的 Profile 和运行时变体，提供原生运行时 0.164.0 (r58)，
  Claude 运行时 claude-r32 自带 Python 和固定依赖。
- Kimi 和 DSH 为实验功能，不包含在 0.2.0 二进制发行中，随后续公开更新提供。
- 从 0.1.5 升级会将持久化智能体通信改为格式 v2。
  这是不可自动回退的更新；使用旧版本前须先离线恢复数据。

- Kimi runtime limitations: Linux lacks the node-pty binding, so interactive terminal tools are unavailable; offline Glob/Grep require system ripgrep (`rg`).

## 0.1.5 "First Take" (2026-10-20)

- Owner devices: every new device bound to the owner is announced until you acknowledge it, actions that change who controls the host work only on the host or over SSH, and each device can be limited to chat or read. Claude agents: Side chats and forks while busy, waiting notes read at the start of every turn, context usage kept after reconnecting, a changed system prompt reaches resumed sessions (Claude runtime claude-r21). A cross-host message can wake the other agent when its host allows it. Home pages and remote close of a desktop live on the host; Profiles import from CC Switch; on Windows every agent runtime runs in its own Job object. A rollback never gives a limited device more rights. New agents start with this release's tools; idle agents move over after the update.
  主人的设备：每台新的主人设备都会通知，直到你确认；改变主机归属的操作只能在主机本机或通过 SSH 做；每台设备可限制为只聊天或只查看。Claude 智能体：侧聊和忙碌时分叉、每一轮开头读入待读留言、重新连接后保留上下文用量、恢复会话时使用更新后的系统提示（Claude 运行时 claude-r21）。主机允许时，跨主机留言可以唤醒对方的智能体。主页和远程关闭桌面保存在主机上；可从 CC Switch 导入 Profile；Windows 上每个智能体运行时放进自己的 Job 对象。回退不会让受限设备获得更多权限。新智能体使用本版本的工具，空闲的智能体会在更新后自动迁移。

## 0.1.4 "First Take" (2026-10-19)

- Claude agents: launch permission modes (Auto does not decide cutex approvals), Claude runtime claude-r19, context windows and effort levels from the runtime, repin to the selected Claude runtime from the app, skills import, subagent transcripts. Profile pictures, avatar themes and greetings are stored on the host; agents' pending requests are visible after reconnecting; approvals record who decided; proxy modes per Profile; agents can use browser tabs in the background. New agents start with this release's tools; idle agents move over after the update.
  Claude 智能体：启动时可选权限模式（「自动」不代替 cutex 审批）、Claude 运行时 claude-r19、上下文窗口和思考强度来自运行时、可在应用里迁到所选 Claude 运行时、导入技能、查看子智能体记录。Profile 头像、头像主题和开场白保存在主机上；重新连接后能看到智能体待处理的请求；审批记录由谁决定；Profile 可单独设置代理模式；智能体可在后台使用浏览器标签页。新智能体使用本版本的工具，空闲的智能体会在更新后自动迁移。

## 0.1.3 "First Take" (2026-10-17)

- New native runtime cute-codex 0.160.0-r52.3: under the Windows sandbox without administrator rights, commands work when PowerShell is the Microsoft Store version. Agent browser tools work in independent instances (non-default cutex homes); usage cost for models priced by their family (e.g. GLM-5.3 through a Claude account or a custom provider). New agents start with the new native and this release's tools; idle agents move over after the update.
  新的原生运行时 cute-codex 0.160.0-r52.3：Windows 免管理员沙箱下，商店版 PowerShell 也能正常运行命令。独立实例（非默认 cutex home）里的智能体可以使用浏览器工具；按模型系列估算用量费用（如通过 Claude 账号或自定义提供方使用的 GLM-5.3）。新智能体使用新的原生运行时和本版本的工具，空闲的智能体会在更新后自动迁移。

## 0.1.2 "First Take" (2026-10-17)

- The desktop's Auto mode uses Codex's automatic reviewer for escalations; Windows sandbox readiness and a one-click setup without administrator rights; copy a Profile to another host; precise in-app browser refusal reasons for agents; fixes: Windows API-key Profiles that could not start agents (repaired automatically), deleting agents that had been forked, previews of artifacts published under a label name. New agents start with this release's tools; idle agents move over after the update.
  桌面端的「自动」模式由 Codex 自动审批员处理越界操作；Windows 沙箱就绪检查与免管理员一键设置；Profile 可复制到其他主机；智能体能得到内置浏览器的具体拒绝原因；修复：Windows 上无法创建智能体的 API-key Profile（自动修复）、删除被分叉过的智能体、以名称发布的成果无法预览。新智能体使用本版本的工具，空闲的智能体会在更新后自动迁移。

## 0.1.1 "First Take" (2026-10-16)

- Optional components (claude-runtime, job-service on Linux) installed and updated on demand; the notification worker runs inside the Management API of managed installs; NCI remote jobs on Linux; Claude safety checks under full access can be granted once by the owner; delegation results are delivered once. New agents start with this release's tools; idle agents move over after the update.
  可选组件（claude-runtime；Linux 上的 job-service）按需安装并随后端更新；托管安装自带通知 worker；Linux 主机支持 NCI 远程作业；完全访问下的 Claude 安全检查可由主人批准一次；委托结果只投递一次。新智能体使用本版本的工具，空闲的智能体会在更新后自动迁移。

## 0.1.0 "First Take" (2026-10-15)

- First public release of persistent agent sessions and the managed host update channel.
  首个公开版本，提供持久智能体会话和托管主机更新频道。
