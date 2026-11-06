# Removed legacy Task Service interfaces

The legacy Task Service, Release rotation and copied Director seat are not part
of the public cutex workflow. Do not use historical Task Service commands as
substitutes for current delegations or jobs.

Use the current `cutex human --help`, Management v2 schema and capabilities to
choose supported commands. In particular, `cutex human tasks` and
`--cancel-tasks` are removed. Agent work is represented by sessions and
delegations; long-running external jobs use the optional Job Service.

A Project's Director comes from Project authority. There is no separate Task
Service seat that can override it. Historical transcript rows can still render
legacy task deliveries without making the removed actions available again.

## Historical archive key

An earlier migration retained removed data under the literal storage key
`archive/r219-removed/`. The directory name is a stable migration identifier,
not a public release version. A retained `.migration.lock` or `migration.json`
belongs to that migration's recovery record. Do not rename the key or move
archived directories back into active state while services run.

Public upgrades and supported recovery follow the signed release manifest.
Restoring an executable does not undo a one-way data migration; retain the
appropriate offline backup and stop all writers before a documented restore.

For current operations see [human CLI](management/human-cli.md),
[agent management](agent-management-provider.md) and
[agent communication](agent-bus-operations.md).
