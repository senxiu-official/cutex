# Optional Kimi Code runtime

`kimi-runtime` runs Kimi Code (`@moonshot-ai/kimi-code`, MIT) as a cutex agent
runtime through its ACP server (`kimi acp`). The component pins Kimi Code
`2.3.3` and ws `8.22.0`. Platforms are Linux x64 and Windows x64 (Windows paths
are written but untested). Stable Node >=22.19.0 is required (Kimi Code's own
`engines`). A host supplies Node, or an offline payload includes `runtime/node`
(`runtime/node.exe` on Windows).

## Install

```sh
cutex component list
cutex component install kimi-runtime
cutex component status kimi-runtime
```

A source-only install uses `cutex component install kimi-runtime --from-source
<absolute-path>`. The input is this directory with an explicitly staged
`runtime/`: `package{,-lock}.json` and `node_modules/` from an offline
`npm install @moonshot-ai/kimi-code@2.3.3 ws@8.22.0` for the target platform.
Kimi's optional native addon (`node-pty`) has no Linux prebuild. Kimi works
without it. Neither installation nor packaging runs npm or package scripts.

If Node is not on the daemon PATH, set `CUTEX_KIMI_NODE` to an absolute
executable and install again. When the component is installed, the host
advertises the `kimi-runtime` capability (`features.kimiRuntime`), and Kimi
Profiles report `supported` instead of `componentNotInstalled`.

## Packaging

```sh
build.sh linux-x64 kimi-r2 <out-dir> <source-tree>
pwsh build.ps1 -Platform windows-x64 -Version kimi-r2 -OutDir <out> -SourceTree <src>
```

Archives contain no npm `.bin` shims or links.

## What each agent gets

Each agent has its own private `KIMI_CODE_HOME`, `HOME`, XDG directories and
temp directory under its cutex state directory. cutex writes that home's
`config.toml` again at every start. It holds:

- the provider: a stored or env API key named by `api_key_env`, or a Kimi
  account's managed provider;
- the models;
- `[[permission.rules]]` (parsed but not applied by Kimi 2.3.3);
- for a read-only agent, a `[[hooks]]` `PreToolUse` entry that refuses every
  tool outside the read set (no writes, no shell commands);
- `[tools] disabled = [CronCreate, CronList, CronDelete, AskUserQuestion]`;
- `[watch] enabled = false`;
- `telemetry = false`.

The launch also sets `KIMI_CODE_WATCH=0`, `KIMI_DISABLE_TELEMETRY=1`,
`KIMI_DISABLE_CRON=1`, `KIMI_CODE_NO_AUTO_UPDATE=1` and
`KIMI_CODE_REGION_MARKER=off`.

cutex's instructions go into the agent's private `KIMI_CODE_HOME/AGENTS.md`.
This covers `# Cutex`, the user AGENTS.md, file references and project memory.
Project `AGENTS.md` and `.kimi-code/AGENTS.md` files are Kimi's own, so cutex
does not inject them a second time.

Kimi has no OS sandbox. cutex maps the agent's settings onto Kimi's modes:

| cutex | Kimi |
|---|---|
| workspace-write (default) | mode `default`: workspace reads/edits run; commands and anything else ask the owner |
| read-only | mode `default` + deny `Write`/`Edit` |
| full access + approval never | mode `auto` (never asks) |
| full access + another approval policy | mode `yolo` (asks only for sensitive cases) |
| approval `never` with another sandbox | asks are refused at once (no prompt) |

The cutex MCP tools (`mcp__cutex__*`) are always pre-approved.

Kimi treats the nearest ancestor with `.git` as the project root. It reads
instructions, skills and MCP files on every level down to the cwd. If that
root is above the agent's folder and its `.git` is not a repository (for
example an empty `.git` in a home directory), cutex refuses to start the
agent. Make the agent's folder a git repository, or remove the stray `.git`.

## Tests

`node --test test/*.test.mjs` drives the real pinned Kimi Code against the
scripted offline mock (`test/mock-llm.mjs`). Set `KIMI_INSTALL_ROOT` to an npm
prefix holding `@moonshot-ai/kimi-code@2.3.3`. The tests use no key or network,
only private homes.

## Public 0.2.0 compatibility

Kimi is experimental and its binary component is not included in public 0.2.0.
Source metadata and the runtime catalog remain available. Installation reports that the
component is not in this release; existing installed versions remain selected.
The existing kimi-r2 Linux package has no node-pty binding, so interactive terminal tools
are unavailable; offline Glob/Grep require system ripgrep (`rg`). A reproducible public
CI recipe and Node/PTY fix will use the future kimi-r3 identity in a later update.

Kimi 为实验功能，公开 0.2.0 不提供其二进制组件。源码元数据和运行时目录仍保留；
安装时会提示本次发行不包含该组件，现有已安装版本仍保持选择。
kimi-r2 的 Linux 包缺少 node-pty 扩展，交互式终端工具不可用；离线 Glob/Grep 需系统 rg。
可复现的公开 CI 打包与 Node/PTY 修复将在后续更新中使用 kimi-r3 版本号。
