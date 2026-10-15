# New-session defaults and Settings cancellation

Settings has separate first-level Profiles and Defaults categories. Defaults stores the default profile, a skip-picker preference for ordinary sessions, per-profile model/effort overrides, and the initial notification level. Existing installations need no migration; absent fields preserve inherited models/effort and notification Off.

`~/.cutex/config.json` fields:

```json
{
  "default_profile": "example",
  "default_profile_direct_launch": false,
  "new_session_defaults": {
    "example": { "model": "example-model", "reasoning": "high" }
  },
  "new_session_notification": "off"
}
```

Overrides apply to new sessions. Missing overrides follow the selected profile, then the selected native home's shared model/effort settings. Explicit CLI options win. Resume/fork invocation does not apply new-session overrides. Profile names scope overrides; renaming a profile does not move another profile's overrides automatically.

Managed-agent creation always displays profile, model, effort and notification fields, with defaults prefilled. Skipping the ordinary-session picker never bypasses this form. The chosen profile/model/effort are persisted atomically with adoption through optional `creation_defaults` in the Human adoption request. Ordinary adoption omits that field. Existing adoption receipts and idempotency behavior remain valid.

Managed creation initializes notification state when the UUID is known. For ordinary CLI sessions, Cutex exports a creation threshold and initial level to its existing notification helper. Only timestamped native identities created after the launch threshold qualify; the helper persists once under its existing lock and never overwrites an existing preference. No cute-codex source changes are needed.

Esc closes an active editor first. Categorized Settings then unwinds Value -> Options -> Categories before returning to the originating panel. Dirty drafts still use the existing leave review.

Validation: scoped model/effort drafts and discard, profile/shared inheritance, preference persistence and creation cutoff, adoption retries with fixed creation settings, Settings cancellation routes, and terminal UI checks. The full TUI suite retains three previously documented environment/fixture failures (installed-runtime expectation, invalid synthetic native ID, and fixture owner collision).

### Settings presentation (r42)

Editable and actionable setting labels are white; read-only labels and values are
light gray. Selection changes the background without replacing semantic foreground
colors. Notification priority previews and the default notification choice list use
labels, foreground colors and bold settings from `~/.cutex/notifications/config.json`.
No emoji or edit icons are added.

List STATUS uses `N/A` for an unavailable observation and reserves seven columns;
Details retain the underlying reason. Filter titles explicitly reset their text
style, preventing underlying content from contributing colors or bold attributes.

Validation: 285 TUI tests passed, two ignored; the same three existing runtime/fixture
failures remain. The new filter-title regression covers underlying colored/bold text.
