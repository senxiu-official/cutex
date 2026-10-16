<p align="center">
  <img src="assets/logo.png" width="160" alt="Cutex 图标">
</p>

<h1 align="center">cutex</h1>

<p align="center"><a href="README.md">English</a> · <b>简体中文</b></p>

**0.1.1**：可选组件（Job Service、Claude 运行时）由 cutex 安装和更新；托管安装自己运行通知 worker（0.1.0「First Take」是第一个公开版本）。更新内容见 [CHANGELOG.md](CHANGELOG.md)。

> **0.1.x 的已知缺口**（正在修，修好后删除这段）：
> - **NCI 远程作业**在公开版本里还不能用。
> - **手机推送 / 配对手机**：还没有公开的手机应用。
>
> Claude 智能体需要可选组件 `claude-runtime`，长时间作业（Linux）需要 `job-service`：在托管安装上运行 `cutex component install <名字>`，或点桌面端的「安装」按钮（[components/README.md](components/README.md)）。

cutex 是在你自己的电脑上运行编程智能体的后台服务。智能体运行在
[cute-codex](https://github.com/senxiu-official/cute-codex)（OpenAI Codex 的分支）或 Claude Code 上
（通过 [`components/claude-runtime/`](components/claude-runtime/) 里的适配器）。所有状态都在 cutex 里：智能体、项目、队列、委托、
Profile 和用量。

客户端通过 Management v2 HTTP / SSE 接口和它通信。主要的客户端是
[桌面端](https://github.com/senxiu-official/cutex-desktop)，另外还有命令行（`cutex …`）和会话 TUI
（不带参数运行 `cutex`）。

支持 Linux 和 Windows（x64）。

## 功能

**智能体协作**
- 智能体总线：智能体可以列出彼此、互相发消息（`agent_list`、`agent_tell`）。每个智能体用自己的凭据认证。
- 委托：一个智能体把任务交给另一个智能体（`agent_delegate`），可以等结果，也可以先做别的；能查看进度，
  也能回答对方的提问（`question_answer`）。所有委托你都能看到，也可以暂停。
- 过长的消息和结果存成文档，接收方需要时再读。已结束的委托和消息进入可搜索的归档。
- 项目把智能体按工作目录分组。项目可以有一个 Director；Director 和它授权的 operator 可以创建、启动、
  停止和替换成员。不同项目的智能体互相看不到，除非你把项目连起来，或者给某个项目单独授权某个智能体。
- 项目记忆会在启动时加进每个成员的指令里。
- 闹钟：智能体可以给自己或别的智能体定一条稍后送达的消息。你可以查看和取消。
- 通知：智能体可以给你发消息（`owner_notify`），也可以要求你回复。通知级别决定哪些事件、以什么方式通知你；
  通知 worker 把通知转发到 webhook（用 `cutex service install` 安装时它运行在管理服务里；其他情况把 `cutex notify run` 作为单独的服务运行）。
- 审批：超出智能体自身权限的操作（比如使用标记为“需要审批”的 Profile）要等你批准或拒绝。
- 工作流：一个经过批准的脚本启动一次性的步骤智能体，并汇总它们的结果。
- 笔记和侧聊：只属于你自己的笔记（智能体看不到），以及在 Codex 智能体旁边开的临时侧聊。
- 跨主机 peers：两台 cutex 配对后，智能体可以委托或发消息给 `名字@主机`。配对之前不开启。
- 用户和设备：接口默认只监听本机回环地址，也可以开启 tailnet 地址。设备用一次性配对码配对，各自有自己的令牌。
  可以给其他用户开放指定的项目和智能体，他们各自管理自己的设备。

**智能体可用的工具**
- `cutex` MCP 服务给每个智能体提供上面的总线、委托、闹钟和项目工具，按角色筛选。
- 服务：智能体登记自己运行的 Web 服务（`service_register`、`service_list` 等），你通过带认证的代理打开。
- 成果：智能体发布结果，给你和其他智能体看（`artifact_publish`、`artifact_list`、`artifact_read`）。
- 浏览器：智能体使用你在桌面端借给它的内置浏览器标签页（`browser_*`）。
- 网页搜索：通过智能体自己的服务商搜索（`web_search`），是 Codex API key 类 Profile 的开关，默认关闭。

**Profile 和用量**
- 支持 ChatGPT 登录、Claude 账号（订阅或 API key）和 OpenAI 兼容的 API key 作为 Profile，带服务商预设和模型列表。
  同一个模型可以有多个命名配置。
- 每个智能体用一个 Profile，可以换成同一服务商系列的另一个 Profile。
- 服务商提供额度和套餐信息时直接显示。Claude 因用量上限中断的一轮，可以在额度重置后自动继续。
- 按智能体、Profile、模型和日期统计 token 用量，按价格表估算费用，可用美元、澳元或人民币显示。
  在 cutex 之外运行的 Claude Code、Codex 和 ZCode 的用量也可以导入。用量记录只存数量和 ID，不存提示词和回复。

**会话**
- 一个智能体就是一个持久会话、一个原生线程。关掉客户端或重启后台服务后会话仍在；机器重启后，
  cutex 会提示把之前在线的运行时恢复。
- 每一轮都可以排队、引导或停止。给离线智能体发的输入可以直接把它启动。
- 分叉智能体、回退到之前的某一轮，或者只撤销某一轮改动的文件。也可以归档或删除。
- 文件、git 状态和改动对比、终端、图片附件都经过接口，另一台机器上的客户端用法一样。

**主机**
- 能力发现（`GET /v2/capabilities`）和接口的 JSON Schema（`cutex management schema export`）。
- 主机健康检查（`GET /v2/health`）。
- `cutex service install` 把后台服务安装为用户服务（Linux 上是 systemd 用户单元，Windows 上是登录任务）：智能体总线和管理服务，管理服务同时运行通知 worker（webhook、通知状态）。
- 可选组件，按需安装、更新和移除（`cutex component`、`/v2/components`）：`claude-runtime`（Claude 智能体，Linux 和 Windows）和 `job-service`（智能体的长时间作业，Linux）。见 [components/README.md](components/README.md)。
- 用不同的目录运行相互独立的实例（`CUTEX_HOME`）。

## 安装

可以用发布的二进制安装，也可以从源码构建。完整的安装指南两种方式都涵盖，包括 Linux 和 Windows 上的 cutex、
cute-codex、可选组件和桌面端，人照着做或者交给编程助手执行都可以：

**[AGENT-INSTALL.md](https://github.com/senxiu-official/cutex-desktop/blob/release/docs/install/AGENT-INSTALL.md)**
（在 cutex-desktop 仓库里，英文）

**发布的二进制。** 每个 [GitHub Release](https://github.com/senxiu-official/cutex/releases) 都附有
GitHub Actions 从对应标签的提交构建的 `cutex` + `cutex-mcp`（Linux x64 和 Windows x64），以及签名的 `manifest.json`。
每个文件都用 minisign 签名（`<文件>.minisig`），用发布公钥校验；cutex 后端更新信任的也是这把公钥（`src/upgrade/keys.rs`）：

```sh
minisign -Vm <文件> -P RWRH9INhAb2gACJAg/h5EBjbudos2LXj6K+5hRBEGeVLDKaKzE+FICmS
```

指南的 B 节把它们装成托管安装，之后通过 `cutex upgrade` 或桌面端的后端更新升级。Windows 和 macOS 的桌面应用没有用付费
证书做代码签名，第一次启动时 Windows SmartScreen 和 macOS Gatekeeper 会提示（指南写了该点哪里）；这里的命令行程序不受影响。

**从源码**单独构建 cutex（Rust 1.95）：

```sh
git clone --branch release https://github.com/senxiu-official/cutex.git
cd cutex
cargo build --locked --release --bin cutex --bin cutex-mcp
./target/release/cutex --version
```

`cutex` 是后台服务、命令行和 TUI，`cutex-mcp` 是智能体使用的 MCP 服务。之后的步骤（cutex 目录、
`cutex service install`，以及智能体运行所需的 cute-codex 或 Claude 运行时）见安装指南。

## 运行模型

| 组件 | 作用 |
|---|---|
| cute-codex | Codex 智能体：原生 app-server、模型调用和工具 |
| `components/claude-runtime/` | Claude 智能体：基于 Claude Agent SDK 和 Claude Code 的 app-server 适配器 |
| Profile | 一个服务商账号的凭据和设置，各 Profile 相互隔离 |
| 会话存储 | 智能体的持久身份、原生线程 ID、工作目录、项目、Profile 和生命周期 |
| 托管运行时 | 一个智能体的常驻原生进程；没有客户端连接时也继续运行 |
| 管理服务 | HTTP / SSE 接口、运行时恢复、事件和主人操作 |
| 智能体总线 | 智能体之间的发现和带认证的消息投递 |

`online`、`offline` 和 `close` 作用于托管运行时；关掉终端或桌面端不会停止它。归档会话会保留原生历史。

## 文档

- [docs/management/human-cli.md](docs/management/human-cli.md)：`cutex human`，主人用的命令行
- [docs/management/human-configuration.md](docs/management/human-configuration.md)：单个智能体的配置
- [docs/management/user-input-delivery.md](docs/management/user-input-delivery.md)：用户输入如何排队和投递
- [docs/PEERS.md](docs/PEERS.md)：后台服务配对和跨主机委托
- [docs/SERVICES.md](docs/SERVICES.md)：智能体服务和代理
- [docs/NOTES.md](docs/NOTES.md)、[docs/SIDE-CHATS.md](docs/SIDE-CHATS.md)：笔记和侧聊
- [docs/NOTIFICATION-LEVELS.md](docs/NOTIFICATION-LEVELS.md)：通知级别
- [docs/agent-tool-security.md](docs/agent-tool-security.md)：智能体凭据、项目边界、角色和审批
- [docs/agent-management-provider.md](docs/agent-management-provider.md)：项目、Director 和 operator
- [docs/settings-defaults.md](docs/settings-defaults.md)：新会话的默认设置
- [docs/usage-pricing.md](docs/usage-pricing.md)：用量和费用估算
- [docs/profile-model-configurations.md](docs/profile-model-configurations.md)：模型的命名配置
- [docs/FILES-REVERT.md](docs/FILES-REVERT.md)：撤销某一轮改动的文件
- [docs/health-check.md](docs/health-check.md)：主机健康检查
- [docs/backend-upgrade.md](docs/backend-upgrade.md)：托管安装、`cutex service install` 和签名的后端更新（`cutex upgrade`）
- [components/README.md](components/README.md)：可选组件（`cutex component`）
- [docs/INSTANCES.md](docs/INSTANCES.md)：相互独立的实例（`CUTEX_HOME`）
- [docs/agent-bus-operations.md](docs/agent-bus-operations.md)：智能体总线的令牌、审计日志和卡住的消息
- [docs/PROFILE-SEARCH-TOOL.md](docs/PROFILE-SEARCH-TOOL.md)：`web_search` 工具
- [docs/SESSION-FAST-MODE.md](docs/SESSION-FAST-MODE.md)：按会话的快速模式

以上文档均为英文。

## 现状

这是第一版。作者每天都在用，但其他用户还不多。

- **远程访问默认在你自己的网络里。** 接口默认只监听本机回环地址。访问其他机器请通过 SSH 或你信任的私有 tailnet，
  不要把管理接口或智能体总线的端口暴露到公网。
- **用户身份是为了方便。** 用户身份是为了和熟人共用主机，不是防陌生人的安全边界。
- **升级。** 托管安装（`cutex service install`）用 `cutex upgrade` 或桌面端的后端更新应用签名的发布版本
  （见 [docs/backend-upgrade.md](docs/backend-upgrade.md)）。
  从源码构建的安装也可以继续从源码升级。
- 在 Linux 上，智能体可以用 Job Service 运行长时间的命令，它是一个可选组件（[components/job-service/](components/job-service/)）。

### 本地数据

cutex 的状态都在 `~/.cutex`（Windows：`%USERPROFILE%\.cutex`）里：`config.json`（设置和令牌）、
`accounts.json` 和 `profiles/`（Profile 及其凭据）、`cutex-sessions.sqlite3`（会话存储）、`codex-home/` 和
`runtime/`（运行时状态、日志和各类存储）。这些文件包含凭据、私人路径和你的工作记录，不要公开这个目录，
也不要把它复制到发布包里。

API 密钥和令牌只在后台服务里。每个智能体拿到的是自己的总线凭据，而不是根令牌；Codex 智能体启动的 shell 工具
看不到服务商密钥。

## 从源码构建（开发）

```sh
cargo fmt --check
cargo clippy --all-targets --locked
cargo test --locked -- --test-threads=1
cargo build --release --locked
```

测试不能碰你真实的 `~/.cutex`。调试版本和测试进程会拒绝使用它（`.cargo/config.toml` 设置了
`CUTEX_TEST_FORBID_REAL_HOME=1`）。要让调试版后台服务或测试使用单独的目录，先建一个目录，放一个空的标记文件
`.cutex-test-private-home`，再把两个变量都指向它：

```sh
mkdir -p ~/cutex-test-home && touch ~/cutex-test-home/.cutex-test-private-home
CUTEX_TEST_PRIVATE_HOME=~/cutex-test-home HOME=~/cutex-test-home cargo test --locked -- --test-threads=1
```

这样 cutex 会使用 `~/cutex-test-home/.cutex`。设置了 `CUTEX_TEST_PRIVATE_HOME` 时，发布版本会拒绝运行。调试版的
`cutex management serve` 请用单独的端口（`--port`），避免和已安装的后台服务冲突。

Claude 适配器是一个 Python 包，测试在 `components/claude-runtime/` 里。

欢迎在 GitHub issue 里报 bug。觉得有用的话，欢迎在 GitHub 上点个 Star，方便更多人找到它。

## 致谢

由 Codex、Claude、DeepSeek、GLM 和 Kimi 参与构建。产品名称是各自所有者的商标，提及不代表有任何关联或背书。

Codex 智能体运行在 cute-codex 上，它是 [OpenAI Codex](https://github.com/openai/codex)（Apache-2.0）的分支。
cutex 原样使用了 Codex 0.154.0 的默认模型指令，见 [NOTICE](NOTICE)。

## 许可证

Apache License 2.0：见 [LICENSE](LICENSE) 和 [NOTICE](NOTICE)。`components/claude-runtime/` 使用 MIT 许可证
（[components/claude-runtime/LICENSE](components/claude-runtime/LICENSE)）。
