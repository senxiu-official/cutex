# Local runtime deployment

Linux can select a locally installed light runtime in
`~/.cutex/runtime/light/deployment.json`:

```json
{
  "native_home": "/absolute/source/codex-home",
  "bundle_manifest": "/absolute/install/bundle.json",
  "job_mcp": null
}
```

`LocalDeployment::install` verifies the artifact template and writes this selection
privately. The template uses the existing StockBundle fields, with `version: 4`.
It records file hashes for the selected CLI, app-server, companion, facade, schema
and config. Version 4 permits local executable rebuilds; it still checks every
file's bytes and the currently supported generated protocol schema. Initializing
the owner still verifies ExternalInput v1/v2, presentation v1 and Soon delivery.
Old bundle versions retain their historical validation rules.

Normal saved-session Adopt uses this selection for newly managed local records.
It preserves the native UUID and durable identity, copies the one stopped rollout
into `runtime/light/agents/<native UUID>`, writes source provenance, and projects
the current shared config into that home. Source history remains available for
recovery; the managed runtime uses the new copy. Skills, memories and plugins
remain shared links. Credentials continue to come from the selected profile.
An open writable source rollout or recorded runtime owner must be stopped first.
A new-agent helper creates and persists an empty native thread, stops its creator,
and calls the same root adoption API. It does not send a model prompt.

These homes use launch contract version 4. Existing managed records and the 34
migration homes are not converted by default selection. Without an installed
selection the historical Adopt path remains available. A malformed installed
selection produces an actionable error instead of silently choosing a different
backend.

The Management API runtime review should use deployment `job_mcp` when no
explicit descriptor or prior agent descriptor exists, refreshing its live daemon
occurrence. The descriptor's launcher must equal the selected bundle CLI; it no
longer needs a separately compiled CLI hash. Installing native repairs does not
change an already running owner. Existing agents require a human configuration
update to their new per-home manifest, followed by the intended restart.

## Upgrading pinned agents from Management v2

Explicit-launch agents keep their pinned bundle when a new deployment is selected.
The daemon re-pins them on request (root credential only; capability
`runtime-package-upgrade`):

- `GET /v2/runtime-packages` lists the selected bundle and every enabled,
  non-retired, persistent, local, host-backend explicit-launch agent whose pinned
  bundle differs from it, with runtime status and busy state, plus the active and
  last run and `revertable` (`{runId, agentIds}` of the newest upgrade run while
  it has agents left to move back, else null).
- `POST /v2/runtime-packages/runs` with `{"agentIds": [...]}` or `{"all": true}`
  starts a run (202). There is one run at a time, upgrade or revert (409
  `run_in_progress`). A daemon thread processes the agents one by one. Busy
  agents (running turn or non-idle thread) are skipped. An online idle agent goes
  offline (`cutex/runtime/offline`), is reviewed and applied
  (`review_runtime_package_upgrade` / `apply_runtime_package_upgrade`, target
  checked against the bundle selected at run start) and comes back online. An
  offline agent is only re-pinned. An offline agent whose record still carries
  a runtime claim (binding, pid, runtime agent or launch claim of a runtime that
  exited) is reconciled first (step `reconcile`), as a start would: only proven
  process absence clears it, nothing is stopped or killed; a running (possibly
  reused) PID fails the item with code `staleClaim`.
- An agent whose pinned package files were deleted (an old release directory
  removed) but whose manifest, native home and thread still validate is
  upgraded as a **repair** (`repair: true` on the agent row and the run item):
  the review records the missing files, the item is never revertable (there is
  no package to go back to). Any other defect of the current package still
  refuses the upgrade.
- `GET /v2/runtime-packages/runs[?limit=N]` and `GET /v2/runtime-packages/runs/{runId}`
  read run records. They are stored in `~/.cutex/runtime/package-upgrade-runs/`
  (private) and rewritten after every change. A record left `running` by a daemon
  that stopped becomes `interrupted` when it is next read.
- `POST /v2/runtime-packages/runs/{runId}/revert` (`{}` or `{"agentIds": [...]}`)
  puts the moved agents of an upgrade run back on their previous contract.
  Agents an earlier revert of the same run already moved back are left out, so a
  second revert retries only the rest; when nothing is left the answer is 409
  `already_reverted` and no run starts. An agent whose contract changed since the
  upgrade is skipped. The revert runs
  in the daemon (`revert_runtime_package_upgrade`) under the same locks as apply,
  including the cross-process session-store lock. **No service has to be
  stopped**, unlike the one-off r210 repin script, which edited
  `cutex-sessions.json` from outside the daemon. The previous package must still
  load and validate.

Every change is published as `cutex/runtimePackages/runUpdated` on the host event
stream (`cutexSessionId` `cutex.host`) with the run header (`runId`, `kind`,
`status`, `revertOf`, `startedAt`, `finishedAt`, `currentAgentId`, `counts`) and
only the changed `item`. Clients read the full run with
`GET /v2/runtime-packages/runs/{runId}` on start, finish or a gap. The newest 20
records are kept, plus older upgrade runs that can still be reverted (moved
agents not yet moved back) for up to 90 days.
