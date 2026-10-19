# Persistence and `deny_unknown_fields`

Rule for backend upgrades and rollbacks (`docs/backend-upgrade.md`): **stored
structures do not use `deny_unknown_fields`; only API / tool input is strict.**
A field a newer cutex writes must never make an older binary refuse a whole
store (several early rollbacks failed exactly there).

## Rules

1. A struct that is read from disk ignores unknown fields.
2. Where the struct is written back by a read-modify-write and the field could
   matter to the newer writer, keep the unknown fields (`#[serde(flatten)] extra:
   serde_json::Map<String, Value>`, skipped when empty). Where a store is
   deliberately cleaned on save (`CodezConfig` drops removed Task Service keys and
   their tokens), unknown fields are dropped on save instead.
3. An API body that reuses a stored struct checks `extra` itself and answers
   `invalid_request` (e.g. `PUT /v2/profiles/{p}/quota-config`).
4. Security-relevant refusals stay explicit lists, not a side effect of
   strictness (selected profile config: credential / command keys).
5. Enum values are not covered yet: an unknown variant still fails the field
   (see "Not changed" below).

## Changed so far

| Store | Struct | Before | Now |
|---|---|---|---|
| `accounts.json` `options` | `ProfileOptions`, `TextStyle`, `ClaudeDefaults`, `quota::Config` | strict: an unknown option made the whole accounts store unreadable | unknown fields kept in `extra` and written back |
| `config.json` `appearance` | `Appearance`, `TextStyle` | strict inside `CodezConfig`: one unknown key = config.json unreadable | kept in `extra` |
| `config.json` `new_thread_defaults` | `NewThreadDefaults`, `ThreadLaunchDefaults` | strict | ignored, dropped on save |
| session records | `LaunchPermissions` | strict | ignored, dropped on save |
| `config.json` `browser` | `BrowserSettings` | strict | ignored, dropped on save (API input validated by `patched`) |
| `runtime/users/persons-v1.json` | `persons::State` | unknown fields ignored, dropped on rewrite | unknown top-level fields kept in `extra` (with `ownerPerson`) |
| profile `config.toml` (selected profile v2) | `selected_profile::Config` root, `model_providers.<id>` | strict: r233 `web_search` / r53 provider `capabilities` made older binaries refuse the profile | unknown keys ignored (never projected into a launch, listed in `Config::ignored_keys`); `api_key`, `notify`, `experimental_bearer_token`, `http_headers`, `env_http_headers`, `chatgpt_base_url` (root) and `http_headers`, `env_http_headers`, `experimental_bearer_token`, `api_key`, `query_params` (provider) stay refused; nested tables (`tools`, `tui`, `skills`, …) stay strict |

## Still strict and stored (follow-up list, by risk)

High (a whole store / launch path fails):

- `launch/local_deployment.rs` `LocalDeployment` (`runtime/light/deployment.json`)
  and `launch/job_mcp.rs` `JobMcpDescriptor` inside it.
- `runtime/claude.rs` `ClaudeDeployment` (`runtime/claude/deployment.json`),
  `ClaudeSession`, `ClaudeProfileRuntime`.
- `launch/version_registry.rs` `NativeVersionRegistry` (display only; a failure
  degrades to "unknown version").
- `session/model.rs` `FormalAgentNameReceipt`.
- `agent_management/model.rs`, `projects.rs`, `sparse.rs`, `archive.rs`,
  `durable_import.rs`, `runtime_package.rs`, `explicit_launch.rs` receipts /
  records / contracts (`ExplicitLaunchContract`, `ManagedAgentRecord`,
  `ProjectStateRecord`, …) in the Agent Management store.
- `management/v2/usage/prices.rs` `UsagePriceTable`.
- `launch/selected_profile.rs` `Projection` and `selected_status.rs` (stored
  projections; new optional fields are written only when set, which is the
  current mitigation).

Medium / immutable per release (written once by release tooling):
`launch/stock.rs` `StockBundle`, `VerifiedFile` (bundle manifests),
`profiles/catalog_overlay.rs` `Overlay`, `notes/mod.rs` `Note*`,
`browser_relay/shot_store.rs`, `management/connections.rs` `Hosts`,
`config/instance.rs` `InstanceSettings`, `notify/*` configs,
`inbound_mail` / `nci_verification` `Config`, `runtime/claude_pin.rs`,
`app_server/external_input.rs` receipts.

Strict on purpose (API / tool input; keep): every `*Request`, `*Args`, `Body`,
`Patch`, `Input` struct in `management/v2`, `asks/tools.rs`, `workflows/tools.rs`,
`agent_bus/*` tool arguments, `browser_relay` wire types, `LaunchOverrides`,
`QuotaConfigUpdate`, `health::FixRequest`.

## Not changed

- Enums: an unknown variant (`quota_preset`, `provider`, `cli_kind`, …) still
  fails the containing field. Mitigation used so far: write the nearest legacy
  value plus a separate additive field (`provider_category`). A general
  `#[serde(other)]` fallback would lose the value on write-back, so it needs a
  per-field decision.
- `CodezConfig` top level already ignores unknown fields (and drops them on save
  on purpose); `StoredAccount` top level ignores and drops them.
