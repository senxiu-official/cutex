## 0.2.6 "Beautiful World"

Adds personal assistants and recurring tasks, project controls and delivery recovery. Includes Native 0.172.0-r70 and Claude claude-r59. Job Service 0.1.3 is reused without changing its signed bytes. Existing agents retain their selected runtime.

**One-way upgrade from 0.2.5:** after 0.2.6 has started, rollback to 0.2.5 is unsupported. Keep a complete, consistent offline backup from before the upgrade and the upgrade-run backup. The old 0.2.5 managed backup does not contain personal-assistant or schedule stores. Preserve consistent snapshots of those stores separately when keeping data created by 0.2.6.

新增个人助理与定期任务、项目控制和投递恢复改进。提供 Native 0.172.0-r70 与 Claude claude-r59；Job Service 0.1.3 复用原始签名文件。已有智能体保留所选运行时。

**从 0.2.5 开始的单向升级：**0.2.6 启动后，不支持退回 0.2.5。请保留升级前完整且一致的离线备份，以及升级程序生成的备份。旧版 0.2.5 的内建备份不包含个人助理和任务日程存储；保留 0.2.6 新增数据时，还须单独保留这两份存储的一致快照。

# Changelog

## 0.2.5 "Beautiful World" (2026-11-23)

- Preserves ordered owner input through interruption and recovery. Uncertain deliveries remain visible, and resend is offered only when the host can prove an input was not sent.
  在中断和恢复过程中保留用户输入的原始顺序；投递结果未确认时保留可见记录，仅在主机能够确认尚未发送时提供重发。
- Admits pending notes at new-turn boundaries with bounded work and explains missing delivery or read evidence without blocking turns.
  在新轮次开始时以有界处理接收待投递便条，并说明缺失的投递或已读证据，保持轮次可用。
- Improves Linux runtime process supervision and recovery while preserving the detected user service-manager environment.
  改进 Linux 运行时进程监管与恢复，并保留已检测到的用户服务管理器环境。
- Before manually returning to 0.2.4, stop all stock Native and Claude scopes with 0.2.5 and verify they are empty or collected.
  手动退回 0.2.4 前，先用 0.2.5 停止全部 stock Native 和 Claude scope，并确认已经清空或回收。
- Includes Job Service 0.1.3 with explicit follow-up disposition. After that disposition is saved, Job Service 0.1.2 cannot read the store; retain a backup and do not downgrade the component.
  提供 Job Service 0.1.3 和明确的后续处理结束记录；保存该记录后，Job Service 0.1.2 无法读取存储，请保留备份并不要降级该组件。
- Includes Native 0.169.0-r65 and Claude claude-r55. Existing agents retain their selected runtime.
  提供 Native 0.169.0-r65 和 Claude claude-r55；已有智能体保留运行时选择。

## 0.2.4 "Beautiful World" (2026-11-14)

- Adds authoritative disposal receipts for queued input and closes ended delegation requests as Stopped when dismissed, preventing stale actions and stuck deliveries.
  新增排队输入处置回执；放弃时将已结束的委托请求标记为已停止，避免失效操作和投递卡住。
- A row still sending after its fixed 60-second deadline becomes delivery-unconfirmed, allowing its delivery state to be checked before retrying.
  消息发送达到固定 60 秒期限仍未确认时，会转为投递未确认，重试前可先核对投递状态。
- Adds bounded filesystem read windows for large file views.
  新增分段文件读取接口，用于大型文件查看。
- Applies configured busy-wake timing to alarms.
  闹钟投递遵循配置的忙碌唤醒时机。
- Includes Native 0.167.0-r61 with connection retry handling and Claude claude-r48 with bounded transient retries, recovery of saved input and UTF-8 direct stdio output. Existing agents retain their selected runtime.
  提供改进连接重试的 Native 0.167.0-r61，以及支持有界临时故障重试、恢复已保存输入和 UTF-8 标准输出的 Claude claude-r48；已有智能体保留运行时选择。

- Known limitation: On a manual return to 0.2.3, dismissals still waiting to apply are not applied there; dismiss those rows again in that version. The records are retained and used again after returning to 0.2.4.
  已知限制：若手动退回 0.2.3，当时尚未执行完的放弃操作不会由旧版执行，请在旧版重新放弃相应行。记录仍会保留，回到 0.2.4 后继续使用。

## 0.2.3 "Beautiful World" (2026-11-13)

- Improves Claude runtime observation, exact process adoption and readiness after restart. A failed supplementary status probe no longer prevents delivery through a healthy runtime connection.
  改进 Claude 运行时观察、精确进程接管及重启后的就绪检查；补充状态探测失败不再阻止通过健康连接投递。
- Reconciles owner-carried result delivery using bound durable evidence, and fences late callbacks to their original reservation.
  使用绑定的持久证据核对随主人消息送达的结果，并将迟到回调限制在原预留内。
- Includes host capabilities for project memory, owner pins, background apps and Baidu Netdisk where supported by the corresponding API. Runtime identities, user permissions and credential handling remain enforced by the host.
  提供相应接口支持的项目记忆、要处理事项、后台应用及百度网盘主机能力；运行时身份、用户权限和凭据处理继续由主机校验。
- Adds the Claude claude-r46 component with adapter 0.8.14, Agent SDK 0.2.180 and bundled CLI 2.1.321. Existing agents retain their pinned runtime; Native 0.167.0-r60 and unchanged components remain available.
  新增 Claude claude-r46 组件，包含适配器 0.8.14、Agent SDK 0.2.180 和 CLI 2.1.321；已有智能体保留运行时固定版本，继续提供 Native 0.167.0-r60 及未变化的组件。

## 0.2.2 "Beautiful World" (2026-11-09)

- Filesystem root identity checks are cached; Git status uses its own worker pool and coalesces concurrent reads for the same repository. Other management requests remain available while Git is slow.
- Includes native 0.167.0-r60 and job-service 0.1.2, which reports observed process state while checking process identity. Claude claude-r40, DSH dsh-r3 and Kimi kimi-r3 retain their existing signed archives. Existing agents keep their selected runtime until explicitly upgraded.
- Owner message retries retain reserved notes when an uncertain prior send cannot be proven delivered.

- 文件系统根身份检查使用缓存；Git 状态使用独立线程池，同一仓库的并发读取合并处理。Git 较慢时仍可处理其他管理请求。
- 提供原生运行时 0.167.0-r60 和 job-service 0.1.2，后者在核对进程身份的同时报告观测到的进程状态；Claude claude-r40、DSH dsh-r3 和 Kimi kimi-r3 保持原有签名包。已有智能体保持所选运行时，直到明确升级。
- 主人重试消息时，若无法确认先前的发送是否投递，随带通知仍保持预留。

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
