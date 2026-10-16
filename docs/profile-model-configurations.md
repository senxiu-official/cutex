# Profile model configurations

Management feature `profile-model-variants` (`features.profileModelVariants`)
adds named configurations of one real Codex model. A profile still has one
credential; configuration IDs select catalog rows within that profile.

`GET /v2/profiles/{profile}/models` adds:

- `modelVariantsSupported`: the selected verified native server advertises
  model variant support. Old runtimes return false; official catalogs stay
  read-only there.
- `discoverySupported`: `/models` discovery is available only for custom
  API-key profiles, independently of whether a catalog is editable.
- Model `requestModel`: the real upstream model ID for a configuration, or
  null for a base model. The existing `id` selects a local catalog row.

For example, PATCH `.../models/Astra%20%5B400K%5D` with
`{"requestModel":"gpt-6-astra","displayName":"Astra [400K]","contextWindow":400000}`
creates a configuration by copying the complete base model metadata, then
applying the patch. `Astra [285K]` can coexist with it and the original model.
The native runtime sends `gpt-6-astra` to Responses, WebSockets and compaction;
the local selection, history and context accounting keep the configuration ID.

`requestModel` is immutable for an existing ID. It must name an existing base
row, never another configuration or itself. For official profiles it must also
exist in the native catalog. PATCH without `requestModel` still edits a base
model; official profiles cannot add arbitrary unknown base IDs. Reset restores
base metadata while preserving a configuration's ID, request model and display
name. A base model cannot be deleted while configurations reference it.

## Official catalogs are an overlay (`profile-catalog-overlay`)

Official profiles (ChatGPT login, `source: official`) follow the live upstream
list: the native cache (`models_cache.json`) or the selected CLI's bundled list,
as for a profile without edits. Edits do not freeze a copy. They are stored as
this profile's differences in `<profile dir>/model-overlay.json`
(`src/profiles/catalog_overlay.rs`):

```json
{"version": 1, "migratedFrom": "…/models-<uuid>.json",
 "entries": [
   {"id": "gpt-6-astra", "overrides": {"auto_compact_token_limit": 700000}},
   {"id": "Astra [400K]", "requestModel": "gpt-6-astra", "overrides": {"context_window": 400000}},
   {"id": "gpt-5.6-sol", "hidden": true}]}
```

- `overrides` hold only the editable `ModelInfo` keys (display_name, context
  and max context window, auto-compaction, effective context percent, input
  modalities (vision), reasoning levels and default); `null` removes the key.
  All other fields (instructions, tools, truncation, ...) follow upstream.
- A named configuration is the live upstream base row plus its own overrides,
  so base metadata updates reach it.
- `hidden`: an upstream entry this profile removed. Editing it shows it again.
- Upstream = the complete native list, rows the native hides from pickers
  (`visibility: hide`) included. Visibility is a display property: a
  configuration on a hidden base resolves and launches; `GET .../models` only
  leaves hidden rows out of `models` (except the selected model), like the
  native picker. Only a base that is really gone makes a configuration
  unavailable.
- Effective list = upstream rows in upstream order with overrides applied and
  hidden entries left out, then the named configurations in overlay order. A
  model upstream adds appears without an edit.
- An edit patches the stored entry it targets and nothing else. Keys the PATCH
  sets are stored even while they equal upstream (a pinned value stays pinned
  when upstream later moves away again); other entries are never re-derived
  against the current upstream. Only reset drops an entry's overrides.
- An entry whose model (or a configuration's base model) left upstream stays in
  the overlay but is left out of the effective list and is reported in
  `GET .../models` `unavailableModels` (`reason: modelMissing |
  baseModelMissing`). Nothing is lost: when the model returns, so does the
  entry. `DELETE .../models/{id}` removes it; a PATCH of it is refused. A
  profile or agent that selects such a configuration fails closed at launch
  (the model is not in the catalog) until it selects another model.
- Every named configuration keeps its ID reserved, available or not: an
  upstream model with the same slug stays out of the effective list until the
  configuration is deleted, so a saved selection never silently switches to a
  different model. `requestModel` is checked against the stored overlay:
  reusing an ID for another base needs a DELETE first.
- Removing every difference clears the profile's catalog pointer: the profile
  is back on the plain native list. `model-overlay.json` stays with
  `"entries": []` as the "migrated" marker, so an emptied overlay is never
  mistaken for a legacy snapshot.
- Commit order: the overlay is written first, then the pointer. A crash in
  between leaves a pointer that the next edit or launch refresh re-derives
  from the overlay. A per-profile exclusive file lock
  (`<profile dir>/model-overlay.lock`, cross-process: a standalone CLI launch
  next to the daemon waits for it) covers migration, reads, edits and pointer
  updates.
- API-key custom profiles are unchanged: their discovered catalog is their own
  (`models-<uuid>.json` per edit).

Launches stay deterministic. The native reads one catalog file, so cutex
writes the effective list to a content-addressed `models-<sha256>.json` (never
rewritten) and points `model_catalog_json` at it, after each edit and before
each launch review (`refresh_for_launch`, best effort: on failure the last
selected file is used). The launch receipt digests that file exactly as
before, so usage attribution and pricing bindings verify against it. A running
runtime keeps its startup catalog: model selection among its loaded
configurations applies to subsequent turns, while new or edited rows (and new
upstream models) require a new runtime.

Duplicating a profile copies its overlay and its current effective file into
the duplicate's own directory; deleting the source profile does not break the
duplicate's configurations. Credentials are still never copied.

One-time migration: an official profile that still points at a pre-overlay
snapshot (`models-<uuid>.json`, written by its first catalog edit) and has no
overlay is converted on first use (models view, edit or launch): overlay =
the snapshot's editable differences against the current upstream. Upstream
models the snapshot lacks are not hidden (they may be new), so a model removed
before the migration reappears. The previous `config.toml` and the snapshot are
copied to `<profile dir>/pre-overlay-<UTC timestamp>/`; the snapshot itself is
left in place and named by `migratedFrom`. Rollback: restore that
`config.toml` (pointer back to the snapshot) and delete `model-overlay.json`.
While upstream is unavailable the snapshot stays selected and the conversion
is retried later. Releases before this feature never wrote snapshots for
official profiles, so their homes have nothing to convert.

Store change after the 2026-10-11 review (R5–R9): an overlay emptied by the
last delete is now kept as `{"version": 1, "entries": []}` instead of being
deleted. A profile that an earlier overlay release left pointing at a
content-addressed `models-<sha256>.json` without an overlay (its last entry
was deleted and the process stopped before the pointer was cleared) is
converted once on first use: the empty marker is written and the pointer
cleared, never re-migrated from that file (log line
`cutex-catalog-overlay: profile … marked as emptied, pointer cleared`). The
release before this change reads the empty marker as "no edits" as well
(an empty `entries` array is valid there), so no backup is needed.

## Limits, runtime capability and usage

Profile `modelContextWindow` and `modelAutoCompactTokenLimit` overrides retain
their precedence over all configurations. Clear these overrides to use each
configuration's distinct values. Effective limits and their sources remain
visible in the models resource; auto-compaction is capped at 90% of the
resolved context window.

Capability probing is local and does not start a session or contact a provider.
Split bundles probe their actual `codex-app-server --model-variants-capability`;
monolithic runtimes use `debug models --capabilities`. Both return
`{"modelVariants":true}`. The executable is digest-verified, and results are
cached per executable (expected digest, probe arguments, path, size and
modification time): while those are unchanged the ~260 MB server is neither
re-hashed nor re-run; a changed file is verified again before it is probed.
Every launch checks its actual bundle, including
explicitly pinned older runtimes; a new frontend cannot enable variants on an
old app-server. Catalogs requiring this capability fail closed after downgrade.

Usage samples keep the local configuration ID as `model` for display and
grouping, and persist optional `requestModel` for pricing. The runtime captures
bindings from its matching launch receipt's digest-verified catalog; initial
selection comes from the actual native thread/start or thread/resume response,
including reconnects of the same runtime generation; later observed
thread/settings/updated selections take precedence. No current profile
lookup is used to reinterpret historical samples. Missing launch evidence leaves
the upstream binding unknown; changing runtime generation clears stale bindings.
Server model reroutes use the actual rerouted model instead of the original
configuration's binding. Native usage attribution with captured runtime evidence
freezes at turn/started: changing settings while it runs affects the next turn. Only one turn's
attribution is retained, including across a reconnect of the same generation.
Legacy emitters and Claude adapters keep their existing settings-based attribution.

An exact manual price for the configuration ID (including explicit price-table
aliases) wins; otherwise manual/library/API-equivalent/Codex-credit estimates
use the persisted request model, including its service-tier rules. Editing or
deleting the profile or catalog does not alter this binding in saved usage.
Older samples without `requestModel` retain their previous pricing behavior;
no historical mapping is guessed from a configuration name.

## Per-entry display names (`profile-model-display-names`)

Every model entry of a profile (a base model, a named configuration, a native
or Claude alias) can carry the profile's own display name. It is stored per
profile in the accounts store, top-level `model_display_names`
(`{modelId: name}`, omitted when empty), not in the model catalog: renaming
must work for profiles whose catalog is read-only (Claude aliases, official
profiles on a native without `modelVariants`) and must not turn an official
list into an edited catalog just to rename an entry. It is display only: the id
stays what is selected, launched, sent to the provider and recorded in usage.

- `GET /v2/profiles` and `GET /v2/profiles/{p}`: `modelDisplayNames`.
- `PATCH /v2/profiles/{p}` `{"modelDisplayNames": {"<id>": "<name>" | null}}`
  merges: a string sets (whitespace collapsed, 1-80 characters, no control
  or bidi characters), `null` / `""` / a name equal to the id removes. At
  most 256 names per profile. Applies immediately (no runtime involved).
- `GET /v2/profiles/{p}/models`: `models[].entryDisplayName` (null = none);
  `displayName` remains the catalog / native name.

Resolution in clients: the agent's profile entry name, then the catalog
display name, then the raw id. Duplicating a profile copies the names.
Older binaries ignore the key and drop it when they save the store.

Per-entry context: an entry's `contextWindow` / `autoCompactTokenLimit` live
in its catalog row (`PATCH .../models/{id}`), for single entries the same as
for named configurations. A profile-level `modelContextWindow` /
`modelAutoCompactTokenLimit` still takes precedence over every entry.

## Claude profile default model (`claude-profile-default-model`)

A Claude profile's default model and effort live in its store options
(`accounts.json` `options.claude.model` / `effort`; built-in default
`sonnet`, effort unset = the model's own default). `PATCH /v2/profiles/{p}`
sets them with the same fields as a Codex profile's config:

- `model`: a Claude model id or alias as the adapter takes it (1–200
  characters, no whitespace / control characters; e.g. `claude-opus-5-5`,
  `opus`, `claude-opus-5-5[1m]`). `null` restores `sonnet`.
- `modelReasoningEffort`: `low` | `medium` | `high` | `xhigh` | `max`;
  `null` (or `default`) = the model's own default.

Any other Codex config field on a Claude profile is still refused (400
`Codex model settings are unavailable for a Claude profile`). Before this
capability `model` was refused too.

**Precedence for new sessions** (owner-created agents, quick chats,
`cutex` Claude launches, profile switches to a Claude profile, the models
list's `isDefault` / `selectedModel`): an explicit request model, then
`config.json` `new_session_defaults[<profile name>].model` (`/v2/settings`
`newSessionDefaults`, the more specific per-profile override), then the
profile's own default. The Claude profile item `model` shows that
effective value; `detail.claudeProfileModel` is the profile's own,
`detail.claudeNewSessionModel` the override (null when unset),
`detail.claudeBuiltinDefaultModel` = `sonnet`, and
`detail.editable.claudeDefaultModel` is true on Claude profiles.

A `PATCH` that sets or clears `model` on a Claude profile also drops that
profile's `newSessionDefaults` model when it differs (its `reasoning` stays;
an entry left empty goes), so the value written is what new sessions get.
Existing sessions keep their own model (a Claude session copies it at
creation); pinned older adapters (claude-r5 …) pass the string to the SDK
unchanged, ids and aliases alike.
