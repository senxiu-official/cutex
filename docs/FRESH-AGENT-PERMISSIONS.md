# Fresh-install agent permissions

Capability `fresh-agent-auto-permissions` (owner decision 2026-10-15). Code:
`src/config/new_thread.rs` (`LaunchPermissions::builtin`), `src/config/fresh_agent_permissions.rs`
(migration), hook at the top of `cli_app::app::run`.

## Behaviour

- The built-in default for **new agents** (`cutex human new`, `POST /v2/human/agents`, when
  `config.json` `new_thread_defaults.agent` does not set a field) is the desktop composer's 「自动」 mode:
  sandbox `workspace-write`, approval `on-request`, network access on, no extra writable roots. Before it was
  `danger-full-access` + `never` (network off).
- Quick chats are unchanged (`workspace-write` + `on-request`, network off).
- `/v2/settings` `newThreadDefaultsBuiltin.agent` shows the new built-in; `newThreadDefaults.agent` shows the
  effective defaults (stored fields over the built-in), so the desktop's new-agent form and Settings → 新建默认
  show whatever the home resolves to. Resetting the agent kind (`PATCH /v2/settings {newThreadDefaults:
  {agent: null}}`) now means the auto mode.
- Existing agents are never changed: each session record keeps the permissions it was created with.

## Fresh vs existing home (one-time decision)

Every `cutex` process (CLI, `management serve`, `agent serve`, …) calls
`fresh_agent_permissions::run_on_startup` right after argument parsing / instance preflight, before any
command writes into the home. The first process of this release decides once and writes
`~/.cutex/archive/fresh-agent-permissions/migration.json`; any later process sees the record and does
nothing, so a home this binary created is never taken for an existing one on its second start.

- **Existing** = any of `accounts.json` (profiles), `cutex-sessions.sqlite3`, `cutex-sessions.json` or the
  `runtime/` dir in the cutex dir, the legacy `~/.codez-cli` dir (non-instance homes), or a `config.json` with
  a key outside the bootstrap set `agent_bus_enabled`, `agent_bus_token`, `agent_bus_port`,
  `management_api_token`, `management_port`, `management_listen`, `instance` (e.g. `new_thread_defaults`,
  `default_profile`, any other setting). A `config.json` with only bootstrap keys (what the desktop install
  guide (cutex-desktop `docs/install/AGENT-INSTALL.md` §5.1) writes before the first `cutex` command: the two tokens and
  `agent_bus_enabled`) shows the home was prepared, not used, and is not evidence; neither is `codex-home/`.
  Each of `sandbox` / `approval` /
  `network_access` missing (or `null`) in `config.json` `new_thread_defaults.agent` is written with the old
  built-in value (`danger-full-access`, `never`, `false`); fields the owner set stay. The config is edited as
  raw JSON (unknown fields kept) before the record is written, so an interrupted run repeats harmlessly.
- **Fresh** = none of these (no config, or a bootstrap-only one): only the record is written, `config.json`
  is left untouched; new agents get the auto mode.
- Record (schema `cutex/fresh-agent-permissions-migration/v1`): `at`, `home` (`fresh` | `existing`),
  `evidence` (the files found), `previousAgentDefaults` (the stored `new_thread_defaults.agent` before, `{}` or
  `null`), `written` (the fields set).
- Errors (unreadable / unparsable / non-object `config.json`, unwritable dir) never fail the command; nothing is recorded
  and the next process retries.

## Rollback

- Existing homes: an older binary reads the explicit `new_thread_defaults.agent` values (`danger-full-access`,
  `never`, network off) — exactly its own built-in, so nothing changes. Binaries from before
  `new_thread_defaults` ignore the block (and drop it on their next config save) and always used full access.
- Fresh homes: no explicit values are stored, so an older binary creates new agents with its built-in full
  access again. Agents created under this release keep their stored auto permissions on relaunch.
- Moving forward again: the record exists, nothing is re-decided.
- To give an existing home the new default on purpose: Settings → 新建默认 → 自动, or `PATCH /v2/settings
  {newThreadDefaults: {agent: null}}`.
