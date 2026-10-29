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
| Python | none for a release install: the archive carries CPython 3.13.17. `CUTEX_CLAUDE_PYTHON` remains an explicit override; a source build needs a bootstrap Python 3.11+. |
| Network | none for a release install: the archive carries Python and its hash-locked dependencies |
| Claude Code | claude-r30 release archives bundle the SDK-matched CLI; set `CUTEX_CLAUDE_BUNDLE_CLI=0` only for a CLI-free release build |
| A Codex runtime | selected with `cutex human install-runtime` (install guide). Its `codex` binary is the optional foreground TUI. |
| Disk | about 350 MB per installed version (mostly the Python dependencies) |

### Bundled or system Claude Code

The SDK wheel's MIT metadata does not declare the separately licensed Claude Code binary as MIT.
Anthropic's legal/compliance documentation permits preinstalling the unmodified binary in products
subject to its Commercial Terms and conditions (including keeping built-in authentication available
and having users use their own credentials and pay Anthropic directly). The release owner confirmed
those conditions for claude-r28 / r259 on 2026-10-25; claude-r29 / r260 and claude-r30 / r263 keep the same unmodified,
SDK-matched packaging policy. Consequently the release build defaults
`CUTEX_CLAUDE_BUNDLE_CLI` to `1`: it carries Anthropic's unmodified platform wheel, marks the bundled
executable and needs no second download from claude.ai. Set the flag explicitly to `0` to make a
CLI-free archive; that build requires a system CLI.

Set the global Management setting `claudeCli` to `system` if you prefer a separately installed CLI:

```json
{"claudeCli":"system"}
```

Install that CLI from Anthropic, for example:

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
`%APPDATA%\npm`). `CUTEX_CLAUDE_CLI=<path>` for the cutex services always wins over the setting.
Changing `claudeCli` reselects the deployment for new agents; existing agents keep their pinned
deployment. Component status reports the selected source and both detected versions.

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
2. uses `CUTEX_CLAUDE_PYTHON=<python>` when set, otherwise safely materialises the bundled
   `python-runtime.tar.gz` into `prepared/python/`; archives older
   than claude-r28 keep their host-Python fallback;
3. creates `prepared/venv/` with copies from that interpreter and installs `requirements.lock` from the bundled `wheels/`
   (`pip install --no-index --require-hashes`; output in `prepared/install.log`);
4. imports the adapter and the SDK once with the new venv;
5. probes the bundled CLI and any system CLI, chooses `bundled` by default (`system` when selected),
   finds the TUI binary, picks the Claude login home, writes
   `prepared/deployment.json` (below) and selects it for **new** Claude agents.

If the archive is CLI-free, or `claudeCli` is `system`, and step 5 cannot find that CLI, the setting
update/install reports the missing system CLI. A marked bundled build has no separate CLI installation step. The component also shows
**needs you** when no Claude account is logged in yet (next section).

From a source checkout (install guide, path A), which downloads the pinned standalone Python from
GitHub and vendors the wheels from PyPI first (this build path needs a bootstrap Python 3.11+):

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
  "python": "/home/me/.local/share/cutex/components/claude-runtime/claude-r30/prepared/venv/bin/python",
  "adapter_source": "/home/me/.local/share/cutex/components/claude-runtime/claude-r30",
  "claude_cli": "/home/me/.local/share/cutex/components/claude-runtime/claude-r30/prepared/venv/lib/python3.13/site-packages/claude_agent_sdk/_bundled/claude",
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

Release archive: `bash components/claude-runtime/build.sh <linux-x64|windows-x64|darwin-arm64> <rN> <out-dir>`.
It defaults to the self-contained Python+CLI variant (`CUTEX_CLAUDE_BUNDLE_CLI=1`). Prefix the
command with `CUTEX_CLAUDE_BUNDLE_CLI=0` only when a CLI-free archive is required. The command
writes `component-claude-runtime-<rN>-<platform>.tar.gz` (the shipped source, pinned
python-build-standalone `install_only_stripped` payload (safely extracted on the target), licence notices, `wheels/` and
`requirements.lock`), `SHA256SUMS-component-claude-runtime-<platform>`
and `BUILD-INFO-component-claude-runtime-<platform>.json`. The component version is `version` in
`component.json` and must equal `release` in `src/cute_claude/features.json` (the build fails
otherwise). A version is published once and never changes: bump both whenever a shipped file or a
pin in `requirements-runtime.txt` changes. A new `claude-agent-sdk` pin also needs every platform
wheel's sha256 and the CLI-free sdist's sha256 in `scripts/vendor_wheels.py`. Standalone-Python assets, byte sizes and sha256 values are pinned in
`python-build-standalone-assets.json`; `CUTEX_PBS_CACHE` can name a verified asset or cache directory.

Release builds (`Build binaries`) package linux-x64 and windows-x64 only: there is no darwin cutex
release whose signed manifest could name a darwin-arm64 archive. On macOS (Apple silicon) build and
install it from a checkout: `cutex component install claude-runtime --from-source <checkout>`. A
signed release without a package for the host's platform answers `component_no_platform_package`.

Protocol notes: [docs/BUSY_DELIVERY.md](docs/BUSY_DELIVERY.md) (busy wakes and delegation results).

### Model, effort and fast mode while a turn runs (features `liveModelEffort`, `compactionModels`, claude-r24)

On Claude Code 2.1.295 or newer (`claude --version`, the model catalog's probe), `thread/settings/update`
changes model, effort and fast mode on the running Claude Code, also while a turn runs. The adapter sends
`set_model` and the `apply_flag_settings` control request (`effortLevel`, `fastMode`). Claude Code uses
them from its next model request, which can be the next step of the same turn; no child restart, no new
connection. On an older or unknown CLI, or while `CLAUDE_CODE_EFFORT_LEVEL` pins the effort, an effort
or fast mode change still replaces the idle child and is refused while busy (-32009).

Auto-compaction per model: cutex passes the profile's table (`claude-compaction-models.json`) in
`CUTEX_CLAUDE_COMPACTION_MODELS` = `{"<model id>": {"window", "percent"}}`. A model's entry is set
when Claude Code starts (`CLAUDE_CODE_AUTO_COMPACT_WINDOW` / `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`) and on
every model switch: through `apply_flag_settings` `env` on a live CLI, or by replacing the idle
child. Models without an entry use the profile-wide values; with none, Claude Code decides.

Behaviour checked against the real CLIs and the evidence: `src/cute_claude/live_settings.py`, and
`tests/test_live_settings_native.py` (opt-in, `CUTEX_TEST_CLAUDE_CLI`, fake loopback API).

### Cutex tools that hold for the owner (feature `approvalWait`, claude-r16)

cutex writes `approvalWaitSecs` into the cutex MCP descriptor for an adapter declaring
`features.approvalWait`. The adapter then runs each cutex façade call with
`CUTEX_MCP_APPROVAL_WAIT_SECS` and `CUTEX_MCP_TURN_ID` (the calling turn), so a gated
`service_register` / `service_update` / `agent_create` / `member_replace` holds on the Agent Bus for
the owner's decision, and gives those tools `approvalWaitSecs + 30` seconds instead of `--tool-timeout`.
`delegation_wait` always gets 150 s. Without `approvalWaitSecs` nothing is set and the façade answers
`approval_required` at once (cutex `docs/agent-tool-security.md` §7.1).

### Background subagents (feature `backgroundSubagents`, claude-r23)

`Agent` / `Task` with `run_in_background: true` (or without the argument) now runs in the background
past its turn, so the main agent stays free to read the owner's messages. The child's tools are
admitted by its agent id; cutex tools, nested subagents, questions to the user and approvals are
refused in it (under full access a safety check becomes a safety block on the origin turn). Its
transcript stays open until the task ends; `claude/tasks/stop` stops it. A finished, failed, stopped
or lost one (and a main-agent background shell command, at most one notice a minute) adds
`notifyParent` to `claude/task/updated`, which cutex delivers as one "Claude Task" message to the
agent. Its usage is added to the origin turn. Claude Code's own invisible notification turn stays
without tools. Details: docs/BUSY_DELIVERY.md, "Background subagents" and "Claude Code's
notification turn".

### Context usage while a turn runs (claude-r22)

`thread/tokenUsage/updated` follows each assistant message that brings new usage, not only the turn's
terminal result, so the context ring moves during long turns. Claude Code reports the context window
only with the result (`model_usage`) or the idle context read, so the mid-turn updates carry the
thread's last known `modelContextWindow` instead of null (none before the thread's first report since
the adapter started).

### Side chats and busy forks (features `sideChat`, `forkWhileBusy`, claude-r21)

A Side chat (cute-codex `/side`; cutex `docs/SIDE-CHATS.md` "Claude agents") is a temporary
conversation that sees the agent's context through a finished turn but never writes into it.
`claude/side/open {threadId, anchorTurnId?, lastTurnId?, model?}` copies the agent's native transcript
through that turn with the official SDK's `fork_session(up_to_message_id)` (read-only on the source:
no session lease, the running runtime is neither closed nor interrupted, a running turn is left out)
and binds the copy to a hidden thread (property `side`, not in `thread/list`) with the agent's model,
effort, permission mode and instructions. Its turns run like any thread's, in their own Claude Code
process. The Side inherits the agent's system prompt record (see "A changed system prompt reaches
resumed sessions" below), so it sends the agent's system prompt (and shares its prompt cache); the Side instructions and boundary are two text
blocks before the first Side message, not shown in the Side timeline. `claude/side/list|read|send|
interrupt|respond|close {threadId, chatId, ...}` answer cutex's Side snapshot (`events`,
`pendingRequests`, `revision`, long-poll `afterRevision` + `waitMs` up to 25 s); refusals carry the
cutex code in `error.data.code`. The Side's notifications and approval requests go to a private
in-process peer: never to the agent's subscribers, cutex's observer or `/v2/approvals`. `close`
deletes the hidden thread and its native copy; `claude/runtime/close` of the agent and
`claude/server/shutdown` close its Side chats first. State is in memory: a restart loses it and the
next Side request deletes leftover hidden threads.

`thread/fork` / `claude/thread/fork` of a busy thread (a running turn, queued inputs or notes) used to
fail with "interrupt the active turn …" / "finish or explicitly clear queued inputs …". It is now made
from the live transcript the same way (no close, no lease; the running turn and the queue are left
out; `lastTurnId` of a running turn is refused with `data.code: turn_in_progress`), and the receipt has
`sourceRuntime: null, sourceRuntimeUntouched: true`. An idle thread is forked as before (its runtime
is closed and restarted around the copy). Both wait up to 3 s for the kept turn's last message to
reach the transcript and refuse with `data.code: native_transcript_behind` (retry) rather than guess
where a just-started turn begins. Opt-in real-CLI check against a loopback model:
`CUTEX_SIDE_FORK_NATIVE_SMOKE=1 python -m pytest -q -s tests/test_side_fork_native.py`.

### A changed system prompt reaches resumed sessions (claude-r21)

Claude Code records a session's system prompt on its first request and sends that record on every
later request and resume, even when a later launch passes another `--append-system-prompt`, until a
compaction (`systemPromptSnapshot`, SDK `system_prompt.snapshot`, on by default; bundled CLI 2.1.299).
So a new `# Cutex` harness, AGENTS.md / memory text, runtime-limits note, no-exec note, auto-mode note
or thread instructions never reached an agent whose session had started before the change. The adapter
now keeps the SHA-256 of the `append` the session recorded (thread property `systemPromptRecord`; set
when a new session starts and after each compaction, which re-records the launch prompt). A resume
with the same `append` keeps the snapshot (same prompt-cache prefix); a resume with another `append`,
or of a session started before this change (no record), runs with `snapshot: false`: Claude Code
renders the prompt fresh on every request, as before snapshots existed. The first request after a
change therefore misses the prompt cache once; such a session stays without a snapshot until its next
compaction (or until the text is back to the recorded one). Forks, reverts and Side chats copy the
record with the thread properties. Opt-in real-CLI check against a loopback model:
`CUTEX_SYSTEM_PROMPT_NATIVE_SMOKE=1 python -m pytest -q -s tests/test_system_prompt_native.py`.

### Foreground subagents only (claude-r21; replaced in claude-r23)

Every Agent / Task call had to pass `run_in_background: false`; leaving it out was refused too, because newer
Claude Code runs a subagent in the background by default. A background subagent's later turns had no
adapter input, so they could not use tools once the parent turn ended. claude-r23 admits them (above).

### Inputs wait for a turn Claude Code started by itself (claude-r21)

When a background command finishes, Claude Code starts a turn by itself. A cutex input that arrived
while that turn was still streaming used to be submitted at once and was then credited with the rest
of that turn and its result (wrong reply, early completion). It now waits until that turn ends: its
result, 3 s without a frame of it, or at most 120 s (docs/BUSY_DELIVERY.md, "Autonomous turns").
Interrupting or closing does not wait.

### Large tool results (claude-r20)

A single message from Claude Code may be up to 64 MiB (the SDK's `max_buffer_size`; its default is 1 MiB). Before,
a tool result over 1 MiB, typically a screenshot or a large file read, ended the turn with
`SDK receive failed (CLIJSONDecodeError)`.

### Auto mode and cutex approvals (claude-r19)

When a thread's Claude permission mode is `auto` (desktop 「自动」) as its Claude Code session starts,
the runtime-limits note in the preset system prompt append also says that this mode only covers Claude
Code's own tool permissions: cutex approvals (managed and persistent services, delegation reviews,
browser grants, other cutex tool approvals) still go to the owner, and such a call may wait for the
decision. The note follows the mode the session started with; a switch while it runs changes Claude
Code's permission mode at once but not the prompt until the session's next start.

### Steers after an interrupt, runtime limits note (claude-r18)

An owner `turn/interrupt` drops the running turn's unread steers when it is requested, not only when
the turn ends `interrupted` / `failed` (claude-r17): the client resends them (desktop interrupt &
send), so a PostToolUse before the abort lands, or a turn that completes anyway, never delivers them
a second time. A failed interrupt request puts them back. Steers with images are dropped the same way.
The preset system prompt append now ends the host text with a short runtime-limits note: background
subagents are refused (use foreground ones), and a turn Claude Code starts by itself when a background
shell command finishes cannot use tools and is not shown (docs/BUSY_DELIVERY.md, "Autonomous turns").

### Hooks and auto memory (claude-r17)

With the profile's command execution off (`--allow-exec` not given) the adapter's flag settings
(`--settings`, also the live `apply_flag_settings`) carry `disableAllHooks: true`, so hooks from the
project / user settings sources, skill or agent frontmatter and plugins, and a statusLine command,
never run; with exec on hooks behave as before. The adapter's own hooks are SDK callbacks registered
through `initialize` and keep running (verified against Claude Code 2.1.289). Claude Code's built-in
auto memory is off for every Claude agent (`CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`), not only when a
settings source is on.

### Turns cut off by a stop or restart (claude-r16)

`claude/runtime/close` (cutex Offline / Restart runtime / profile switch / archive) interrupts a running
turn first, like cutex's `turn/interrupt` before a Codex runtime stops: the turn ends `interrupted`,
its open tool calls closed, and the terminal events reach subscribers before the close returns. Only an
interrupt Claude Code confirms within `interrupt_timeout` claims that; otherwise the turn ends `failed`
(outcome uncertain) as before. A turn the previous adapter process left running (killed, crashed) is
settled on the next start: `interrupted`, every open item closed (`failed`), its input `uncertain`, the
queue paused; the settling `item/completed` / `turn/completed` / `thread/status/changed` events are sent
to each peer that resumes the thread (`thread/resume`) until the thread's next turn starts.

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
(each wheel carries its license in its `.dist-info`). The default release build
(`CUTEX_CLAUDE_BUNDLE_CLI=1`) carries the official platform SDK wheel and its unmodified Claude Code
binary subject to Anthropic's separate Commercial Terms and documented product-preinstallation
conditions. `CUTEX_CLAUDE_BUNDLE_CLI=0` instead builds the MIT SDK source without the bundled CLI.
