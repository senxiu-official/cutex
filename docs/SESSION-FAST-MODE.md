# Fast mode per session (`session-fast-mode`)

Codex `/fast` for Cutex agents. Capability `session-fast-mode` (`features.sessionFastMode`).

## What the native does (cute-codex r52.x)

- A thread has a selected **service tier**: `priority` (Fast; the catalog tier named "Fast"),
  `flex`, or the explicit standard sentinel `default`. The legacy request value `fast` is
  normalized to `priority`; a `null` update is stored as `default`.
- `model/list` items carry `serviceTiers: [{id, name, description}]` (deprecated
  `additionalSpeedTiers: ["fast"]`) and `defaultServiceTier`. A model without the tier ignores it:
  the request goes out at standard speed (`ModelInfo::service_tier_for_request`). Bedrock never
  sends a tier. Feature `fast_mode` (stable, on by default) gates `priority` / `default`.
- The TUI `/fast` (a slash command per catalog tier, plus a keybinding) flips the tier between the
  model's Fast tier and `default`, sends it with `OverrideTurnContext` and persists it to
  `config.toml` `service_tier`. The catalog `defaultServiceTier` is applied only by the TUI, never
  by the app-server.
- App-server: `thread/start`, `thread/resume`, `thread/fork`, `turn/start` (this turn and later
  ones), `thread/settings/update` accept `serviceTier` (`null` = clear = `default`);
  `turn/start.serviceTierForTurn` overrides one turn only. `ThreadSettings.serviceTier` /
  `thread/settings/updated` report the selected tier.
- **Resume does not restore it**: model and effort come back from the thread metadata, the tier
  comes from the profile config (`service_tier`) or the resume request.

## What Cutex does

- The session record keeps `service_tier` (absent = the profile config decides).
- `thread/settings/updated` mirrors `threadSettings.serviceTier` into it (string = set, `null` =
  cleared, key absent = unchanged), exactly like model / effort / permissions.
- Every Codex thread start / resume (managed and explicit-launch agents) sends the stored tier as
  `serviceTier`, so Fast survives runtime restarts, upgrades and `cutex/runtime/online`.
- `GET /v2/sessions/{id}` `runtimeDefaults.serviceTier` appears only when the session has its own
  tier (the frozen v2 field set is unchanged otherwise).
- `cutex/session/defaults/update {patch: {serviceTier}}` sets it for the next start / resume:
  `priority` | `default` | `flex` | `fast` (stored as `priority`) | `null`. Claude sessions
  refuse it (`invalid_request`). It does not touch a running thread.

Clients toggle a running thread with native `thread/settings/update {threadId, serviceTier}` —
the model's Fast tier id to turn it on, `default` to turn it off (not `null`, which would fall
back to a profile `service_tier`). Offer the toggle only when the host has `session-fast-mode`
and the current model's `serviceTiers` contain the Fast tier. Profiles whose `config.toml` sets
`service_tier` keep working: a session without its own tier follows them.

## Rollback

Older binaries ignore the record field (session records are not `deny_unknown_fields`) and drop
it on their next save of that record; such a session then follows the profile config again after
its next resume. No migration, no store format change.

## Claude

Not offered. Claude Code has a fast mode (Opus 4.8 / 5 / 5.5, first-party only, billed from usage
credits; a mid-conversation enable re-bills the context once). Headless it needs
`--settings {"fastMode": true}` at spawn and the `apply_flag_settings` control request to toggle,
which the Python Agent SDK does not wrap (private `_query._send_control_request`). The Claude
adapter reports `serviceTiers: []`, so clients show no toggle for Claude agents.
