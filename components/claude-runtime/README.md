# claude-runtime: the Claude runtime of cutex

`claude-runtime` is the optional cutex component that runs **Claude agents**. It is a small Python
app-server: cutex starts one adapter process per Claude agent and talks to it with the same
app-server protocol subset it uses for Codex agents. The adapter in turn drives the official
**Claude Code** CLI through the official **Claude Agent SDK** (`claude-agent-sdk` on PyPI), with your
own Claude login or an Anthropic-compatible API key.

It is not an Anthropic product and does not replace or modify Claude Code. You need it only if you
want Claude Profiles / Claude agents; Codex-based agents work without it.

## What you need

| | |
|---|---|
| Python | 3.11 or newer (`python3` on Linux, the `py` launcher or `python` on Windows). On Debian / Ubuntu also the `python3-venv` package. |
| Network | none for a release install: the archive carries its Python dependencies (wheels for CPython 3.11 to 3.14) |
| Claude Code | the official `claude` CLI, **installed by you** (see below). cutex never ships or downloads it. |
| A Codex runtime | selected with `cutex human install-runtime` (install guide). Its `codex` binary is the optional foreground TUI. |
| Disk | about 350 MB per installed version (mostly the Python dependencies) |

### Install Claude Code yourself

Claude Code is proprietary software of Anthropic (its license: all rights reserved, use subject to
Anthropic's terms), so cutex does not bundle or redistribute it. Install it from Anthropic:

```sh
# Linux (native installer, puts `claude` in ~/.local/bin)
curl -fsSL https://claude.ai/install.sh | bash
```

```powershell
# Windows (native installer, puts claude.exe in %USERPROFILE%\.local\bin)
irm https://claude.ai/install.ps1 | iex
```

Other methods are documented by Anthropic (<https://docs.anthropic.com/en/docs/claude-code/setup>).
Check with `claude --version`. cutex looks for `claude` on PATH and in the installers' default
places (`~/.local/bin`, `~/.claude/local`, `/usr/local/bin`, `%USERPROFILE%\.local\bin`,
`%APPDATA%\npm`); set `CUTEX_CLAUDE_CLI=<path>` for the cutex services when it lives elsewhere.

The `claude-agent-sdk` wheels on PyPI also contain a copy of the CLI. The runtime archive does not
carry them: it ships a CLI-free wheel built from the SDK's MIT-licensed source, and agents always run
the `claude` you installed, so you decide its version and update it the way Anthropic documents.

## Install

From a release install of cutex (managed install, section B of the install guide):

```sh
cutex component install claude-runtime
cutex component status claude-runtime
```

The desktop offers the same as **Install** where a Claude Profile or agent needs the runtime.
cutex takes `component-claude-runtime-<rN>-<platform>.tar.gz` named by the signed manifest of the
cutex release it runs, checks its sha256, unpacks it to
`<install base>/components/claude-runtime/<rN>/` (never changed afterwards; `receipt.json` lists
every file's sha256), and then:

1. checks every unpacked file against `receipt.json`;
2. finds Python 3.11 or newer (`CUTEX_CLAUDE_PYTHON=<python>` overrides the search);
3. creates `prepared/venv/` there and installs `requirements.lock` from the bundled `wheels/`
   (`pip install --no-index --require-hashes`; output in `prepared/install.log`);
4. imports the adapter and the SDK once with the new venv;
5. finds your `claude` CLI and the TUI binary, picks the Claude login home, writes
   `prepared/deployment.json` (below) and selects it for **new** Claude agents.

If step 5 cannot find `claude`, the component is installed but shows **needs you** with the steps;
install Claude Code, then run the install again (it reuses the venv and only redoes step 5). The same
state appears when no Claude account is logged in yet (next section).

From a source checkout (install guide, path A), which vendors the wheels from PyPI first:

```sh
cutex component install claude-runtime --from-source ~/src/cutex
```

Install base: Linux `~/.local/share/cutex`, Windows `%LOCALAPPDATA%\cutex`.

Versions are kept side by side. Existing Claude agents keep the version they were created with
(as Codex agents keep their natives); a backend update that brings a new `claude-rN` installs it next
to the old one and selects it for new agents only. `cutex component remove claude-runtime` only stops
new agents from using it; `cutex component prune claude-runtime` deletes versions no agent uses.

## Log in

Each Claude agent uses a Claude account: a subscription login or an API key. The default account's
login home is `~/.cutex/claude-home` (Windows `%USERPROFILE%\.cutex\claude-home`); it is kept when
the adapter is updated.

```sh
cutex human claude account list
cutex human claude account login default     # runs the official `claude auth login` for that home
```

Or add an API account (key read from stdin, never as an argument):

```sh
cutex human claude account add work --api --api-key-stdin [--base-url https://…/anthropic]
```

Then create a Claude Profile in the desktop (Settings → Profiles → New profile, kind Claude) and a
Claude agent with it. An agent whose account is not logged in still starts; its first message fails
with an authentication error.

The adapter never reads, copies or refreshes your Claude credentials itself; Claude Code does that in
its own config directory. Your use of Claude through Claude Code stays subject to Anthropic's terms
for your plan.

## `deployment.json`

The selected runtime is `~/.cutex/runtime/claude/deployment.json`. The component install writes it
for you; to point cutex at a runtime you assembled yourself, write the same five absolute paths into
a file and select it with `cutex human claude install <file>`:

```json
{
  "python": "/home/me/.local/share/cutex/components/claude-runtime/claude-r14/venv/bin/python",
  "adapter_source": "/home/me/.local/share/cutex/components/claude-runtime/claude-r14",
  "claude_cli": "/home/me/.local/bin/claude",
  "claude_home": "/home/me/.cutex/claude-home",
  "tui_binary": "/home/me/.local/share/cutex/natives/0.160.0-r52.2/codex"
}
```

| Key | Must be |
|---|---|
| `python` | an existing Python whose environment has the runtime dependencies installed (Windows: `…\venv\Scripts\python.exe`) |
| `adapter_source` | a directory with `src/cute_claude/__main__.py` and `scripts/connect_tui.py` (this directory, or an unpacked release archive) |
| `claude_cli` | the official Claude Code executable |
| `claude_home` | an existing directory: Claude Code's config dir (`CLAUDE_CONFIG_DIR`) of the default account; inside the cutex home for an independent instance |
| `tui_binary` | the cute-codex `codex` binary. Only `cutex human claude attach` (foreground TUI) runs it, never headless agents or the desktop, but the file must exist |

No other keys are accepted. Every Claude agent keeps a copy of the deployment it was created with.

## Foreground TUI

`cutex human claude attach <agent>` opens the cute-codex TUI on a running Claude agent through
`scripts/connect_tui.py` (authenticated loopback WebSocket; no unauthenticated fallback). The desktop
does not need it.

## Development

```sh
cd components/claude-runtime
python3 -m venv .venv && . .venv/bin/activate
python -m pip install -r requirements-tested.txt
python -m pytest -q
```

The tests use a fake SDK and fixture processes; they never call a model or read a login. The offline
mock backend needs no Claude at all:

```sh
python -m cute_claude --mock --workspace "$PWD/demo-workspace" --state-dir "$PWD/.state" --listen stdio://
```

Release archive: `bash components/claude-runtime/build.sh <linux-x64|windows-x64> <rN> <out-dir>`
writes `component-claude-runtime-<rN>-<platform>.tar.gz` (the shipped source, `wheels/` and
`requirements.lock`, made by `scripts/vendor_wheels.py`), `SHA256SUMS-component-claude-runtime-<platform>`
and `BUILD-INFO-component-claude-runtime-<platform>.json`. The component version is `version` in
`component.json` and must equal `release` in `src/cute_claude/features.json` (the build fails
otherwise). A version is published once and never changes: bump both whenever a shipped file or a
pin in `requirements-runtime.txt` changes. A new `claude-agent-sdk` pin also needs its sdist's sha256
in `scripts/vendor_wheels.py`.

Protocol notes: [docs/BUSY_DELIVERY.md](docs/BUSY_DELIVERY.md) (busy wakes and delegation results).

### Protocol: safety checks under full access (feature `safetyGrants`)

Under full access (`bypassPermissions`) Claude Code still calls the permission callback for safety
checks no rule can skip (an `rm` on a possibly empty variable, an `rm` inside an unparsable `sh -c`).
The adapter never sends these to a human and never waits:

1. It declines the call with Claude Code's reason and a hint (rewrite with a literal path or
   `${VAR:?}`, no `rm` inside `sh -c`; if the exact command is truly necessary, do not retry or work
   around it: stop, explain it to the owner and end the turn). The tool row ends `declined`.
2. It records a *safety block* and emits `thread/safetyCheck/blocked {threadId, turnId, itemId, block}`.
3. A grant (`thread/safetyCheck/grant`) allows the next call with the same tool and significant input on
   that thread within 30 minutes, once (`Bash`: the `command`; other tools: the input minus
   `description`); the block then turns `used` (`thread/safetyCheck/updated`). Anything else is declined
   as in step 1.

| method | params | result |
|---|---|---|
| `thread/safetyCheck/list` | `{threadId, state?}` | `{data: [block]}` |
| `thread/safetyCheck/grant` | `{threadId, blockId, decidedBy: {kind: "owner", id?}}` | `{block}` |
| `thread/safetyCheck/dismiss` | `{threadId, blockId, decidedBy: {kind: "owner", id?}}` | `{block}` |

`block` = `{blockId, threadId, turnId, itemId, toolName, toolInput, command, reason, source, state
(blocked | granted | used | dismissed | expired), blockedAt, decidedAt, decidedBy, expiresAt, usedAt,
grantSeconds}`. Only a connection that declared `claudeInteractiveApprovals` may grant or dismiss, and
only with `decidedBy.kind == "owner"` (else `data.code` `owner_required`); a decided block answers
`safety_block_closed`, an unknown one `safety_block_not_found`. Blocks and grants are stored in the
state database (`safety_blocks`), so a grant survives an adapter restart and is still used only once.
cutex exposes the decision to the owner only (capability `claude-safety-grants`, cutex
`docs/SAFETY-GRANTS.md`).

## License

MIT, see [LICENSE](LICENSE). The vendored wheels are third-party packages under their own licenses
(each wheel carries its license in its `.dist-info`); the `claude-agent-sdk` wheel is built from
Anthropic's MIT-licensed SDK source without the bundled CLI, and Claude Code is not part of this
package.
