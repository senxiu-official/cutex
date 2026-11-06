# Optional Kimi Code adapter — kimi-r3

This source-only layout requires the cutex 0.2.1 `external-runtime-root-v1` interface.
Older bundled installers cannot use this layout. The detailed guide is maintained in this
repository, outside the immutable adapter archive; componentGuideUrl links here.

## What ships

Only our `src/`, `component.json`, `external-runtime.json`, minimal ESM
`package.json`, LICENSE and NOTICE. This README is not included in the archive. No Node, upstream CLI,
node_modules, node-pty or ripgrep is redistributed. Development dependencies,
tests and build tools are excluded by the packager. The source checkout's ws
dev dependency is for tests; the published package metadata has no dependencies.

`external-runtime.json` is the `cutex/external-runtime/v1` detection
contract: an absolute npm prefix, exact upstream/ws package versions, a known
relative JS entry, Node requirements and optional native/tool checks. The daemon
passes the resolved prefix as `KIMI_INSTALL_ROOT`; it runs the real JS entry through an
absolute Node executable rather than an npm shim or a shell command string.

## Prerequisites

- Stable Node 24.x >=24.9.0; recommended/test target24.21.0. Both adapters use
  `node:sqlite`; the older layout's existing Node rules and session pins stay unchanged.
- User-installed Kimi Code 2.3.3 and ws8.22.0 in one versioned npm prefix.
- A loadable node-pty dependency for the configured Node, and ripgrep visible to the daemon (or an explicit rg path).

The install prefix must contain `node_modules/@moonshot-ai/kimi-code/dist/main.mjs` and the package metadata listed by the
contract. Install a new version into a new prefix; configuring it changes only
new agents. Never overwrite or rename the older bundled component directories. Normal
npm installation is a user action; the cutex adapter builder/installer runs no
npm, lifecycle hooks, login or model requests.

## Linux x64 setup

Install Node24.21.0 first. Use a versioned prefix owned by the daemon's account:

```sh
prefix="$HOME/.local/share/cutex-external/kimi/2.3.3"
npm install --prefix "$prefix" @moonshot-ai/kimi-code@2.3.3 ws@8.22.0
# cutex 0.2.1 interface; configure paths on the target daemon host.
cutex component configure kimi-runtime --external-root "$prefix" --node "$(command -v node)" --rg "$(command -v rg)"
```

On Ubuntu/Debian, install prerequisites with
`sudo apt-get install build-essential python3 ripgrep`. Do not use
`--ignore-scripts`: node-pty's normal install tries a prebuild then node-gyp.
It is optional, so npm may succeed even if its native build failed. Verify load:

```sh
node -e 'const p=require(process.argv[1]); if(typeof p.spawn!=="function") throw Error("node-pty unavailable")' "$prefix/node_modules/node-pty"
rg --version
# If the optional native install failed, fix compiler/Python prerequisites first:
npm rebuild --prefix "$prefix" node-pty
```

This load test does not spawn a PTY or run Kimi. A user Kimi cache is not the
same as cutex's per-agent private KIMI_CODE_HOME; ensure daemon-visible rg.

## Windows x64 setup (PowerShell)

Install Node24.21.0 first. Use the account that runs the daemon:

```powershell
$prefix = Join-Path $env:LOCALAPPDATA 'cutex-external/kimi/2.3.3'
npm install --prefix $prefix '@moonshot-ai/kimi-code@2.3.3' 'ws@8.22.0'
# cutex 0.2.1 interface; use the actual Node executable, not a node.cmd shim.
cutex component configure kimi-runtime --external-root $prefix --node (Get-Command node.exe).Source --rg (Get-Command rg.exe).Source
```

Install ripgrep and confirm `Get-Command rg.exe` finds it. Windows normally uses
the node-pty prebuild. If native loading fails, use the tested Node version and
follow node-pty's compiler/Build Tools instructions rather than ignoring the
failure. Check with:

```powershell
node -e 'const p=require(process.argv[1]); if(typeof p.spawn!=="function") throw Error("node-pty unavailable")' "$prefix/node_modules/node-pty"
rg --version
```

Then install the small component through the desktop or
`cutex component install kimi-runtime` and check `cutex component status kimi-runtime`.
Installing/configuring the adapter does not replace Profile credentials or
upstream login. In a remote-host desktop, all paths and commands refer to that
host, not the desktop machine.

## Missing prerequisites and existing sessions

The cutex 0.2.1 daemon contract exposes needsUser details for missing/invalid
prefix, wrong vendor/catalog/ws version and missing/unsupported Node, plus missing rg and a missing/unloadable node-pty binding. Show
expected/actual versions, the target host, paths and actionable install/retry
steps. The desktop must provide configuration and detection actions; a static
refresh does not configure a prefix. An interactive shell's PATH may differ
from the service/GUI daemon's PATH, so explicit paths are preferred.

The existing bundled layout remains valid for its pinned sessions. A new
kimi-r3 selection affects new sessions only. Store resolved paths and versions in
new session bindings; if an external package is upgraded in place, report the
changed dependency on restart rather than silently rebinding it. Removing or
updating an adapter does not delete profiles, histories or pinned versions.

## Deterministic source-only build

From a clean committed checkout with Python3.11+ and Git (no Node/npm needed):

```sh
bash components/kimi-runtime/build.sh linux-x64 kimi-r3 /tmp/adapter-out
```

PowerShell7:

```powershell
./components/kimi-runtime/build.ps1 windows-x64 kimi-r3 <out-dir>
```

Public CI passes `PYTHON` explicitly and uses the same three-argument build
entrypoint. Both targets contain identical platform-neutral source bytes;
platform/commit/build information is outside the tar. Entries are sorted regular
USTAR files, owner0:0, mode0644, fixed mtime1735689600, LF committed bytes;
gzip has no name or timestamp. The archive, SHA256SUMS and BUILD-INFO use the
existing component artifact naming. Existing output names are never replaced.
Public/private kits reuse one canonical released archive per identity/platform;
local milestone builds are review fixtures, not published canonical assets.

The older four-argument offline recipe is not accepted here. Never repack older bundled
bytes under the same version. The source checkout's runtime behavior tests
still use explicit upstream installs and private test homes; no real keys are
needed for the mock-provider tests.

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
