# Human administration

`cutex human` is the entry for a user or an agent acting at the user's request.
It uses the existing local Management credential automatically. It does not
require a Director role, another login, or proof of a physical human. The name
communicates intent; processes running as the same OS user are not isolated from
this credential. Ordinary commands retain their existing grouping.

Common commands:

```
cutex human new NAME --cwd /path/to/project
cutex human start NAME
cutex human attach NAME
cutex human stop NAME
cutex human restart NAME
cutex human stop NAME --force
cutex human recover NAME
cutex human action ACTION_ID
cutex human action ACTION_ID --resume
cutex human config show NAME
cutex human config set NAME model=MODEL reasoning=high cwd=/path
cutex human config set NAME profile=null
cutex human config undo NAME ORIGINAL_ACTION_ID
```

Names may be a unique formal/display name, native UUID, or exact durable ID.
`stop` interrupts the runtime while retaining history. `--force`
uses the existing forced stop path when graceful interruption is unavailable.
`cutex human tasks ...` and `--cancel-tasks` were removed in r219 with the
legacy Task Service (see [r219-removals.md](../r219-removals.md)).

Configuration changes affect the next launch. The current runtime keeps its own
launch configuration and package identity for Attach/Stop. Changes return an
action ID and before/after values. Undo restores only those fields and refuses
to overwrite newer changes to the same fields; a new explicit Set remains
available. Bundle selection accepts a manifest path and computes its hash.

The TUI exposes Start/Attach/Restart and interrupted-start recovery. Alt+N in
Agents or Recent opens the New Agent form. Ordinary saved-session Adopt uses
the selected local runtime and preserves the saved native ID. New agent creation
persists an empty native thread; starting a model turn remains a separate action.

Local runtime installation:

```
cutex human install-runtime /path/to/template.json --source-home /path/to/native-home --job-descriptor /path/to/job.json
cutex human config set NAME bundle_manifest=/path/to/agent-specific-bundle.json
```

Installing selects the default for future New/Adopt. Updating an existing agent
uses a per-home manifest; it does not copy its history or restart it. Prior
packages remain available for undo and running owners.

`CUTEX_MANAGEMENT_URL=http://host:port` selects an already running Management
endpoint for these commands without starting another service. Credentials still
come from the caller's configured home. This also supports isolated local repair
and testing. Do not submit a second start merely because its HTTP response was
lost: the client queries the original action and reports its action ID if the
result remains unknown. After fixing a missing dependency, `human action ID --resume`
replays that same action and reuses any published process. A receipt short of Ready
returns a nonzero CLI exit status.

## Stuck Agent Bus messages

```
cutex human bus-messages list [--min-age 1h] [--agent ID] [--limit N] [--json]
cutex human bus-messages discard MESSAGE_ID --reason TEXT [--expected-sha256 HEX] [--allow-protected]
```

`list` shows undelivered messages (durable `pending` / `stale_held` records and
un-ACKed entries of the bus's live runtime queues) with age, reasons and live
placement. `discard` makes one terminal (`owner_message_discarded`; never
redriven), withdraws it from the live queues and writes an audit fact; it is
idempotent. Both use Management `/v2/agent-bus/stuck-messages`
(capability `agent-bus-stuck-messages`); see `docs/agent-bus-operations.md`.

## Ordinary lifecycle commands

Managed native agents use one runtime owner across the TUI, ordinary session
commands, and Human administration:

```
cutex session online NATIVE_OR_DURABLE_ID
cutex session foreground NATIVE_OR_DURABLE_ID
cutex session takeover NATIVE_OR_DURABLE_ID
cutex session offline NATIVE_OR_DURABLE_ID
```

`online` starts the saved agent in the background or returns the matching Ready
receipt for its existing live owner. `foreground` starts it if necessary and
attaches the native TUI; leaving that TUI keeps the owner running. `takeover`
attaches an already running native owner. The older Start wizard offers Start
and Attach for these records too. Online and foreground use the same Management
runtime action API as Human Start; neither launches a second local native core.
A pending start directs the caller to its original action, so repeated ordinary
commands cannot replace an unresolved owner.

The saved runtime configuration remains authoritative. A one-launch `--profile`
override, or a different foreground cwd, is not silently persisted or applied to
an existing owner. Set the saved profile with `cutex session profile set ID PROFILE`
or edit cwd with `cutex human config set ID cwd=/path`, then launch/restart as
intended. Existing non-native runtime records retain their previous lifecycle.

## Raw and quick launch compatibility

Raw `cute-codex`, `cutex --quick`, and profile/direct argument passthrough remain
legacy launch routes. They select the existing executable through
`CUTEX_CODEX_BIN` or PATH and pass profile/auth/custom-status file settings through
legacy environment variables such as `CODEX_CONFIG_FILE` and `CODEX_AUTH_FILE`.
The installed light runtime uses explicit configuration and credential projection
instead. Repointing the raw executable alone would bypass that projection and can
silently select the wrong account or settings. No raw symlink is changed by local
runtime installation; managed New/Adopt/Start/Attach use the selected deployment
manifest directly. Use those managed entries for the unified runtime behavior.

The v2 `cutex/runtime/online` method also routes native managed Agents through
these same runtime receipts when called with the existing Human Management
bearer. It retains the expected-runtime-generation check and lifecycle events.
The response includes `actionId` and `attachCommand`; an HTTP caller can start the
owner, then run the returned command in a terminal. `openVisibleTerminal` cannot
transfer a caller's terminal through HTTP, so this path emits the existing
foreground-required event when terminal access is requested.

An Agent Bus bearer is not a Human Management bearer. It does not authorize native lifecycle operations. If an ordinary bridge
credential reaches the legacy mutation handler, `human_runtime_route_required`
provides the exact Human endpoint and CLI command; otherwise authentication
rejects the request before dispatch. A human may delegate those commands to an agent using
the already configured Human credential. No additional approval token is needed.

Visibility controls list exposure, not Human ownership: an authenticated Human
may address an active local durable Agent by exact ID even while it is hidden.
The normal method validation, generation checks, and archive lifecycle rules
still apply. Other callers retain the existing visibility rules.
