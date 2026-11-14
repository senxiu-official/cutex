# claude-runtime

`claude-runtime` is the optional component that runs Claude agents in cutex.
Each agent has its own Python app-server process. It drives the unmodified
Claude Code CLI through the official Claude Agent SDK, using your Claude
subscription login or an Anthropic-compatible API key.

## Public component

**claude-r48** contains adapter **0.8.15**, Claude Agent SDK **0.2.180**,
Claude Code **2.1.321** and bundled CPython **3.13.17**. Release archives are
provided for Linux x64 and Windows x64. macOS uses the desktop as a client
of a supported host; the source component also declares Darwin arm64.

The component supports the advertised app-server subset, including message
streaming, images, approvals, questions, interruption, busy input, history,
forks, Side chats, usage totals and the browser command/script tools.
Capabilities are read from `src/cute_claude/features.json`; they describe
adapter support and do not grant access to an account or model.

System instructions use a private UTF-8 append file, avoiding the Windows
command-line length limit. A matching host permits a combined instruction
budget of 64 KiB. The file is replaced atomically at runtime start, remains
available while the CLI and its children run, and is removed on explicit
thread deletion. Foreground native Agent children keep Claude Code's own
independent system prompt. The preset, prompt digest, resume snapshot and
compaction behavior are preserved.

## Install and sign in

From a managed cutex installation:

```sh
cutex component install claude-runtime
cutex component status claude-runtime
```

The desktop provides the same installation action where a Claude agent or
Profile requires the component. The signed backend manifest selects the
archive; cutex verifies it and prepares the bundled Python environment
without downloading replacement dependencies at runtime.

Sign in through the desktop's Claude account flow or configure your own
API-key Profile. Updating the adapter preserves account state. You can
inspect configured accounts with:

```sh
cutex human claude account list
```

Existing agents retain their pinned runtime. Use the desktop runtime
upgrade controls to move selected agents to this component. Do not replace
or remove files belonging to a pinned version.

## Bundled or system Claude Code

The release includes the unmodified SDK-matched CLI. Claude Code has its own
license and terms; the SDK's MIT metadata does not relicense the CLI. Users
use their own credentials and account access.

To use a separately installed CLI, set the Management setting:

```json
{"claudeCli":"system"}
```

A source build can set `CUTEX_CLAUDE_BUNDLE_CLI=0` to omit the CLI. Such a build
requires a compatible system installation. A normal signed bundled component
must report a missing bundled file as a failed component check instead of
silently selecting an external runtime.

## Build from source

From this public source checkout, with Git, GNU tar, gzip, sha256sum and a
bootstrap Python 3.11 or newer:

```sh
bash components/claude-runtime/build.sh linux-x64 claude-r48 /path/to/output
```

Use `windows-x64` for the Windows package. The builder checks that
`component.json` and `features.json` declare the same component identity,
exports committed LF bytes, downloads hash-pinned wheels and CPython, and
produces the archive, checksums and build record. It never packages tests
or a prepared virtual environment. Release builds and signing run through
the repository's GitHub Actions pipeline.

Each published component identity is immutable. Any later payload change
requires another unused component number.

## Approval notices and background tasks

On a matching host, approval-notice delivery receipts can bound the approval read
window. The component uses Agent SDK 0.2.180 and its unmodified bundled CLI.
Background children remain active while the SDK inventory reports them running;
their terminal task events and completion notifications settle them. Existing
sessions retain their selected runtime until explicitly upgraded.

## Transient request recovery

The adapter bounds automatic retries of recoverable connection and provider failures. It resumes saved input at most once for a completed failure, preserving the existing permission checks and active request identity. Authentication, account, permission and cancellation errors do not enter transient retry. Direct stdio writes UTF-8 JSON independently of the process locale. Provider availability and network connectivity still determine whether a request succeeds.
