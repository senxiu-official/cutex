# Agent Management v1 native provider boundary

Agent Management project authority comes only from the authenticated runtime's
current durable Cutex session and the Agent Management provider store. Agent Bus
groups are collaboration and discovery labels. Neither `CUTEX_AGENT_GROUPS` nor
any `project:*` group grants, selects, or implies Agent Management authority.

## CLI contract for the cute-codex 0.150 consumer

Invoke exactly one operation with:

```text
cutex agent manage <operation> --request-file <private-json-path>
```

`<operation>` is one of `create`, `query-managed`, `online`, `offline`,
`restart`, `close`, `replace`, `grant-operator`, or `revoke-operator`.
`director-rotate` was removed in r219 (see [r219-removals.md](r219-removals.md)).
The request file must be a private regular file. The caller process supplies its ambient `CUTEX_AGENT_ID`;
normal Cutex Agent Bus URL and token configuration authenticate that live
runtime. The consumer must not read or interpret `CUTEX_AGENT_GROUPS`.

The request is strict Agent Management v1 JSON:

```json
{
  "schema": "cutex/agent-management/v1",
  "action_id": "stable-action-id",
  "project_id": "optional-project-selector",
  "operation": "query_managed"
}
```

`project_id` may be omitted. It is a selector, not an authority claim. Caller
session, runtime, Director, and authority-epoch claims are forbidden in
this model-authored document. Operation-specific fields retain the existing v1
shape.

The CLI writes one `cutex/agent-management/v1` response as compact JSON to
stdout. A service-level denial is a successful CLI exchange with a typed
`no_write` outcome. Local file, transport, or response-decoding failure exits
nonzero. Complete receipts retain their concrete `project_id`, canonical parsed
request SHA-256, operation, result, and normal Agent Management v1 identities.

## Project selection

After Agent Bus authentication, Cutex resolves the runtime Agent ID to one
active, non-retired durable Cutex session whose current runtime occurrence is
the sender. It then reads the Agent Management provider store:

- one authorized project plus no selector selects that project;
- multiple authorized projects require a selector;
- a selector must name a project currently authorized to that caller;
- no authorized projects denies ordinary Workers and retired Directors;
- an exact existing action replay retains its originally resolved project while
  still requiring the caller to hold current authority for unfinished effects.

## Project roles

Each project has at most one **Primary Director** (see "Projects without a
Director" below), stored in `authorized_director_session`. This project
authority is the only Director source: there is no Director seat. The Primary
Director retains grant/revoke, Project presentation, failure-routing,
ownership-import, and reservation-reconciliation authority.

An **Agent Operator** is an additive durable grant keyed only by the exact
`ProjectId` and exact Cutex session ID. The grantee must be an active,
unretired explicit managed Agent in that project. Operators may query the
project and managed roster and may create, online, offline, restart, close, or
replace ordinary same-project Agents. They cannot change the Director,
grant/revoke Operators, change presentation, import ownership, reconcile
reservations, act on the Primary Director, or disable another privileged
Operator. Names, cwd, groups, model, profile, runtime IDs, and prose never
confer this role.

An **Observer** is a read-oriented product role, not a persisted Agent
Management grant in v1. This change does not infer Observer access from an
Operator grant.

Removed in r219 (see [r219-removals.md](r219-removals.md)): the Task Service
Director seat copy, the legacy global `cutex-director` / `cutex-release` seats,
and agent-initiated Director rotation. Tool roles, owner-notify `directors`, and
project managers come only from `authorized_director_session`.

Grant and revoke are strict typed Agent Management operations with stable
action IDs and an `expected_grant_revision` compare-and-swap. Successful
changes increment the per-project grant-set revision and append an immutable
audit event. Authorization is rechecked while holding the provider-wide
cross-process mutation lock. Therefore revoke and lifecycle operations have a
single linear order: once revoke wins, no new or unfinished external effect can
start. An already-terminal complete/no-write response remains exactly
replayable by its original authenticated caller without a new effect.

Selection failures do not reserve an action or mutate provider state. Stable
typed codes include `stale_runtime_identity`, `not_authorized_director`,
`project_selection_required`, and `project_not_authorized`.

## Managed Agent process boundary

On Linux with a running systemd user manager, every manager-owned app-server
core starts in a stable per-`cutex_session` transient scope named
`cutex-agent-<readable-id>-<identity>.scope` beneath `cutex-agents.slice`.
`systemd-run --scope` executes the app-server in place, so the PID persisted in
the runtime binding and Agent Bus registration remains exact. Commands, tests,
builds, and their crash/resource accounting inherit the Agent scope instead of
the Cutex service cgroup. The optional native TUI remains a detachable peer and
is not the tool-execution owner.

`offline`, `close`, failed-start cleanup, stale-runtime recovery, and therefore
`restart` signal the entire Agent scope before applying the existing exact-PID
and durable CAS cleanup. Graceful lifecycle sends `SIGTERM` and preserves the
existing bounded failure result; force lifecycle may then send `SIGKILL`.
Transient units use `--collect`, allowing the same durable Agent name to be
reused by its next runtime generation. An occurrence already running before
this code is installed remains in its original cgroup until its next explicitly
authorized launch or restart; installation does not migrate or restart it.

Windows, non-Linux platforms, Linux not booted with systemd/cgroup v2, and
Linux without a reachable user manager or the required
`systemd-run`/`systemctl` capabilities retain direct process launch. That
supported fallback is explicit in
`CUTEX_MANAGED_AGENT_ISOLATION=direct:<reason>` and the Cutex service log.
Detected systemd scope startup failures do not silently retry outside the
scope; normal launch readiness fails with the wrapper error in the runtime log.
No lifecycle path starts an alternate service, scheduled task, or visible
terminal fallback.

## Crash-safe predecessor closure

Replace with `close_before_create` or `close_after_ready` durably commits an
exact `predecessor_closing` intent before the provider invokes the external
close. The intent remains bound to the original action digest, project,
expected predecessor, successor reservation, policy/mode, and current Director
authority.

If the process dies after external retirement but before
`predecessor_closed`, exact replay observes only that named managed predecessor.
It requires the complete durable/native identity, managed specification,
positive runtime generation, inactive lifecycle state, and absence of every
runtime and Agent Bus endpoint. The provider then CAS-retires the unchanged
ownership record and advances the original action. Partial, mismatched,
cross-project, authority-changed, or externally unknowable state fails closed.
For `close_after_ready`, replay validates and reuses the already-created exact
successor. It does not bootstrap again or resend the idempotent handoff.

## Director rotation (removed in r219)

`director_rotate` (the coupled project-authority + Task Service seat transfer,
the `authority_transfer_pending` recovery state, and the Operator grant retained
for a predecessor) was removed in r219; see [r219-removals.md](r219-removals.md).
New requests with it are refused with `tool_removed`, and an in-flight
`director_rotate` action found on recovery is finished as failed
(`director_rotate_removed`). The operation kind, `rotation_mode`, and the
rotation audit kinds stay decode-only so recorded receipts and audits still
load. The Director changes only through `create_project` / `assign_director` /
`clear_director` (below) and the owner-initiated rotation (next section).
Capability `task-service-removed` (replacing `session-tasks` and `task-detail`)
advertises that the Task Service routes this rotation shared a store with are
gone.

## Owner-initiated Director rotation (`owner-rotation`)

Only the owner rotates a Director (root credential; no agent tool). The
Management v2 routes `/v2/director-rotations` (`owner_rotation_api.rs`, schema
`owner-rotation.json`, CLI `cutex human project rotate-director` / `rotation`)
drive a persisted state machine (`runtime/owner-rotation/rotations.json`,
`owner_rotation.rs`): ask the Director by an owner message to write
`<workspace root or Director cwd>/handoff/<UTC ts>.md` and end its turn
(`handoff_requested` -> `handoff_written`, or `timeout` where the owner chooses
continue or cancel) -> create the successor unstarted (`successor_created`) ->
the human mutation `rotate_director` (`authority_epoch` fence, successor must be
an Active non-Operator candidate as for `assign_director`, epoch + 1, audit
`director_rotated`, Operator grants and memberships kept, the predecessor stays
a member) (`authority_rebound`) -> dispose the predecessor (member / operator /
archive; a failure is a warning) (`predecessor_disposed`) -> start the successor
with a message naming the handoff note (`done`). Cancel works until the
successor creation begins. After a restart every state resumes; only a
successor creation without proof of its outcome fails
(`successor_creation_interrupted`). Updates are host events
`cutex/ownerRotation/updated`.

## Minimal downstream change

The cute-codex 0.150 handler should remove ambient-project discovery and all
`CUTEX_AGENT_GROUPS` parsing. Keep `CUTEX_AGENT_ID`, the existing argv operation
mapping, private request file, timeout, and typed response handling. Add an
optional `project_id` selector to the native tool/provider request; omit it for
the common one-project Primary Director or Operator. Continue to reject model-supplied caller,
runtime, Director, or authority claims.

The handler can still compute `request_sha256` from its stable serialized
request bytes. For an explicit selector, require the receipt/failure project to
match it. For implicit selection, accept one syntactically valid concrete
project from the typed provider payload and require that same project
consistently within the payload; the authenticated Cutex provider is the source
of that resolved value.

## Root-only legacy Director ownership import

A project whose authority predates Agent Management may have an authorized
Director but no `ManagedAgentRecord`. An Owner may import exactly that missing
record once through the dedicated Management root credential:

```text
cutex management agent-ownership-import \
  --request-file /private/path/legacy-director-import.json
```

The request is closed JSON and contains only stable action identity plus the
project-authority CAS:

```json
{
  "schema": "cutex/agent-management/legacy-director-ownership-import/v1",
  "action_id": "legacy-director-import-example-r2-01",
  "project_id": "example-project",
  "director_cutex_session_id": "cutex.example-director-r2",
  "expected_authorized_director_session": "cutex.example-director-r2",
  "expected_authority_epoch": 1
}
```

Before the atomic Agent Management write, the server loads the exact durable
session key and validates active lifecycle, local host ownership, native
session identity, canonical persistent-Agent state, and every field of the
managed spec. Those facts are never accepted from the request, Agent Bus
groups, cwd hashes, runtime IDs, or prose. The operation leaves project
authority unchanged. Its typed receipt records the authoritative session
revision and runtime generation used as evidence. Exact action replay returns
the immutable receipt with `replayed: true`; changed-payload reuse and every
identity, authority, lifecycle, or ownership conflict fail closed.

This route is authenticated by the dedicated Agent Management root credential,
the same administrative credential class as `management agent-authority`. It
is not an `AgentOperation`, is not registered in the Director/Worker tool, and
does not expose a general adoption mechanism.

## Exact create retry before native session creation

Create keeps its original action ID, request digest, project, reservation,
specification, groups, and message identity when profile/bootstrap preflight
fails before a native command can be spawned. The production lifecycle now
validates the selected profile and its credential/config files in-process. A
failure at that boundary is definite: no native SID or durable Agent can have
been created. The action records a `native_bootstrap_retryable` permit and does
not cache the `owner_action_required` response as terminal.

An identical request from the same still-authorized project Director consumes
that permit in the durable store before attempting startup again. This
consume-before-launch fence makes concurrent exact retries and service restart
safe: at most one retry can launch, and a crash after permit consumption fails
closed instead of launching again. Success continues the original action and
uses the original idempotent message identity. A changed payload conflicts and
a fresh action remains blocked by the unresolved reservation.

Future native bootstrap uses `codex exec --json`. The provider accepts a SID
only from the exact JSONL event `{"type":"thread.started","thread_id":"<uuid>"}`
on stdout. UUIDs in profile diagnostics, stderr, or any other structured event
are ignored. Missing, conflicting, malformed, or non-UTF-8 structured output
fails closed. A nonzero command that emitted one valid creation event retains
that exact SID for the existing captured-session continuation and never
launches a second native session.

R23 also recognizes the exact historical `native_session_unknown` receipt
shape emitted when the old wrapper completed nonzero without printing a SID.
That receipt text is only a reconciliation trigger, not proof of absence. Before
upgrading it, the lifecycle provider must read the Agent Bus registry, durable
Cutex session store, the direct cute-alden session registry, native session
index, and native rollout tree. Runtime evidence is correlated with the launch
profile, runtime name, exact requested groups, deterministic cute-alden name,
and reserved managed cwd as available; those markers are evidence only and
never grant authority. Native index and rollout records are merged by native
session ID so an explicitly unrelated cwd in the same attempt window is
ignored, while an index-only record without a cwd marker remains ambiguous.
Reconciliation selects the launch profile's actual Codex home: the aggregate
host Codex home for a host profile, or the host-mounted container `.codex` home
for a Docker profile. Rollout filenames are local wall-clock discovery hints,
not attempt-time proof. Inclusion uses the embedded RFC3339 creation timestamp
and enclosing `session_meta` event timestamp; both must be present, valid,
consistent, and inside the attempt window. Missing, malformed, conflicting, or
window-straddling embedded timestamps fail closed.

Only complete absence across every provider-owned source authorizes retry.
Exact present evidence, ambiguous/mismatched evidence, malformed evidence, and
an unavailable source remain fenced without launching. On exact replay the
provider projects a transient typed owner-action code ending in `present`,
`ambiguous`, or `unavailable` with a concise reason. The original action,
response, and failure event remain byte-for-byte unchanged in the durable
store. The original caller must still be the current project Director;
immutable completed/no-write receipts and all other owner-action-required
results remain unchanged.

The exact historical `native_session_ambiguous` receipt produced by legacy UUID
scraping has a separate no-relaunch continuation. Under the current project
Director's authority, the provider reads the most recent durably journaled
`native_bootstrap_pending` attempt window and reconciles Agent Bus, durable
session, deterministic cute-alden, and selected-profile native rollout/index
evidence. Only one valid UUID that agrees across all present sources may be
atomically recorded as `native_session_captured`; the same action then adopts
that existing thread. Zero, multiple, conflicting, malformed, stale, or
unavailable evidence returns a transient typed reconciliation fence and never
authorizes another bootstrap. Concurrent replay and service restart converge
on the captured SID, original request digest/reservation/spec/groups, and
original message ID. The historical failure event remains immutable.

## Root reservation reconciliation

`POST /v2/agent-management/reservation-reconciliation` and
`cutex management agent-reservation-reconcile --request-file ...` are dedicated
root-administration surfaces. They are not Agent operations and are never
accepted through the ambient Agent Bus lifecycle tool. The strict request
contains only CAS selectors for the project authority and one legacy reserved
action. The provider clears no reservation unless the immutable failure is the
allowlisted pre-effect predecessor-ownership failure and every authoritative
Agent, durable-session, Agent Bus, cute-alden, native-index, and rollout source
proves the reserved successor absent. Success appends audit and `no_write`
phase evidence, preserves the original failure journal, and caches an
idempotent reconciliation receipt.

## Cutex Projects read and presentation projection

`AgentManagementProvider::list_cutex_projects` and `read_cutex_project` expose
the bounded workspace projection for an authenticated Agent Management
principal. The provider store's exact `ProjectId` key is the sole ownership
identity. Reads require the caller's durable Cutex session to be the current
Primary Director or an exact active Agent Operator and return only that exact
project's authority epoch, Primary Director, Agent Operators, ordinary active
durable Agents, retired durable Agents, and runtime
observations where available. Groups, cwd, names, runtime IDs, and native Codex
workspace records never select a project or grant access.

Project presentation is a separate non-authoritative record keyed by canonical
`ProjectId`. It contains only a display name, a one- or two-cell badge, a color
from the existing high-contrast named palette or a custom `#RRGGBB` value,
update provenance, and an independent revision. Missing presentation records
produce deterministic defaults without writing. Only the authenticated current
Primary Director can update presentation, and the canonical ID and authority
fields are absent from the editable payload. Optimistic presentation revisions
prevent lost updates. Additive store and presentation fields survive
presentation-only writes.

The terminal UI presents this projection as **Cutex Projects**. The unrelated
native Codex catalog remains available as the secondary **Workspaces** entry
and retains its existing create and import operations.

R36 adds a second, explicit read/write adapter for the normal Human TUI. It is
reachable only through dedicated Management-root HTTP routes. The server mints
a private, non-serializable Human principal after authenticating that bearer;
the client request never supplies or fabricates `CUTEX_AGENT_ID`, a Director,
or an Operator identity. Collection and detail reads preserve the same
canonical provider authority projection.

Human presentation changes compare both the current project authority epoch
and presentation revision and record Human/Management provenance without
mislabeling the Primary Director as the actor. Human Operator grant/revoke
requests compare both authority epoch and grant-set revision, use a separate
idempotency domain, and append the same durable Operator audit facts with an
explicit Human/Management actor marker.

Completed legacy retained-predecessor rotations can be shown as review-only
Operator repair candidates when their exact successor and authority epoch are
still current. Projection is read-only: the only repair path is an explicit
confirmed Human grant through the normal CAS and audit boundary.

Persistent durable Agents, including Offline Agents, can now be explicitly
imported through the Human/Management boundary and confirmed for Create/Add or
guarded Detach then Move (formal-name provenance, eligibility, nullable profile,
locking and recovery are checked by the server).

## Projects without a Director

Capability `project-optional-director`. A Project exists through its
`project_states` record; the `projects` authority map only holds Projects that
currently have a Director. Stores written before this change are migrated on
load (`migrate_project_states`: an authority without a state record gets its
effective state, active at revision 0). Rollback to an older cutex is not
read-compatible once a Project without a Director exists: restore the
pre-deploy backup of `runtime/agent-management/v1`.

`POST /v2/agent-management/project-mutations` adds three kinds (same request
envelope, `action_id` idempotency, optional `expected_authority_epoch` /
`expected_project_revision` fences):

- `create_project {presentation, workspace_root?, director_cutex_session_id?}`:
  creates the Project at revision 1, optionally with its canonical workspace
  root and an initial Director. The Director must be a managed, active,
  independent Agent; it joins the Project. Requires zero/absent CAS.
  `create_workspace_root: true` (capability `project-create-workspace-root`)
  creates a missing workspace root (the missing tail, as the daemon user)
  after the id and Director checks passed; `.` / `..` components are
  `invalid_workspace_root`, a file in the way `workspace_root_not_directory`,
  any other failure `workspace_root_create_failed`.
- `assign_director {cutex_session_id}`: assigns a Director to an active Project
  that has none. The candidate is an independent Agent (it joins) or a current
  member. The authority epoch continues from the last cleared Director
  (`ProjectStateRecord.last_director_epoch`), so epochs stay monotonic; the
  change is a single Agent Management commit.
- `clear_director`: clears the Director of an active Project. The former
  Director stays an ordinary member. Refusals name the blocking precondition:
  `director_rotation_in_progress` (a Director rotation action recorded before
  r219 is still unresolved; recovery finishes it as failed) and
  `project_has_agent_operators` (revoke the Operator grants first).

These three kinds change Agent Management only. The Director is
`ProjectAuthority.authorized_director_session` and nothing else; the Task
Service Director seat store they used to leave stale was removed in r219, so
no seat copy exists to disagree with the authority.

Without a Director, `authority_epoch` is the last Director's epoch (0 if there
never was one) and is the CAS fence for the other mutations; summaries omit
`director_cutex_session_id` / `director_name`, workspaces omit `director`.
Membership, workspace root, project memory, scoped MCP, archive / restore /
remove all work without a Director; Director-only tools are simply unavailable.
Workspace root does not yet record a native Codex project trust entry for the
path (follow-up).
