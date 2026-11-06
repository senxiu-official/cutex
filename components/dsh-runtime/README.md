# Optional DeepSeek Harness adapter — dsh-r3

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
passes the resolved prefix as `DSH_INSTALL_ROOT`; it runs the real JS entry through an
absolute Node executable rather than an npm shim or a shell command string.

## Prerequisites

- Stable Node 24.x >=24.9.0; recommended/test target24.21.0. Both adapters use
  `node:sqlite`; the older layout's existing Node rules and session pins stay unchanged.
- User-installed DeepSeek Harness 0.2.1-rc.3 and ws8.22.0 in one versioned npm prefix.
- The matching `@deepseek-ai/dsh-session-format-catalog@0.2.1-rc.3` in that same prefix.

The install prefix must contain `node_modules/@deepseek-ai/dsh/lib/bin.js` and the package metadata listed by the
contract. Install a new version into a new prefix; configuring it changes only
new agents. Never overwrite or rename the older bundled component directories. Normal
npm installation is a user action; the cutex adapter builder/installer runs no
npm, lifecycle hooks, login or model requests.

## Linux x64 setup

Install Node24.21.0 first. Use a versioned prefix owned by the daemon's account:

```sh
prefix="$HOME/.local/share/cutex-external/dsh/0.2.1-rc.3"
npm install --prefix "$prefix" @deepseek-ai/dsh@0.2.1-rc.3 @deepseek-ai/dsh-session-format-catalog@0.2.1-rc.3 ws@8.22.0
# cutex 0.2.1 interface; configure paths on the target daemon host.
cutex component configure dsh-runtime --external-root "$prefix" --node "$(command -v node)"
```

## Windows x64 setup (PowerShell)

Install Node24.21.0 first. Use the account that runs the daemon:

```powershell
$prefix = Join-Path $env:LOCALAPPDATA 'cutex-external/dsh/0.2.1-rc.3'
npm install --prefix $prefix '@deepseek-ai/dsh@0.2.1-rc.3' '@deepseek-ai/dsh-session-format-catalog@0.2.1-rc.3' 'ws@8.22.0'
# cutex 0.2.1 interface; use the actual Node executable, not a node.cmd shim.
cutex component configure dsh-runtime --external-root $prefix --node (Get-Command node.exe).Source
```

Then install the small component through the desktop or
`cutex component install dsh-runtime` and check `cutex component status dsh-runtime`.
Installing/configuring the adapter does not replace Profile credentials or
upstream login. In a remote-host desktop, all paths and commands refer to that
host, not the desktop machine.

## Missing prerequisites and existing sessions

The cutex 0.2.1 daemon contract exposes needsUser details for missing/invalid
prefix, wrong vendor/catalog/ws version and missing/unsupported Node. Show
expected/actual versions, the target host, paths and actionable install/retry
steps. The desktop must provide configuration and detection actions; a static
refresh does not configure a prefix. An interactive shell's PATH may differ
from the service/GUI daemon's PATH, so explicit paths are preferred.

The existing bundled layout remains valid for its pinned sessions. A new
dsh-r3 selection affects new sessions only. Store resolved paths and versions in
new session bindings; if an external package is upgraded in place, report the
changed dependency on restart rather than silently rebinding it. Removing or
updating an adapter does not delete profiles, histories or pinned versions.

## Deterministic source-only build

From a clean committed checkout with Python3.11+ and Git (no Node/npm needed):

```sh
bash components/dsh-runtime/build.sh linux-x64 dsh-r3 /tmp/adapter-out
```

PowerShell7:

```powershell
./components/dsh-runtime/build.ps1 windows-x64 dsh-r3 <out-dir>
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

The adapter projects authoritative native logs into separate SQLite storage.
Recovery/replay never resends a prompt. Context occupancy is separate from
unavailable billing. Steer, native fork/revert/delete, images and unsupported
Codex features stay disabled; approvals use Management's one-shot broker.
