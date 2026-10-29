# Safety grants (Claude Code safety checks under full access)

Capability `claude-safety-grants` (Management v2). Code: `components/claude-runtime/src/cute_claude/safety_grants.py`
(adapter), `src/management/v2/safety_checks.rs` (routes, observer hook), `src/owner_notify/store.rs`
(`SafetyBlock` on owner notices). Adapter feature flag: `safetyGrants` in `features.json` (claude-r14).

## Why

Full access (`bypassPermissions`) lets a Claude agent run any tool without asking. Claude Code still calls
the permission callback for a few **safety checks** that no permission rule can skip, for example an `rm`
on a possibly empty variable (`rm -rf $DIR/$name`) or an `rm` inside an `sh -c` it cannot parse. Before,
such a call went to the clients as an approval request with no timeout, so a turn another agent started
could wait for hours for an answer nobody saw.

The design follows how Claude Code's own auto mode treats a blocked action: decline at once, tell the
model what to do instead, and let a human allow the exact action later.

1. **Decline and rewrite.** The adapter declines the call at once with Claude Code's reason and a hint:
   rewrite in a checkable form (a literal path or `${VAR:?}`, no `rm` inside `sh -c`); if this exact
   command is truly necessary, do not retry it or work around the check, but stop, explain to the owner
   what the command is and why it is needed, and end the turn.
2. **Escalate without blocking.** The adapter records a *safety block* and reports it. cutex shows it to
   the owner as a 需要你 item. The turn never waits for it.
3. **The owner grants one exact retry.** 批准一次 stores a grant in the adapter for exactly that thread,
   tool and input, used once, valid 30 minutes, and cutex tells the agent
   「主人已批准一次：<command>（30 分钟内有效，仅此一次）」 as an owner input: idle, a turn now; busy, it
   joins the running turn at its next step (never held until the turn ends).
   不批准 closes the item without a grant.

This applies to every turn of a full-access Claude agent, whoever started it (the owner, another agent,
a service). Asking modes (`default`, `acceptEdits`, `auto`) are unchanged: their approvals still go to
the clients. Codex agents have no such checks.

## Adapter protocol (app-server style)

All three methods require `threadId`; unknown fields are refused.

| method | params | result |
|---|---|---|
| `thread/safetyCheck/list` | `{threadId, state?}` | `{data: [block]}` (newest first, at most 200) |
| `thread/safetyCheck/grant` | `{threadId, blockId, decidedBy: {kind: "owner", id?}}` | `{block}` |
| `thread/safetyCheck/dismiss` | `{threadId, blockId, decidedBy: {kind: "owner", id?}}` | `{block}` |

Notifications (durable in the adapter's event log, readable with `claude/events/read`):

- `thread/safetyCheck/blocked {threadId, turnId, itemId, block}`: emitted after the declined tool row
  (`item/completed`, status `declined`).
- `thread/safetyCheck/updated {threadId, block}`: granted, dismissed, or used (the grant was consumed).

A block:

```json
{
  "blockId": "sb-…", "threadId": "…", "turnId": "…", "itemId": "<tool_use id>",
  "toolName": "Bash", "toolInput": {"command": "rm -rf $S/$v", "description": "…"},
  "command": "rm -rf $S/$v", "reason": "<Claude Code's text>",
  "source": {"kind": "user|agent|service", "id": "…", "name": null},
  "state": "blocked|granted|used|dismissed|expired",
  "blockedAt": "…Z", "decidedAt": null, "decidedBy": null, "expiresAt": null, "usedAt": null,
  "grantSeconds": 1800
}
```

Rules:

- **Who may decide.** Only a connection that declared `claudeInteractiveApprovals` (cutex Management, the
  attached TUI) and only with `decidedBy.kind == "owner"`; anything else is refused with `data.code`
  `owner_required`. Every adapter connection holds the adapter's transport token, so this is a protocol
  rule; the authority check is cutex's owner-only route below.
- **What a grant covers.** The same thread, the same tool and the same *significant* input: for `Bash` the
  `command` only (description, timeout and background flag do not change what the check judged); for
  other tools the whole input minus `description`. Another command, another tool or another thread is
  declined as in step 1.
- **Once, 30 minutes.** The first matching `can_use_tool` call within 30 minutes of the grant is allowed
  and turns the block `used`; any later identical call is declined and recorded as a new block. After 30
  minutes the block reads `expired`.
- **Durable.** Blocks and grants live in the adapter's state database (`safety_blocks`), so a grant
  survives an adapter restart; it is still used only once. Forks do not copy them; deleting the thread
  deletes them.
- A block that is no longer `blocked` refuses another decision: `data.code` `safety_block_closed` (with
  `data.block`); an unknown id: `safety_block_not_found`.

## cutex

### Owner notice

cutex observes the adapter's notifications (the process that journals the runtime's events) and keeps
each block as an owner notice: `origin: system`, `systemKind: safetyBlock`, the block under
`safetyBlock`, notification event `system.safetyBlock` (default level `needsYou`, so it counts in 需要你,
the phone's needs-you list and the sidebar like other needs-you items; the owner may change its level in
the notification level tables). Recording is idempotent by block id.

A safety-block notice is pending while its block is `blocked`, also after the owner opened the agent
(reading the agent only acknowledges it). It closes when the block is decided. Live updates use the
existing `cutex/ownerNotify/created|updated` events; bootstrap `cutexState.ownerNotifications` and
`GET /v2/owner-notifications` include these records.

### Routes (owner only)

| route | |
|---|---|
| `POST /v2/sessions/{id}/cutex/requests` `cutex/safetyCheck/grant {blockId}` | grant one exact retry |
| `POST /v2/sessions/{id}/cutex/requests` `cutex/safetyCheck/dismiss {blockId}` | close without a grant |
| `GET /v2/safety-checks?state=open\|all&agentId=&limit=` | `{blocks: [notice]}` newest first |

Both methods need the Management root credential (an owner-bound device / client token counts). A
non-owner user is refused by the access gate (403 `owner_required`), a caller without a Management
credential (an agent, any other process) by the route (401). No agent tool, Agent Bus message or
delegation command can grant.

`grant` result: `{notification, block, submit}`. cutex forwards the decision to the agent's running
adapter (`thread/safetyCheck/grant`, `decidedBy: {kind: "owner"}`), closes the notice, then submits one
owner input to the agent (`clientUserMessageId` `safety-grant-<blockId>`, strategy `auto`: a busy agent
reads it at its running turn's next step (`turn/steer`; a Claude runtime without same-turn steering keeps
it in the Management queue for the next turn), an idle one starts a turn, an offline one is started). If that submit fails the grant stands and the result carries `submitError` instead of
`submit`.

`dismiss` result: `{notification, block}`. When the adapter is not running, the notice closes anyway
(the adapter's block stays `blocked`, which grants nothing).

Errors (`cutex.error.code`): `cutex_method_denied` (403, no root credential), `safety_block_not_found`
(404), `safety_block_closed` (409; also when the TUI decided first: the notice then takes the runtime's
state), `session_offline` (409, grant needs the running adapter: start the agent, then approve again),
`safety_grants_unsupported` (409, the agent's Claude runtime predates `safetyGrants`), `runtime_refused`
(409).

### Generic parts

The notice field, the routes and the desktop item do not depend on Claude: `safetyBlock.runtime` names
the runtime family (`claude` today). A runtime with similar checks only needs the same three adapter
methods and two notifications.

## Limits

- A grant needs the agent's adapter to be running; an offline agent must be started first.
- The owner input is the agent's cue to retry; cutex does not run the command itself.
- An agent with full access runs as the owner's OS user, so it can read whatever that user can. The
  owner-only checks above stop every cutex path (agent tools, bus messages, non-owner users), not an
  agent that deliberately uses the owner's own credentials outside cutex.
