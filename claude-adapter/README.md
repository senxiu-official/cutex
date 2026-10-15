# cute-claude app-server — 0.6.1 / claude-r9

本版在已整合的 r8 上补充带版本号的模型目录、CLI 版本兼容信息，以及父代理实际响应模型记录。
家族别名保留并标注 alias；目录不代表账号已获得对应模型权限。
实现范围、接口与验证边界见 [r9 发布说明](docs/R9.md)。
r8 的 fork/revert、ExternalInput 等能力及升级要求见 [r8 发布说明](docs/R8.md)。

可修改的 app-server 协议子集 + 官方 Claude Agent SDK 持久运行时适配。用于用户本人在本机通过官方登录运行独立 Claude agents，并尝试复用 cute-codex TUI。不是 Anthropic 官方产品，也不是完整 Codex app-server / Claude Code UI 克隆。

持久运行时基于 `CLAUDE_PERSISTENT_RUNTIME_RESEARCH_20260917.md`：每个在线 agent 使用常驻 SDK owner。

- [研究与逐项回应](docs/PERSISTENT_RUNTIME_REVIEW.md)
- [本地 agent 交接与目标机命令](docs/HANDOFF.md)
- [实际验证及未验证项](docs/VALIDATION.md)
- [协议](docs/PROTOCOL.md)
- [cutex-desktop 接口对齐矩阵（Codex v2 形状、能力开关、未支持项）](docs/DESKTOP_PARITY.md)

## 架构

```text
cute-codex TUI / Cutex controller
            │ JSONL / authenticated WebSocket
       open app-server adapter
            ├── durable FIFO inbox / UI projection / host tools
            └── per-thread owner task
                    ├── one persistent ClaudeSDKClient
                    ├── one continuous receive_messages reader
                    └── selected official Claude Code child
                              │ one official login directory
```

Connect / Query / Interrupt / Disconnect 由同一个 owner 执行；reader 是唯一应用层 SDK 消息消费者。结果只结束当前 turn。消息处理仍是 FIFO 独立 turn，不注入运行中的请求。无保活模型请求。

默认选择系统正常安装的 `claude`，可用 `--claude-cli` 明确路径。**不自动选择 SDK wheel 中的 bundled CLI**；wheel 可能仍携带未使用的副本。本版依赖官方闭源运行时，绝不是订阅版 Messages API/token 代理。

## 安装与登录

```sh
python3 -m venv .venv
source .venv/bin/activate
python -m pip install -e '.[subscription,test]'
CLAUDE_BIN="$(command -v claude)"
CLAUDE_HOME="$HOME/.cutex-claude-compatible/.claude"
python -m cute_claude.subscription_auth status --claude-cli "$CLAUDE_BIN" --claude-config-dir "$CLAUDE_HOME"
# 尚未登录该目录时，由用户本人完成官方流程：
python -m cute_claude.subscription_auth login --claude-cli "$CLAUDE_BIN" --claude-config-dir "$CLAUDE_HOME"
```

status 与 login 都直接调用原版 CLI。adapter 不读取、复制或自行刷新 OAuth 凭据；检查 API/token 路由冲突，失败不回退 API。一个人的多个 agent 共享登录，不共享 native session UUID。组织席位、计费、配额并未由本包认证。

运行时启动 / turn 前**不再**跑 `auth status`（0.6.2 起）：它是同一 config dir 上的第二个官方 CLI 进程，access token 过期时会和 SDK 进程争抢 Claude Code 的 OAuth 刷新锁，导致首个 turn 偶发失败。token 由 SDK 进程自己刷新；未登录 / 登录失效时首个请求以认证错误失败。

## 运行

```sh
mkdir -p demo-workspace
python -m cute_claude --backend subscription --model sonnet   --claude-cli "$CLAUDE_BIN" --claude-config-dir "$CLAUDE_HOME"   --workspace "$PWD/demo-workspace"   --state-dir "$HOME/.local/state/cute-claude-persistent-demo"   --listen ws://127.0.0.1:4501
```

新协议需 initialize/initialized。state-dir 中的 rpc-token 用于本地 WS；不要公开 token。前端 helper 不会启动另一份 Claude，它只连接上面同一个服务：

```sh
python scripts/connect_tui.py --binary cute-codex --model sonnet   --workspace "$PWD/demo-workspace" --endpoint ws://127.0.0.1:4501   --token-file "$HOME/.local/state/cute-claude-persistent-demo/rpc-token"
```

**目标 cute-codex 尚未端到端验证**，helper 会检查 remote 参数，不做无认证降级。默认只读 host tools；`--allow-write` / `--allow-exec` 显式开放（同时开放 Claude Code 原生 Edit/Write 与 Bash，分别投影为 Codex `fileChange` / `commandExecution` 项），exec 具有当前系统用户权限且无 OS sandbox。r8 开放继承审批与 Plan 约束的前台 Agent/Task；后台自主子代理与自唤醒工具仍关闭。业务函数通过 SDK 进程内 MCP 自动桥接，不需独立 MCP 服务。Agent 停止时 cutex 用 `python -m cute_claude.history_reader --state-dir …`（只读、不取 writer lease）读取历史。

新控制面：`claude/runtime/start` / `status` / `close`，参数 `{ "threadId": "..." }`。Start 仅连进程；thread/start 建立本地身份、thread/resume 仅重读 UI；turn/start 可按需首次连接。显式 Close 后，先显式 Start。Detach 不停后台；**stdio EOF 会关闭 adapter，持久 detach 场景应使用 WS 托管进程**。

## 测试

```sh
python -m pytest -q
python scripts/demo.py --record docs/demo-trace.jsonl
python scripts/demo_job.py
# 真实目标机；会消耗订阅用量：
python scripts/smoke_subscription.py --model sonnet   --claude-cli "$CLAUDE_BIN" --claude-config-dir "$CLAUDE_HOME"
```

离线模式仍保留（不装 subscription extra 也可）：

```sh
python -m pip install -e '.[test]'
python -m cute_claude --mock --workspace "$PWD/demo-workspace" --state-dir /tmp/cute-claude-mock --listen stdio://
```

Messages API backend 也是显式可选项；它不使用订阅，不能与 subscription 状态目录混用。

SDK 公共 API 目标锁定 0.2.166（捆绑 CLI 2.1.299；adapter 0.7.1）。API 账户（`--api-account-config`）的 key 不再放进官方运行时的环境变量：adapter 通过 flag settings 的 `apiKeyHelper` 让 CLI 运行 `python -I api_key_helper.py <keyFile>` 读取 key 文件，Bash 工具、hooks、MCP stdio 子进程与 shell snapshot 都看不到 key（CLI 把 helper 输出同时作为 `x-api-key` 与 `Authorization: Bearer` 发送，两种 keyEnv 都适用）。可选原生验证：`CUTEX_TEST_CLAUDE_CLI=<CLI> pytest tests/test_api_key_tool_env.py`（假 key + loopback 假端点）。r9 测试使用 fake SDK、实际 AnyIO scope、Python fixture 子进程，以及真实 SDK 的离线 session 文件操作和无 prompt 控制查询；**没有调用真实模型或完成 TUI/桌面整链验收**。见 [r9 验证记录](docs/R9_VALIDATION.md)；旧版本文档不代表当前能力。
