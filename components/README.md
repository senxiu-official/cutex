# Optional components

An optional component is a backend piece that cutex installs, updates and removes on demand, on
managed installs (`cutex service install`) only. Since cutex 0.1.1. Current components:

| Name | What it is | Platforms |
|---|---|---|
| `claude-runtime` | Claude agent runtime; needs Python 3.11+ and your own Claude Code CLI and login ([README](claude-runtime/README.md)) | linux-x64, windows-x64, darwin-arm64 |
| `job-service` | Job Service: long-running agent jobs, the desktop's Jobs page ([README](job-service/README.md)) | linux-x64 |

The user-facing install steps are in the install guide
([AGENT-INSTALL.md](https://github.com/senxiu-official/cutex-desktop/blob/release/docs/install/AGENT-INSTALL.md)
§6.1 and §6.2).

```sh
cutex component list
cutex component status <name>
cutex component install <name> [--from-source <checkout>] [--dry-run]
cutex component remove <name> [--force] [--dry-run]
cutex component prune <name> [--dry-run]
```

The desktop shows the same state and offers an **Install** button where a feature needs a
component. Management API (owner token only, capability `components`):

- `GET /v2/components` → `{managed, installFrom: "release"|"source-only"|"none", components, operation}`.
  Each component: `name`, `supported`, `platforms`, `state` (`missing`, `installing`, `ready`,
  `degraded`, `needsUser`, `external`, `restartPending`, `notInManifest`), `selected`,
  `available`, `origin` (`signed`|`source`), `versions` (`version`, `pinnedBy`, `sizeBytes`),
  `detail`, `error: string|null`, and `steps: string[]` (short things the user must do) when the
  state is `needsUser`.
- `POST /v2/components/<name>/install` and `…/remove`: empty body or `{}` (any field → 400
  `invalid_request`). 202 `{"operation": {"id": "cop-…", "name", "action": "install"|"remove",
  "state": "running"|"done"|"failed", "step": "download"|"unpack"|"prepare"|"select"|"stop"|null,
  "error": string|null}}`. Refusals use the normal error envelope: `unmanaged_install`,
  `component_unsupported_platform`, `component_external`, `component_not_in_release`,
  `component_source_only`, `upgrade_blocked` (`reason: run_in_progress`), `component_pinned`,
  `jobs_running`. Source installs and forced removal are CLI-only.
- Event `cutex/components/updated`: `{name, reason: "operation"|"state"|"startup", state,
  operation: <operation>|null}`, on every operation step, at its end and on state changes.

## Rules

1. **Versions are immutable and kept side by side** in `<install base>/components/<name>/<version>/`
   with a `receipt.json` (sha256 of every unpacked file). Install = unpack + select for new agents.
   Existing agents keep the version they were launched with; a version something pins is never
   deleted (`prune` removes only unpinned versions that neither the running nor the previous cutex
   release names).
2. **Running jobs are never killed.** A service restarts only when its version or configuration
   changed and nothing would be interrupted; otherwise the new version is selected and the status
   shows `restartPending` until the restart is harmless.
3. **Capabilities are static per build.** Component state lives only in `GET /v2/components`. A
   component failure never fails or rolls back a cutex update; the previous version stays
   (`degraded`).
4. **Hand-made hosts are left alone.** An unmanaged host (no `install.json`) gets
   `409 unmanaged_install`; a service cutex did not install is reported `external` and never
   replaced (`409 component_external`).
5. **A component's version is its own identity** and changes only when its bytes change.

## Layout

```
components/<name>/
  component.json   {"schema": "cutex/component/v1", "name", "version", "platforms"}
  README.md        what it is, who needs it, what the user must do
  build.sh         <platform> <version> <out-dir>
```

`build.sh` writes `component-<name>-<version>-<platform>.tar.gz` (ustar, regular files and
directories only, owner 0:0, one mtime, `gzip -n`), `SHA256SUMS-component-<name>-<platform>` and
`BUILD-INFO-component-<name>-<platform>.json` (`name`, `version`, `commit`, `platform`). The archive
carries its license files (the component's LICENSE / NOTICE and a THIRD_PARTY_NOTICES.md for what it
bundles); like any other file, a change to them changes the archive and needs a new version (rule 5).

## Releases

Each component version is published once under its own tag `component-<name>-<version>`. The signed
cutex release manifest names, per component, the one version that cutex release was tested with:

```json
"components": {
  "job-service": { "version": "0.1.0", "artifacts": { "linux-x64": {
    "tag": "component-job-service-0.1.0", "name": "component-job-service-0.1.0-linux-x64.tar.gz",
    "sha256": "…", "size": 0 } } }
}
```

`cutex component install` downloads that archive from the configured release source, checks size
and sha256 against the verified manifest, and unpacks it. `cutex upgrade` stages the new versions of
installed components with the cutex release and selects them during the swap; a rollback selects
the previous versions again. Releases without a `components` section leave installed components as
they are. `--from-source` (CLI only) builds from a checkout instead (`origin: source`).
