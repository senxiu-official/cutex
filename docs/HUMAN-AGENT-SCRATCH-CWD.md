# Scratch cwd for `POST /v2/human/agents`

Capability: `human-agent-scratch-cwd` (`features.humanAgentScratchCwd`). Owner-approved 2026-10-12
for the desktop launcher (create an agent without choosing a folder).

## Request

Unchanged shape (`HumanCreateAgentRequest`, root only, idempotent by `requestId`):

```json
{ "requestId": "…", "name": "Worker", "profile": "…", "start": false, "launch": { … } }
```

How the cwd is chosen:

| `cwd` | `projectId` | cwd of the new agent |
|---|---|---|
| given | any | exactly `cwd` (must be an existing directory) |
| absent | Project with `workspace_root` | the workspace root |
| absent | Project without `workspace_root` | refused: 400 `workspace_root_required` (unchanged) |
| absent | absent | **new**: a fresh private scratch workspace (was 400 `cwd_required`) |

The response is the usual `{contractVersion, requestId, cutexSessionId, session}`; the scratch path is
the session's `cwd`.

## Layout

`<cutex config dir>/agents-scratch/<yyyymmdd>-<8 hex>` (e.g. `~/.cutex/agents-scratch/20261012-1a2b3c4d`),
next to the quick-chat scratch dirs `<cutex config dir>/chats/<yyyymmdd>-<8 hex>`. Both use the same
helper (`human_sessions::create_private_workspace_in`): `agents-scratch` and each workspace are
created owner-only (mode 0700 on Unix; protected owner-only DACL on Windows). The directory is just
the session cwd, so it survives restarts like any other cwd. cutex never deletes a workspace an
agent uses (deleting the agent leaves it, as for quick chats).

## Retries and failures

- The directory is allocated after every validation step (profile, launch options, Claude
  launch defaults), right before the agent is created. Validation refusals leave nothing.
- If creation fails before an agent exists (`agent_creation_failed`), the still-empty directory
  is removed (`remove_dir`, never recursive). Allocation errors answer `scratch_dir_failed`.
- A retry with the same `requestId` replays the stored answer (idempotency store), so it never
  allocates a second directory. `agent_setup_incomplete` (agent exists, a follow-up failed)
  keeps the directory: it is that agent's cwd.
- A daemon crash between allocation and creation can leave one empty directory under
  `agents-scratch`; the retry then answers `request_outcome_unknown`. Empty directories there
  that no session uses can be removed by hand.

## Rollback

An older cutex (without this capability) answers `cwd_required` again for a cwd-less, Project-less
request; the desktop checks the capability first and then asks for a directory. Agents already
created keep their `agents-scratch/…` cwd and keep working (it is an ordinary cwd); the directories
stay. No store format change, no migration.
