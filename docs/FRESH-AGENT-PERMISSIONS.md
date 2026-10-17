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

## Automatic reviewer (`launch-approvals-reviewer`, 2026-10-16)

Owner decision: 「自动」 is `workspace-write` + `on-request` + the native automatic reviewer
(`approvals_reviewer = "auto_review"`), so escalations (writes outside the workspace, network when it is off,
`require_escalated` commands) are decided by Codex's reviewer subagent instead of being sent to the owner.

- Built-in `approvalsReviewer` of both kinds (`LaunchPermissions::builtin`) is `auto_review`; it only matters
  with `on-request` (the native routes to the reviewer only for interactive policies).
  `config.json` `new_thread_defaults.<kind>.approvals_reviewer` overrides it (`/v2/settings`
  `newThreadDefaults.<kind>.approvalsReviewer`, `user` | `auto_review`, null = built-in). The one-time
  fresh-agent migration does not write it, so existing homes also get `auto_review` for new on-request threads.
- Create routes accept `launch.approvalsReviewer` (`user` | `auto_review`; the legacy alias
  `guardian_subagent` is stored as `auto_review`). Claude profiles refuse it like the other permission fields.
- The session record stores `approvals_reviewer` (absent = the profile config / native default `user`). It is
  sent as `approvalsReviewer` on the bootstrap `thread/start` and on every app-server start / resume, and as
  `-c approvals_reviewer=…` for owner launches. Agents created by agents (`agent_create`) keep None.
- `thread/settings/updated` `approvalsReviewer` is mirrored into the record, so a running change (desktop
  「自动」 on an older thread) survives the next relaunch.
- Likewise a workspace-write `sandboxPolicy` without a named permission profile (what a running
  `thread/settings/update` with a `sandboxPolicy` produces) mirrors `networkAccess` and `writableRoots` (unique,
  without the cwd) into `sandbox_network_access` / `sandbox_writable_roots`, which every start / resume
  (`apply_workspace_extras`) and stock launch (`[permissions.cutex-workspace]`) rebuild from. Absent keys keep the
  stored values; other sandbox types and named permission profiles leave them alone.
- Reviewer model on providers a profile defines itself (`src/profiles/reviewer_catalog.rs`): the native picks the
  reviewer from the active catalog (the parent row's `auto_review_model_override`, else the provider's preferred
  OpenAI review model when the catalog lists it, else the agent's model). Without `model_catalog_json` the native
  uses its bundled catalog, which lists the OpenAI review model; a third-party endpoint answers 404 and every
  escalation is denied. So a launch of an API-key profile without a catalog whose provider is not the native's
  built-in OpenAI provider gets `-c model_catalog_json=<profile dir>/reviewer-models-<sha256>.json`: the same
  native's bundled rows (`debug models --bundled`, metadata unchanged), each with `auto_review_model_override` =
  its own slug, plus a row for the launched model when the bundled list has none (the native's prefix match, else
  cutex's template row with the native fallback's 272k context window). The reviewer then uses the agent's own
  model. Profiles with a catalog and the built-in OpenAI provider are unchanged. Best effort (a CLI that cannot
  list its bundled models leaves the launch as before). Limits: a running model switch to a model that is neither
  bundled nor the launched one falls back to the bundled review model again; the native refuses a mid-turn model
  switch between rows with different reviewer overrides (it applies at the next turn).
- Rollback: an older binary ignores the record field and the `approvals_reviewer` config key, but launch
  receipts written with a reviewer (`StockConfiguration.approvals_reviewer`) are unreadable for binaries with
  `deny_unknown_fields` there; the thread itself keeps its persisted native settings.
