# Independent instances (`CUTEX_HOME`)

A second cutex can run next to your main one with its own state directory, its own
ports and its own credentials, from the same release binary. Use cases: a workbench
that must never touch the main agents, a scratch daemon for experiments, an
end-to-end test daemon. (Debug builds also have the test-only `CUTEX_TEST_PRIVATE_HOME`
boundary, see §1.)

## 1. Selecting an instance

| Variable | Meaning |
|---|---|
| unset | the **default instance**: `~/.cutex` (unchanged behaviour) |
| `CUTEX_HOME=<absolute dir>` | an **independent instance** whose state directory *is* `<dir>` (the equivalent of `~/.cutex`, not an OS home) |
| `CUTEX_TEST_PRIVATE_HOME` | unchanged: debug builds only, test boundary with marker file; combining it with `CUTEX_HOME` is refused |

`CUTEX_HOME` is the only selector (no `--home` flag): an environment variable reaches
every process cutex starts for the instance without plumbing — the Agent Bus that
`management serve` spawns, `cutex notify` callbacks from the native TUI, `cutex` run by an
agent's shell tool. Where cutex launches a native with a scrubbed environment it re-adds
`CUTEX_HOME` (stock launch, the native TUI workflow, the config catalog app-server), so a
nested `cutex` stays in the instance. Release and debug builds behave the same.

`CUTEX_HOME` must be absolute, valid UTF-8, without `..`, and must not be, contain, or be
inside the default home `~/.cutex`, nor be inside `~/.codex`, `~/.codez-cli`,
`~/.codex-codez` or `~/.claude`. The check resolves symlinks of the existing ancestors, so
an alias of a protected directory is refused too (comparisons are case-insensitive on
Windows). `CUTEX_HOME` naming the default home itself selects the default instance.

Older binaries ignore `CUTEX_HOME`: run an instance only with a build that has this feature
(`cutex instance show` exists).

## 2. Instance configuration

`<CUTEX_HOME>/config.json` (written by `cutex instance init`):

```json
{
  "instance": { "name": "scratch", "isolated": true, "remote_listeners": false },
  "management_port": 25270,
  "agent_bus_port": 25260,
  "management_api_token": "<64 hex, generated>",
  "agent_bus_token": "<generated>",
  "management_listen": ["127.0.0.1"]
}
```

- `instance.name`: `[a-z0-9][a-z0-9-]{0,31}`. Shown in `/v2/capabilities` and
  `cutex instance show`; qualifies Linux systemd scope names.
- `instance.isolated`: must be `true` (the default when omitted) in an independent
  instance. `false` is reserved for a future non-isolated secondary instance and refused.
- `instance.remote_listeners` (default `false`): see §4.
- `management_port` (new, top-level; also honoured by the default instance, where unset
  means 24270): the port `cutex management serve` listens on when `--port` is omitted and
  the port every local client of the instance uses (CLI, internal control-plane clients,
  lifecycle commands). Before this, clients always assumed 24270.
- `agent_bus_port` (existing): the instance's Agent Bus.

There is no API to change `instance` or `management_port` at run time; `/v2/settings` can
change `agentBusPort` and `managementListen` subject to the guards below.

## 3. Guards (fail closed)

Checked by a preflight before every command except `cutex instance init|show`, and again
where the guarded value is used:

1. **Initialized instance**: an independent instance without a valid `instance` section
   refuses to run anything (no half-created state in an arbitrary directory).
2. **Not isolated on the default home**: an `instance` section with `isolated: true` in
   `~/.cutex` is refused; `isolated: false` in a `CUTEX_HOME` instance is refused.
3. **Ports**: `management_port` and `agent_bus_port` are required, distinct, in
   1024–65535 and never one of the default instance's well-known ports (24270 management,
   24260 bus, 24670 / 24660 their tunnel ports, 24250 desktop notify). Applies to
   `--port`, `management_listen` entries, `/v2/settings agentBusPort` (clearing it is
   refused: the instance would fall back to 24260) and every helper that used to
   normalise the bus back to 24260. The default instance keeps the Bridgeboard 24xxx rule.
4. **No silent redirection**: `CUTEX_MANAGEMENT_URL` (an override honoured by CLI control
   clients) must be a loopback URL on the instance's management port inside an instance,
   otherwise the command fails instead of sending the instance's root bearer elsewhere.
   Server commands (`management serve`, `agent serve`) of an instance drop
   inherited per-agent identity variables (`CUTEX_AGENT_*`, `CUTEX_RUNTIME_*`,
   `CUTEX_MCP_THREAD_ID`, `CUTEX_MANAGEMENT_URL`) at start: they belong to whatever agent
   shell launched the daemon, typically one of the main instance.
5. **Owner-private home**: Unix: owned by the current user, mode without group/other bits
   (`init` creates it 0700). Windows: a protected DACL granting only the current user
   (`init` applies it with the same primitive the runtime state roots use,
   `private_fs::secure_directory`). This is why a home in a directory with inherited
   permissions for other users (e.g. `Authenticated Users:(M)`) works only after `init`
   secured it.
6. **Separate credentials**: `init` generates a fresh Management root token and Agent Bus
   token (distinct) and never copies one from another home; both are required.
7. **State stays inside the home**: the selected runtime (`runtime/light/deployment.json`)
   must have its native home inside the instance; a Job MCP descriptor's private objects
   (endpoint, API token, grant key) must be inside the instance; a Claude deployment's
   login home (`claude_home`) must be inside the instance; a native launch (app-server or
   TUI) is refused for an explicit-launch contract whose native home lies outside. Binaries
   may live anywhere (they are read-only and hash-pinned).

## 4. Side effects that are off in an instance

| Side effect | Default instance | Instance |
|---|---|---|
| Bridgeboard `handoff` registration (management, bus) | on (if `bridgeboard` is on PATH) | off |
| Agent Bus federation (24660 tunnel probe, `bridgeboard list --peers`, `bridgeboard up`) | on | off: the bus sees only its own agents |
| `management remote-up`, `agent remote-up` (Bridgeboard tunnels) | on | refused (start an SSH tunnel yourself) |
| Service controls (TUI Services page): systemd user units `cutex-agent-bus` / `cutex-management-api`, runtime-host | on | refused: they operate the default instance's machine-wide services |
| Legacy layout migration (`~/.codez-cli` → `~/.cutex`, `~/.codex-codez` → `codex-home`) and legacy native index lookups | on | off (legacy paths resolve inside the instance and do not exist) |
| Remote management listeners (`tailscale`, LAN, wildcard) | as configured | refused unless `instance.remote_listeners: true` |
| Linux transient scopes for agents | `cutex-agent-<id>.scope` in `cutex-agents.slice` | `cutex-<name>-agent-<id>.scope` in `cutex-<name>-agents.slice` |
| Windows Job objects for agents | `Global\cutex-agent-<id>` | `Global\cutex-<name>-agent-<id>` |

cutex itself writes nothing outside its home: Start-menu shortcuts, taskbar pins and the
`HKCU\…\Run` key belong to the desktop app and runtime-host installers, systemd units to
the deploy scripts; none of these are touched by the daemon. Stock publication evidence
is under `<home>/runtime/stock-claims` (its Windows Job object name derives from that
file's identity, so it cannot collide). Short-lived files in the OS temp directory are as
before.

## 5. What is shared and what is isolated

| Isolated per instance (inside `CUTEX_HOME`) | Shared |
|---|---|
| `config.json`, tokens, client tokens, `instance-id` | release binaries: `cutex`, `cutex-mcp` |
| accounts / profiles (+ their auth), sessions store, Agent Bus registry, asks, approvals, owner notifications, notifications outbound config | native binaries referenced by a bundle manifest (app-server, TUI, code-mode host, schema) — read-only, sha256-pinned |
| Management v2 journal, idempotency store, usage ledger, logs | Claude adapter source / python / Claude CLI binary (read-only) |
| native home (`codex-home`: `config.toml`, threads, skills, plugins, memories) and every per-agent native home | the OS user (HOME for cwd defaults, `~` expansion, directory browsing) |
| Claude login home, Claude session state | `hostId` (machine name); clients tell daemons apart by `instanceId` |

## 6. Reusing an existing deployment's runtime (read-only)

A bundle manifest binds its binaries **and** a native home (`shared_config` must be
`<native_home>/config.toml`), so another deployment's manifest cannot be selected as is.
`cutex instance use-runtime <manifest>` derives one without touching the source:

1. reads the source manifest and verifies every binary reference (path + sha256);
2. creates `<home>/codex-home/config.toml` (empty table) if missing;
3. writes `<home>/runtime/bundles/<source-stem>-<sha256[..12]>.json`: the same binary references, `shared_config`
   rebased onto the instance's `codex-home/config.toml`;
4. installs `runtime/light/deployment.json` `{native_home: <home>/codex-home,
   bundle_manifest: <that file>, job_mcp: null}`;
5. `--registry <file>` copies a native version registry (display-only metadata) into
   `<home>/runtime/native-version-registry-v1.json`.

Nothing under the source release, its native home or its `deployment.json` is written. The
facade (`cutex-mcp`) of the source release works for any instance: it receives its Bus URL
and credential per launch. Claude: `cutex human claude install <file>` inside the instance,
with `claude_home` inside the instance (binaries may stay in a shared runtime dir).

## 7. Connecting a client (desktop, workbench, scripts)

- Endpoint: `http://127.0.0.1:<management_port>`; `cutex instance show --json` prints
  `{name, isolated, initialized, home, configPath, managementPort, managementUrl, agentBusPort,
  remoteListeners}` (no
  secrets).
- Credential: `management_api_token` in `<home>/config.json` (root), or better a dedicated
  client token: `CUTEX_HOME=<home> cutex management client-token issue --label <client>`.
  Remote listeners (only with `remote_listeners`) accept client / device tokens only.
- Verify: `GET /v2/capabilities` → `instance {name, isolated}`, `instanceId`, feature
  `instanceHome`.

## 8. Recipes

Linux:

```sh
export CUTEX_HOME=$HOME/.cutex-scratch
cutex instance init --name scratch --management-port 25270 --agent-bus-port 25260
cutex instance use-runtime ~/path/to/release/manifests/default.json
cutex management serve        # foreground; starts the instance's bus on 25260
```

Windows (PowerShell): `$env:CUTEX_HOME = "$env:USERPROFILE\.cutex-scratch"`, then the same
commands. `init` on an existing home without an `instance` section adopts it (keeps its
tokens and state, adds the section, the ports and missing tokens) if the guards pass.

Moving an existing hand-made home into an instance: stop its daemon and agents first, run
`init` with the ports it used so clients keep working, then `use-runtime`. Its agents'
Linux scopes are named `cutex-<name>-agent-…` from then on, so stop running agents before
switching.

## 9. Not in v1

Bridgeboard registration under instance-qualified service IDs; federation between
instances; a relocated non-isolated main home; desktop discovery of instances (the desktop
reads `~/.cutex/config.json` and `CUTEX_MANAGEMENT_URL`; for an instance point it at the
instance's port and token).
