<p align="center">
  <img src="assets/logo.png" width="160" alt="Cutex logo">
</p>

<h1 align="center">cutex</h1>

<p align="center"><b>English</b> · <a href="README.zh-CN.md">简体中文</a></p>

**0.1.5**: owner devices (new owner devices are announced, host-control actions only on the host or
over SSH, per-device scope full / chat / read, the owner in person), Claude Side chats and busy forks
(claude-r21), cross-host wake, home pages and remote close of a desktop on the host, CC Switch import,
and a rollback that never widens a device's access (0.1.x is "First Take"; 0.1.0 was the first
public release). See [CHANGELOG.md](CHANGELOG.md).

> **Known gaps in 0.1.x** (being fixed; this note goes away when they are):
> - **Phone push / pairing a phone**: there is no public phone app yet.
>
> Claude agents need the optional `claude-runtime` component and long-running jobs (Linux) the
> `job-service` component: `cutex component install <name>` on a managed install, or the desktop's
> **Install** button ([components/README.md](components/README.md)).

cutex is a daemon that runs coding agents on your own machines. Agents run on
[cute-codex](https://github.com/senxiu-official/cute-codex) (a fork of OpenAI Codex) or on Claude Code
(through the adapter in [`components/claude-runtime/`](components/claude-runtime/)). cutex holds all the state: agents,
projects, queues, delegations, Profiles and usage.

Clients talk to it through the Management v2 HTTP / SSE API. The main client is the
[desktop app](https://github.com/senxiu-official/cutex-desktop). There is also a command line
(`cutex …`) and a session TUI (`cutex` with no arguments).

Linux and Windows x64.

## Features

**Agents working together**
- Agent Bus: agents list each other and send messages (`agent_list`, `agent_tell`). Each agent
  authenticates with its own credential.
- Delegations: an agent hands a task to another agent (`agent_delegate`), waits for the result or
  keeps working, reads progress, and answers the other agent's questions (`question_answer`).
  The owner sees every delegation and can pause them.
- Long messages and results are stored as documents that the receiver reads on demand. Closed
  delegations and messages go to a searchable archive.
- Projects group agents around a workspace folder. A project may have a Director; the Director
  and the operators it grants create, start, stop and replace members. Agents in different
  projects cannot see each other unless the owner links the projects or grants a specific agent.
- Project memory is added to each member's instructions at launch.
- Alarms: an agent schedules a message to itself or another agent for later. The owner can list
  and cancel them.
- Owner notifications: an agent can message the owner (`owner_notify`) and ask for a reply.
  Notification levels decide which events reach you and how; the notification worker forwards them
  to a webhook (it runs inside the Management API of a `cutex service install` install; elsewhere run
  `cutex notify run` as its own service).
- Owner approvals: actions beyond an agent's own permissions, such as using a Profile marked
  "needs approval", wait until the owner approves or denies them.
- Workflows: an approved script starts one-shot step agents and collects their results.
- Notes and side chats: owner-private notes (never shown to agents), and temporary side
  conversations next to a Codex agent.
- Cross-host peers: pair two cutex daemons, then agents delegate to and message `name@host`.
  Off until you pair.
- Users and devices: the API listens on loopback and, if you enable it, on a tailnet address.
  Devices pair with a one-time code and get their own tokens. Other users can be given access to
  specific projects and agents, and manage their own devices.

**Tools agents get**
- The `cutex` MCP server gives each agent the bus, delegation, alarm and project tools above,
  filtered by its role.
- Services: agents register the web services they run (`service_register`, `service_list`, …);
  the owner opens them through an authenticated proxy.
- Artifacts: agents publish results for the owner and other agents (`artifact_publish`,
  `artifact_list`, `artifact_read`).
- Browser: agents use in-app browser tabs that the owner lends from the desktop (`browser_*`).
- Web search through the agent's own provider (`web_search`), a per-Profile switch for Codex
  API-key Profiles, off by default.

**Profiles and usage**
- Profiles for ChatGPT sign-in, Claude accounts (subscription or API key) and OpenAI-compatible
  API keys, with provider presets and model lists. Named model configurations of one model.
- Each agent uses one Profile and can switch to another of the same provider family.
- Quota and plan where the provider reports them. A Claude turn stopped by a usage limit can
  continue on its own when the limit resets.
- Token usage per agent, Profile, model and day, with cost estimates from a price list, in
  USD, AUD or CNY. Usage of Claude Code, Codex and ZCode runs outside cutex can be imported.
  The ledger stores counts and ids only, never prompts or responses.

**Sessions**
- One agent is one durable session and one native thread. Sessions outlive client windows and
  daemon restarts; after a reboot cutex offers to bring the previous runtimes back.
- Queue, steer or stop a turn. Input for an offline agent can start it.
- Fork an agent, revert it to an earlier turn, or revert only the files a turn changed.
  Archive or delete it.
- Files, git status and diffs, terminals and image attachments go through the API, so a client
  on another machine works the same way.

**Host**
- Capability discovery (`GET /v2/capabilities`) and JSON Schemas of the API
  (`cutex management schema export`).
- Health check of the host (`GET /v2/health`).
- `cutex service install` installs the daemon as user services (systemd user units on Linux, a
  logon task on Windows): the Agent Bus and the Management API, which also runs the notification
  worker (webhooks, notification states).
- Optional components, installed, updated and removed on demand (`cutex component`,
  `/v2/components`): `claude-runtime` (Claude agents, Linux and Windows) and `job-service`
  (long-running agent jobs, Linux). See [components/README.md](components/README.md).
- Independent instances in separate homes (`CUTEX_HOME`).

## Install

Install from the release binaries or build from source. The full guide covers both, for cutex,
cute-codex, the optional components and the desktop on Linux and Windows. It is written so that either
you or an AI coding agent can follow it end to end:

**[AGENT-INSTALL.md](https://github.com/senxiu-official/cutex-desktop/blob/release/docs/install/AGENT-INSTALL.md)**
(in the cutex-desktop repository)

**Release binaries.** Each [GitHub Release](https://github.com/senxiu-official/cutex/releases)
carries `cutex` + `cutex-mcp` for Linux x64 and Windows x64, built by GitHub Actions from the tagged
commit, and a signed `manifest.json`. Every file is signed with minisign (`<file>.minisig`); verify it
with the release key, the same key cutex trusts for backend updates (`src/upgrade/keys.rs`):

```sh
minisign -Vm <file> -P RWRH9INhAb2gACJAg/h5EBjbudos2LXj6K+5hRBEGeVLDKaKzE+FICmS
```

The guide's section B installs them as a managed install, which then updates through
`cutex upgrade` / the desktop's backend updates. The desktop apps for Windows and macOS are not
code-signed with a paid certificate, so Windows SmartScreen and macOS Gatekeeper warn on the first
start (the guide says what to click); the command-line binaries here are not affected.

**From source**, the cutex build itself, with Rust 1.95:

```sh
git clone --branch release https://github.com/senxiu-official/cutex.git
cd cutex
cargo build --locked --release --bin cutex --bin cutex-mcp
./target/release/cutex --version
```

`cutex` is the daemon, CLI and TUI. `cutex-mcp` is the MCP server agents use. The guide continues
with the cutex home, `cutex service install`, and the cute-codex build or Claude runtime that agents
run on.

## Runtime model

| Component | Role |
|---|---|
| cute-codex | Codex agents: the native app-server, model calls and tools |
| `components/claude-runtime/` | Claude agents: an app-server adapter over the Claude Agent SDK and Claude Code |
| Profiles | Credentials and settings of one provider account, kept apart per Profile |
| Session store | Durable agent identity, native thread id, cwd, project, Profile and lifecycle |
| Managed runtime | The long-lived native process of one agent; it keeps running when no client is attached |
| Management host | The HTTP / SSE API, runtime recovery, events and owner actions |
| Agent Bus | Discovery and authenticated message delivery between agents |

`online`, `offline` and `close` apply to the managed runtime; closing a terminal or the desktop does
not stop it. Archiving a session keeps its native history.

## Documentation

- [docs/management/human-cli.md](docs/management/human-cli.md): `cutex human`, the owner's
  command line
- [docs/management/human-configuration.md](docs/management/human-configuration.md): per-agent
  configuration
- [docs/management/user-input-delivery.md](docs/management/user-input-delivery.md): how user
  input is queued and delivered
- [docs/PEERS.md](docs/PEERS.md): pairing daemons and cross-host delegations
- [docs/SERVICES.md](docs/SERVICES.md): agent services and the proxy
- [docs/NOTES.md](docs/NOTES.md) and [docs/SIDE-CHATS.md](docs/SIDE-CHATS.md): notes and side
  chats
- [docs/NOTIFICATION-LEVELS.md](docs/NOTIFICATION-LEVELS.md): notification levels
- [docs/agent-tool-security.md](docs/agent-tool-security.md): agent credentials, project
  boundaries, roles and approvals
- [docs/agent-management-provider.md](docs/agent-management-provider.md): projects, Directors
  and operators
- [docs/settings-defaults.md](docs/settings-defaults.md): defaults for new sessions
- [docs/usage-pricing.md](docs/usage-pricing.md): usage and cost estimates
- [docs/profile-model-configurations.md](docs/profile-model-configurations.md): named model
  configurations
- [docs/FILES-REVERT.md](docs/FILES-REVERT.md): reverting the files of a turn
- [docs/health-check.md](docs/health-check.md): host health check
- [docs/backend-upgrade.md](docs/backend-upgrade.md): managed installs, `cutex service install`
  and signed backend updates (`cutex upgrade`)
- [components/README.md](components/README.md): optional components (`cutex component`)
- [docs/INSTANCES.md](docs/INSTANCES.md): independent instances (`CUTEX_HOME`)
- [docs/agent-bus-operations.md](docs/agent-bus-operations.md): Agent Bus token, audit log and
  stuck messages
- [docs/PROFILE-SEARCH-TOOL.md](docs/PROFILE-SEARCH-TOOL.md): the `web_search` tool
- [docs/SESSION-FAST-MODE.md](docs/SESSION-FAST-MODE.md): fast mode per session

## Status

This is a first take. Its author uses it every day, but few others do yet.

- **Remote access assumes your own network.** The API listens on loopback by default. Reach
  another machine over SSH or a private tailnet that you trust; do not expose the Management or
  Agent Bus ports to the internet.
- **Users are a convenience.** User identities make it easy to share a host with people you know;
  they are not a security boundary against strangers.
- **Upgrades.** A managed install (`cutex service install`) applies signed releases with
  `cutex upgrade` or the desktop's backend updates (see
  [docs/backend-upgrade.md](docs/backend-upgrade.md)).
  A source build can also be updated from source.
- On Linux, agents can use the Job Service for long-running commands, an optional component
  ([components/job-service/](components/job-service/)).

### Local data

cutex keeps its state in `~/.cutex` (Windows: `%USERPROFILE%\.cutex`): `config.json` (settings and
tokens), `accounts.json` and `profiles/` (Profiles and their credentials), `cutex-sessions.sqlite3`
(the session store), `codex-home/` and `runtime/` (runtime state, logs, stores). These files hold
credentials, private paths and records of your work. Do not publish the directory or copy it into
a release.

API keys and tokens stay in the daemon. Agents get their own bus credential, not the root token,
and shell tools started by a Codex agent do not see the provider key.

## Build from source (development)

```sh
cargo fmt --check
cargo clippy --all-targets --locked
cargo test --locked -- --test-threads=1
cargo build --release --locked
```

Tests must not touch your real `~/.cutex`. Debug builds and test processes refuse it
(`.cargo/config.toml` sets `CUTEX_TEST_FORBID_REAL_HOME=1`). To run a debug daemon or the tests
against a home of their own, create a directory containing an empty marker file
`.cutex-test-private-home` and point both variables at it:

```sh
mkdir -p ~/cutex-test-home && touch ~/cutex-test-home/.cutex-test-private-home
CUTEX_TEST_PRIVATE_HOME=~/cutex-test-home HOME=~/cutex-test-home cargo test --locked -- --test-threads=1
```

cutex then uses `~/cutex-test-home/.cutex`. Release builds refuse to run with `CUTEX_TEST_PRIVATE_HOME` set. Run a
debug `cutex management serve` on its own port (`--port`) so it never meets an installed daemon.
The same processes also refuse the default ports (24270, 24260, 24670, 24660, 24250): a test home
without a bus port of its own gets a free one, kept in its `config.json`.

The Claude adapter is a Python package with its own tests in `components/claude-runtime/`.

Bug reports are welcome as GitHub issues. If cutex is useful to you, a star on GitHub helps others
find it.

## Acknowledgements

Built with Codex, Claude, DeepSeek, GLM and Kimi. Product names are trademarks of their
owners; naming them implies no affiliation or endorsement.

Codex agents run on cute-codex, a fork of [OpenAI Codex](https://github.com/openai/codex)
(Apache-2.0). cutex uses the Codex 0.154.0 default model instructions unmodified; see
[NOTICE](NOTICE).

## License

Apache License 2.0: see [LICENSE](LICENSE) and [NOTICE](NOTICE). `components/claude-runtime/` is under the MIT
License ([components/claude-runtime/LICENSE](components/claude-runtime/LICENSE)).
