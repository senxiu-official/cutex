# Notification levels

Capability `notification-levels` (Management v2). Design and owner decisions: cutex-desktop
`docs/research/notification-levels.md`. Code: `src/notification_levels/` (registry, presets, evaluator),
`src/config/notification_levels.rs` (settings), `src/management/v2/notification_levels.rs` (routes).

The owner configures two tables, each a preset plus changed cells:

1. **event → level**: every notification event has one of four levels,
   `urgent` > `needsYou` > `inform` > `record`;
2. **level → methods**: every level has its methods: `needsYou` (counts in 需要你: needs strip, sidebar,
   tray / taskbar badge, summary `needs[]`), `mark` (sidebar done / failed mark), `desktop` (OS
   notification), `phone` (summary `messageToOwner` + webhook `agent.message_to_owner`), `sound`, `bigAlert`.

One occurrence can match several events (e.g. an `info` owner_notify message that needs a reply matches
`ownerNotify.info` and `ownerNotify.needsReply`): the highest level wins (ties: the first listed).

**Both tables default to the `default` ("Recommended") preset** (capability `notification-presets-no-legacy`,
owner decision 2026-10-15). The former `legacy` preset of both tables ("Old behaviour", which reproduced
cutex r216 and derived the methods from `ownerNotify.delivery` / `bigAlertSound`) was removed: it is no longer
offered, a `PATCH` naming it is refused as an unknown preset, and a stored `legacy` (or no preset) reads as
`default`. See [Migration from legacy](#migration-from-legacy-2026-10-15).

## Registry (data)

`src/notification_levels/registry.json` (embedded, LF): `levels`, `methods`, `fixedCells`, `eventPresets`,
`methodPresets`, `events`. Titles / descriptions are `{"en", "zh-CN"}`.

Fixed cells (never stored, ignored in a PATCH): `urgent.needsYou = true`, `needsYou.needsYou = true`,
`record.needsYou = false`.

### Event presets

| id | meaning |
|---|---|
| `default` | "Recommended": notification-levels.md §2.1 (only the owner's own work can need the owner) |

### Method presets

✓ = on. Columns: needsYou / mark / desktop / phone / sound / bigAlert.

| level | quiet | default | all |
|---|---|---|---|
| urgent | ✓ ✓ ✓ ✓ ✓ — | ✓ ✓ ✓ ✓ ✓ ✓ | ✓ ✓ ✓ ✓ ✓ ✓ |
| needsYou | ✓ ✓ — — — — | ✓ ✓ ✓ ✓ — — | ✓ ✓ ✓ ✓ ✓ — |
| inform | — ✓ — — — — | — ✓ ✓ — — — | ✓ ✓ ✓ ✓ — — |
| record | — — — — — — | — — — — — — | — ✓ — — — — |

`ownerNotify.delivery` / `bigAlertSound` are still stored and accepted by `PATCH /v2/settings` but no longer
drive any notification (they only fed the removed legacy methods preset).
`effective.legacyNeedsYouRequiresChannel` is always `false` (kept for older clients).

### Events

`appliedBy`: `cutex` = cutex applies the decision today; `client` = desktop / phone apply it with the resolved
tables (after the desktop freeze). `requires`: what cutex still lacks to decide it itself.

| event | applied by | default |
|---|---|---|
| `ownerNotify.info` | cutex | record |
| `ownerNotify.attention` (also urgent over the urgent quota) | cutex | inform |
| `ownerNotify.urgent` | cutex | urgent |
| `ownerNotify.needsReply` (combined with the level event) | cutex | needsYou |
| `ownerNotify.rateLimited` (replaces the level events) | cutex | record |
| `system.quarantine` / `system.deliveryFailed` / `system.idleDelegation` / `system.other` | cutex | record |
| `native.approval` (command / file / permissions) | cutex + client | needsYou |
| `native.question` (requestUserInput / elicitation) | cutex + client | needsYou |
| `ownerApproval.pending` | cutex + client | needsYou |
| `delegation.ownerQuestion` | cutex + client | needsYou |
| `delegation.pendingReview` | cutex + client | needsYou |
| `delegation.outcomeUnknown.owner` / `.agent` | cutex + client | needsYou / record |
| `delegation.awaitingAcceptance.owner` / `.agent` | cutex + client | needsYou / record |
| `delegation.failed.owner` / `.agent` (need kind `delegationFailed`) | cutex + client | needsYou / record |
| `delegation.deliveryFailed` (host event), `agentBus.messageFailed` | client | record |
| `turn.owner.completed` / `.failed` / `.interrupted` | client (requires `turn-triggers`) | inform / needsYou / record |
| `turn.other.completed` / `.failed` | client (requires `turn-triggers`) | record / record |
| `runtime.error.ownerTurn` (requires `turn-triggers`) / `runtime.error` | client | urgent / inform |
| `job.completed.owner` (requires `job-submitter`) / `job.completed.agent` | client | inform / record |

## Where cutex evaluates

- **owner_notify** (`owner_notify::store::create_decided`, Agent Bus process): rate limits first (unchanged),
  then the events above → `Decision` → record fields `channels` (projection of desktop / phone / bigAlert),
  `notificationLevel`, `notificationEvent`, `actions`, and for cutex notices `systemKind`. The agent-facing tool
  result is unchanged.
- **cutex notices** (`documents::notify_owner(kind, …)`): `system.<kind>`. Record in every built-in preset
  (same as r215 / r216); an owner who raises one gets channels / a need like any other event (still not
  replyable).
- **record predicates**: `pending()` = agent needs-reply message open / acknowledged, else open and `actions`
  contains `needsYou`; `alerting()` = open agent message, or open cutex notice with a channel; `to_phone()` =
  channel phone (old cutex notices without `actions` never). Records without `actions` (written before this
  capability) keep the r216 rules.
- **summary** (`/v2/summary`, management process, every tick): native requests, owner approvals, delegations
  and owner_notify records → `needs[]` only when the decision includes `needsYou`; each need gains `level`;
  `asksChanged.count` uses the same rule. `messageToOwner` follows `to_phone()`.
- **webhook** (`cutex notify run`): `agent.message_to_owner` follows the record's channels as before; turn /
  attention / job webhook events keep the per-session level (`notify/session.rs`) for now. The levels
  `task.reviewReady` / `task.closed` / `system.preActivationSkipped` and the webhook events
  `task.review_ready` / `task.closed` were removed in r219 (see [r219-removals.md](r219-removals.md));
  saved owner settings and retired names in `~/.cutex/notifications/outbound.json` stay loadable (the
  retired webhook names are dropped on read).

Overrides: `ownerNotify.agentOverrides` (`mute` / `force`) and `notificationLevels.projectOverrides`
(`<projectId>` or `__independent__` → `mute`). A mute removes desktop / phone / sound / big alert from every
event of that agent / project, keeps `needsYou` / `mark`, and lowers `inform` to `record`. `force` only skips the owner_notify rate limits.

## Routes (root only; also client / device tokens mapped to root)

- `GET /v2/notification-levels` →

  ```jsonc
  {
    "contractVersion": 1, "registryVersion": 2,      // 2: legacy presets removed
    "levels": [{"id", "title", "description"}], "methods": [...], "fixedCells": {"urgent": {"needsYou": true}, …},
    "eventPresets": [{"id", "title", "description", "levels": {"<eventId>": "<level>"}}],
    "methodPresets": [{"id", "title", "description", "methods": {"<level>": {"<method>": bool}}}],
    "events": [{"id", "module", "title", "description", "levels": {"default"}, "appliedBy": [], "requires": [],
                "level": "<effective>", "overridden": bool}],
    "settings": {"eventPreset", "eventLevels": {}, "methodPreset", "methods": {}, "projectOverrides": {}},
    "effective": {"eventPreset": "<id>|custom", "methodPreset": "<id>|custom", "eventLevels": {}, "methods": {},
                  "legacyNeedsYouRequiresChannel": false},
    "unregisteredEventLevels": {}          // stored levels of events no module registered (kept)
  }
  ```

- `PATCH /v2/notification-levels` (same body as `/v2/settings` `notificationLevels`) → the view + `changed`:

  ```jsonc
  {"events":  {"preset": "default", "levels": {"<eventId>": "<level>" | null}},
   "methods": {"preset": "quiet" | "default" | "all", "cells": {"<level>": {"<method>": bool | null}}},
   "projectOverrides": {"<projectId>" | "__independent__": "mute" | null}}
  ```

  A preset clears that table's changed cells (before the cells of the same request) and is stored explicitly
  (also `default`, so an older binary reads the same choice); a cell equal to the preset is not stored; `null` removes a cell; fixed cells are ignored; unknown events / levels / methods / presets →
  400 `invalid_request`. `effective.*Preset` reports `custom` unless the table equals a preset. Applied at once.
- `POST /v2/notification-levels/evaluate {events: [ids], agentId?, projectId?}` (`projectId: null` =
  independent agent) → `{level, event, actions, channels, muted, needsYou}` with the current settings (a test /
  preview helper; nothing is sent).
- `GET /v2/settings` gains `notificationLevels` (`settings` above + `effectiveEventPreset` /
  `effectiveMethodPreset`); `PATCH /v2/settings {notificationLevels: …}` is the same update.

Additive fields elsewhere: owner-notify records `notificationLevel`, `notificationEvent`, `actions`,
`systemKind`; summary `needs[].level`; need kind `delegationFailed`.

Persistence: `config.json` `notification_levels` (`eventPreset`, `eventLevels`, `methodPreset`, `methods`,
`projectOverrides`; omitted while nothing was ever stored). Invalid hand edits fall back cell by cell; an
unknown or `legacy` preset reads as `default`.

## Contributing an event (modules / plugins)

An event is data:

```json
{"id": "waveline.reminderDue", "module": "waveline", "appliedBy": ["client"],
 "title": {"en": "Reminder due", "zh-CN": "提醒到期"}, "description": {"en": "…"},
 "levels": {"default": "inform"}}
```

- `id`: dotted ascii (`module.event[.variant]`), unique. `levels.default` is required; other keys must be event
  preset ids. A `legacy` key (a contribution written for an older cutex) is dropped on registration.
- In code: `cutex::notification_levels::register_event(spec)` at process start (management and / or Agent Bus
  process: each has its own registry), then `Evaluator::current().decide(&["waveline.reminderDue"], scope)`.
- As data: drop `*.json` (one event, or `{"events": [...]}`) into `<cutex config dir>/notification-events/`;
  both processes load them on first use. Bad files / duplicates are skipped and logged.
- Owner settings for an event id that is not registered (module removed or not loaded yet) are kept and apply
  when it registers.

## Migration from legacy (2026-10-15)

One-time, at the start of `cutex management serve` (`config::notification_levels::run_legacy_migration_at`),
marked done by `~/.cutex/archive/notification-presets-no-legacy/migration.json`:

- in `config.json` `notification_levels`, each of `eventPreset` / `methodPreset` that is absent or `legacy` is
  set to `default` explicitly (the raw JSON is edited, unknown fields are kept). The owner's changed cells
  (`eventLevels`, `methods`) and `projectOverrides` are kept and now apply on top of `default`;
- the record (schema `cutex/notification-presets-no-legacy-migration/v1`) holds `at`,
  `previousNotificationLevels` (the whole block before, or `null`), `previousOwnerNotify` (the `owner_notify`
  block, i.e. the delivery / sound settings the legacy methods were derived from) and `written` (the presets
  set);
- a home without `config.json` is only recorded (absent = `default`); an unreadable `config.json` is left alone
  and the migration is retried at the next start. A start that finds the record does nothing, so a later
  choice is never overwritten.

To restore the old delivery-driven behaviour by hand, read the record: the old `ownerNotify.delivery` /
`bigAlertSound` map onto method cells (`urgent` / `needsYou` / `inform` rows ← `delivery.urgent` / `.attention`
/ `.info`) under any preset.

## Rollback (older cutex)

- A binary from before this change reads the stored `default` presets as `default` (both are known ids there),
  so a migrated home keeps the Recommended tables after a rollback. Only a home that never had `config.json`
  (absent block) runs the older binary's absent = `legacy` behaviour. Restoring the old presets on purpose:
  `PATCH /v2/notification-levels {"events": {"preset": "legacy"}, "methods": {"preset": "legacy"}}` on the older
  binary.
- After moving forward again, a `legacy` the older binary stored reads as `default` (the migration does not run a
  second time; the record exists).
- Binaries older than `notification-levels` (r216 and before) ignore `config.json` `notification_levels`, drop
  it on their next config save and run their r216 behaviour. `ownerNotify.delivery` / `bigAlertSound` were
  never changed by this feature.
- owner-notify records: the new fields are ignored (and dropped when it rewrites the store). r216 then judges
  every record by its own rule (open with a channel / needs-reply; cutex notices never); records created under
  the `default` tables can differ (e.g. a muted urgent message that still counted, or a cutex notice the owner
  raised: r216 shows it in no need and sends no further push).
- summary: `needs[].level` and kind `delegationFailed` disappear on the next tick of an r216 binary.

## Not done here (follow-ups)

- Turn triggers (`turn-triggers`, design §3): `cutex/turn/ended` with the trigger, `unreadTurns` / `turnEnded`
  by level, `runtime.activeTurnTrigger`. Turn / runtime / job events are registered (`appliedBy: client`,
  `requires`) but cutex does not evaluate them yet.
- Webhook turn / attention events by level (design C5) and retiring the per-session off / normal / important
  level (Q7).
- Desktop (D1–D6) and phone use the tables after the desktop freeze.
