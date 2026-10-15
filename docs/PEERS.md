# Daemon peers (P0.1)

Paired instances delegate to `name@host`. The destination runs an ordinary ask;
the origin holds a mirror and uses normal delivery and `delegation_wait`.
Identity is the durable `ci-…` instance ID, not the hostname. Separate homes on
one computer are separate peers. Remote IDs never become local sessions or
Agent Bus identities; mirrors never enter local admission, target permissions
or wait-graph edges, even when IDs collide.

## Pair and export

Select the intended instance with `CUTEX_HOME` and, for online administration,
`CUTEX_MANAGEMENT_URL`. Administration uses Management root/client credentials;
mutations require a loopback listener connection.

1. B: `cutex human peer invite --label alpha`.
2. A: `cutex human peer add <B-management-url> <one-time-code> --label beta`.
3. B: PATCH `/v2/peers/<A-instance-id>` with
   `{"policy":{"inbound":{"exports":[{"agent":"<worker-session-id>"}]}}}`.
4. A: PATCH `/v2/peers/<B-instance-id>` with
   `{"policy":{"outbound":{"visibility":"all"}}}`.
5. A's `agent_list.otherHosts` exposes `worker@beta` for `agent_delegate.target`.
   B's owner reviews incoming delegations by default.

`human peer list` and `human peer remove <peerId>` inspect/remove relationships;
`peers` is an alias of `peer`. Exports and remote visibility default to empty/
disabled. The target retains its own permissions; peers never auto-start an
offline runtime. Questions go to the destination owner. Only the final `@`
separates remote name/short ID and peer label. Unqualified names stay local;
a local miss may suggest qualified remote names.

## Credentials, callbacks and persistence

Every authenticated `/peer/v1/*` request, including loopback, needs both
`Authorization: Bearer <cxp1 credential>` and
`X-Cutex-Peer-Instance: <calling instanceId>`. Peer credentials cannot access
Management routes; root and `cxm1` cannot access peer routes. Only POST `pair`
is public, authenticated by its one-time code. Credentials are instance-bound;
revoked, disabled, removed-by-peer and expired-pending credentials are refused.

`cxp1.` encodes 32 OS-random bytes as unpadded base64url. The existing UUID v4
OS RNG supplies them: discard the version/variant octets and copy the remaining
random bytes directly, without hashing or adding a dependency. Incoming tokens
are stored only as `SHA-256("cutex-peer-token-v1\0" || token)` and compared in
constant time. Outgoing tokens live in owner-private
`runtime/peers/secrets/<peerId>.token`; values and hashes never appear in views
or audit logs.

HTTP URLs must resolve entirely to loopback, `100.64.0.0/10`, or
`fd7a:115c:a1e0::/48`. Ordinary LAN/public/link-local addresses, URL credentials,
paths beyond `/`, queries and fragments are refused. Each request resolves once,
validates every result and connects to those pinned addresses. No proxies or
redirects are used; total response size is limited to 1 MiB.

A non-loopback pairing connection cannot register a loopback callback
(`peer_callback_forbidden`). The restriction persists and is checked on every
later DNS result, preventing rebinding to loopback. Local pairing can use
loopback. No listener is added: non-loopback pairing needs an existing permitted
remote Management listener for its callback, else `peer_callback_unavailable`.

Codes expire after ten minutes, are single-use, and have bounded issuance and
redemption attempts. Pending records expire after two minutes and permit only
hello, confirm and abort. Both sides verify hello before activation. Active
confirm is idempotent without another callback. Abort rejects active peers
with `peer_already_active`; active removal uses unpair. Failed pairing cleanup
tries abort, then unpair if the remote already activated. A process crash
between the two confirmations can still leave an unreachable active relationship
requiring owner removal; pairing is not a distributed transaction.

`runtime/peers/peers.json` uses a file lock and atomic replacement. Files use
owner-private Windows ACLs/Unix permissions. Global settings are authoritative
in `config.json`. GET `/v2/settings` does not lock or create `runtime/peers`;
the store is created on use. There is no first-enable ask-store backup.

## Wire protocol and compatibility

Hello exchanges integer `protocol: 1` and features. Four are required:
`asks.v1`, `asks.snapshot.v1`, `asks.close.v1`, `agents.list.v1`. The optional
`tells.v1` (cross-host tells, below) is advertised but never required, so
older peers stay compatible. Only the negotiated intersection may be used. The Management view exposes
`features:{declared,negotiated}`. Missing features produce `peer_unsupported`,
`state:versionMismatch` and `lastError.details.missingFeatures`. Data outboxes
pause while hello probes continue with backoff; compatible hello and directory
refresh restore transport. Changed identity similarly holds data traffic as
`identityChanged`.

`src/peers/wire.rs` defines independent `WireCreateV1`, `WireSnapshotV1`,
`WireAnswerV1` and wire enums, with explicit conversions to private ask models.
Create/snapshot `schemaVersion` is 1; omission defaults to 1, another version is
unsupported. Internal `AskOptions`, answers and state enums are not embedded
in the wire representation.

Unknown object fields are ignored. Unknown enum strings decode as
`Unknown { raw }`. An unfamiliar v1 snapshot is acknowledged by revision and
retained in `remote.remoteState`; its mirror gets `hold.reason:peerUnknown`,
`remote.syncState:remoteStateUnknown`, and
`对端状态：<field=raw>（本机版本不认识）`. Fallback enum values do not drive execution
or result delivery. Unknown command semantics are refused as protocol errors.

P0.1 is incompatible with the P0 `asks-v1` feature. Capabilities retain `peers`
and `cross-host-asks` (JSON flags `peers`, `crossHostAsks`) and add `peers-v2`
(`peersV2`). Desktop clients needing this contract should check `peers-v2`;
re-pair old demo relationships when upgrading.

### Canonical create digest

`requestSha256` is lowercase hexadecimal SHA-256 of compact UTF-8 JSON with
object keys recursively sorted and array order preserved. The v1 field set is
frozen below. Exclude `requestSha256`, unknown fields and private model fields.

```json
{
  "schemaVersion": 1,
  "requester": {
    "cutexSessionId": "<requester session>",
    "name": "<canonical name or null>",
    "caps": {
      "sandbox": "<sandbox or null>",
      "networkAccess": false,
      "unrestrictedServices": false
    }
  },
  "target": "<target session>",
  "title": "<title>",
  "text": "<brief>",
  "options": {
    "ifBusy": "queue",
    "review": false,
    "questions": "requester",
    "notify": "turn",
    "context": {"kind": "brief"}
  },
  "hops": 0
}
```

The shown option values are decoding defaults; actual decoded values are
hashed. Missing name/sandbox become null, capability booleans false, options
the defaults above, and hops zero. Remote delegation supports context `none`
or `brief` and sends question routing `owner`.

Before hashing and persistence, requester names fold Unicode whitespace/newlines
to one interior space, remove other control characters and bidi-format ranges
U+202A–U+202E/U+2066–U+2069, trim surrounding whitespace, and truncate to 128
UTF-8 bytes at a character boundary. The same
single-line sanitizer protects peer-provided requester names, labels and target
names shown to agents. Title/brief content is not name-normalized. Key ordering,
additive unknown fields and future internal fields cannot change the digest.

## Policy and settings

PATCH merges policy fields. `inbound.exports` is **whole-array replacement**,
not an incremental add/remove API. `assumedCaps` is also replaced as an object.
Changes increment `policyRevision` and invalidate approval evidence for work
not yet admitted.

| Policy field | Default / allowed |
| --- | --- |
| `inbound.exports` | `[]`; `{agent:id}`, `{project:id}`, `{all:true}` entries |
| `inbound.accept` | true; false refuses new incoming asks |
| `inbound.review` | `always`; also `ifTargetWider`, `never` |
| `inbound.assumedCaps` | `{sandbox:"read-only",networkAccess:false,unrestrictedServices:false}` |
| `inbound.maxOpenAsks` | 20; 1–10000 |
| `inbound.asksPerHour` | 60; 1–100000 |
| `outbound.visibility` | `none`; also `all` |

`always` requires ordinary destination `ask_delivery` approval for every new
incoming ask. `ifTargetWider` compares the target to **locally configured**
`assumedCaps` using sandbox rank, network and unrestricted services; an unknown
sandbox requires review. `never` means full trust in the peer host for this
admission check. None widens target permissions. Request-body capabilities are
untrusted `claimedCaps` for display, not the approval baseline. Approval binds
peer identity, policy revision, claimed/assumed capabilities and target facts.
The API emits `accept`; P0's input spelling `asks` remains an accepted alias.

GET/PATCH `/v2/settings` provides the `peers` section below. Peer-setting writes
are loopback-only. Successful pairing enables peers.

| Setting | Default | Range |
| --- | --- | --- |
| `enabled` | false | boolean |
| `outboxMaxAgeHours` | 24 | 1–720 |
| `maxHops` | 3 | 1–8 |
| `refreshAgentsSecs` | 60 | 10–3600 |
| `connectTimeoutSecs` | 5 | 1–60 |
| `readTimeoutSecs` | 30 | 1–300 |

Every ask records hops. Local creation inherits the maximum along `parent_ask`
ancestry; crossing a host adds one, and reception retains the wire value.
Local intermediary delegation cannot reset it. Exceeding `maxHops` produces
`peer_hops_exceeded` and an ordinary failure delivery at the origin.

## Scheduling and delivery

Different peers run independently; each peer's HTTP calls are serial, including
directory/handshake requests. URLs are attempted in configured order with the
last successful URL first. Transport failure tries the next URL before marking
the peer unreachable. Local address validation failures (`peer_url_insecure` /
`peer_callback_forbidden`) also try the next URL; semantic/protocol refusals do not.
Connection and read timeouts are separate; trickling cannot extend the total
response-read deadline.

When every URL fails transport or local address validation, the peer opens its
circuit (`unreachable`, `nextProbeAt`) and keeps pending operations and events.
The first such failure ends that peer's data pass; it does not fail asks or
dead-letter events. Hello/directory refresh uses the same backoff. Until the
probe is due, that peer sends no ask/event traffic. Recovery performs hello and
directory refresh, then flushes outboxes. Retry base is 2–300 seconds with ±20%
jitter. The local one-second lifecycle sweep never waits for network I/O and
includes disabled, unhealthy and missing peers. Accepted open asks reconcile
periodically (ten minutes) and after peer recovery.

Create and its outbox entry commit together; incoming state and its snapshot
outbox also commit together. Complete snapshots replace older pending revisions.
Duplicate PUT with the same peer, ask ID and canonical digest returns the existing
snapshot, including after a lost response; conflicts are rejected. Stop/close
effects and receipts share one lock. Deterministic answer IDs/delivery keys and
monotonic snapshot revisions prevent duplicate execution and result delivery.

A close can arrive before its delayed PUT. The destination reserves that ask
ID in durable `AskState.peerCancellations`, bound to the peer and operation;
a later PUT returns 410 `ask_gone` without executing. The create path rechecks
the receipt under the same store lock. Receipts do not expire (which would
permit late ID reuse), are cleared on peer removal, and are capped at 10000 per
peer; reaching the cap returns 429. Closed mirrors with pending expiry closes
remain available for delivery rather than aging out with ordinary ask history.

An unacknowledged create expires as `failed/peer_unreachable`. Accepted asks
without contact for `outboxMaxAgeHours`, or expired identity/version holds,
close as `interrupted/outcomeUnknown`. Expiry retains
`close(abandoned, reason=expired)` until the destination acknowledges it or the
peer is removed; an accepted-but-unacknowledged destination ask is then stopped
through its ordinary lifecycle.

A locally terminal mirror never reopens or delivers another result. Later
snapshots update remote diagnostics only. A later answer appears as top-level
`lateAnswer` (snapshot metadata plus local `artifactId` and bounded `preview`), without waking the originator. Neither `remote.remoteState` nor `lateAnswer` stores inline `text`; full answers live only in local artifacts. Remote diagnostics are normalized to a single line and capped at 512 UTF-8 bytes. State deliveries use the fixed local reason `peerState`, never remote error text.

### Peer-state transitions

The table applies to open asks; locally terminal outcomes remain final.

| Change | Outgoing mirror | Incoming ask |
| --- | --- | --- |
| Peer `enabled:false` | Immediately interrupt/close, `peerDisabled`, ordinary delivery; retain close until re-enabled | Refuse new asks; running work continues; results wait for re-enable or expiry |
| Global `peers.enabled:false` | Same, `peersDisabled` | Same retention/expiry rule |
| `identityChanged` | Hold `peerIdentityChanged`; expiry → `outcomeUnknown` | No authenticated work from changed identity; results wait for recovery or expiry |
| `versionMismatch` | Hold `peerVersionMismatch`; expiry → `outcomeUnknown` | Results wait for compatible hello or expiry |
| Owner delete | Immediately interrupt, `peerRemoved`; clear operations | Stop via ordinary close lifecycle; discard result outbox |
| Authenticated unpair | Same, `removedByPeer` | Same stop/discard behavior; revoke credentials and retain disabled `removedByPeer` diagnostic record |

Ordinary target stop cancels queued work or queues interruption of an admitted/
running attempt through its existing runtime command path. Abort is pending-only
and cannot remove an active relationship or orphan its asks.

A result event rejected with 404/410 or another nonretryable semantic error
moves to `deadLetters`. Transport failures retry only until `outboxMaxAgeHours`,
then also dead-letter. The destination exposes `resultUndeliverable:true`,
records the error and stops further result delivery for that ask. Retention is
the latest 100 events **per peer across its asks**, with an audit entry. Protocol
response decoding errors retain the outbox during hello recovery, subject to the same age limit. A request rejected with HTTP 400 `peer_protocol_error` instead fails only that delegation/event as `peer_request_incompatible`; other requests to the peer continue.

For an accepted, nonterminal ask, an incompatible `stop` cannot establish that
remote execution stopped. The mirror closes once as `interrupted/outcomeUnknown`
and durably queues `close(abandoned, reason=peer_request_incompatible)`, including
across restart. Acknowledgement removes that close without changing the local
outcome or issuing a second terminal delivery. If the close itself is also
incompatible, it is removed with the fixed diagnostic retained; remote execution
remains unknown and may require owner intervention on the destination.

Briefs are at most 4 KiB. Answer text is copied into normal local artifact
storage for reads/waits and limited to 64 KiB on the wire with a truncation
notice. Artifact paths, native diff, profile/model and token usage are omitted.
Attachments, remote file reads/wake, follow-up, await/resume/reopen, brief
revisions and remote job cancellation remain `remote_unsupported`. `alarm_set`
to `name@host` is refused (`remote_unsupported`, "alarms are local to a host");
other tools except `agent_delegate` and `agent_tell` refuse `@` targets too.

## Cross-host tells

`agent_tell` with `to: "name@host"` leaves an FYI note for an agent the other
host exports. The target resolves exactly as for `agent_delegate`
(`peers::asks::resolve`: outbound visibility, global/peer enable, peer status,
cached/refreshed directory) with the same error codes. Only `to` and inline
`text` (1 byte to 16 KiB) cross hosts: `wake:true`, `delegation`/`supersedes`
(revisions) and `attachments` are refused with `remote_unsupported` and a
precise detail before anything is resolved, sent or written.

The send is synchronous: `POST /peer/v1/tells` with
`{schemaVersion:1, tellId, requester:{cutexSessionId,name}, target, text}`.
`tellId` is `ptl-` + 32 hex, derived from the instance, caller and action ID,
so a retry after a lost response carries the same ID. The caller's action
receipt is kept only on success; a refusal writes nothing locally
(`status:"no_write"`). A transport failure reports `status:"response_uncertain"`:
retrying with the same `action_id` is safe. Success returns
`{ok, to:"name@host", name, cutexSessionId, host, peerId, remote:true, tellId,
deliveryKind:"note", recipientState, detail}`. A peer without the route
(404 `not_found`) yields `remote_unsupported`, "peer host does not support
cross-host tells (upgrade it)". `agent_list.otherHosts[].tells` shows whether
the peer's last hello declared `tells.v1`; the sender still tries when it is
false (the declaration may be stale).

The receiver authenticates the peer, requires global and peer enable, and
applies `validate_inbound` like an incoming ask: unknown/unexported targets →
403 `recipient_outside_boundary`, `inbound.accept:false` → 403
`peer_asks_disabled`. Tells count against `inbound.asksPerHour` separately
from asks (429 `peer_rate_limited`). Receipts in
`runtime/peers/tell-receipts.json` (created on first use, file-locked, 7 days,
at most 2000 per peer) dedupe by `(peer, tellId)`: the same request answers
from its receipt with `deduplicated:true`, other content is 409
`idempotency_conflict`. A failed local enqueue (503) stores no receipt.

Delivery is an ordinary passive note on the receiver's Agent Bus with sender
`name@<receiver's label for the peer>`, control type `cutex.peer.tell.v1`
(facts: peerId, host, remote sender ID/name, tellId), external message ID prefix
`ptell-` and no `fromCutexSessionId`: remote IDs never become local sessions
or bus identities. Only service callers can enqueue it (MCP-fenced agents
cannot set control records). Codex receives a service-source (`cutex-peers`)
ExternalInput of type `peer_message`; Claude gets the same source with view
`cutex.peer-message.v1`. The model text names the sender as `name@host`, says
it came from another host (paths refer to that host) and how to reply.
Both sides audit the request (`route:/peer/v1/tells`, `tellId`); bodies are
never logged. Capability `cross-host-tell` (`crossHostTell`).

Rollback: an older binary ignores `tell-receipts.json` and its hello lacks
`tells.v1`. Queued peer-tell notes not yet delivered stay in the bus store;
an older bridge cannot deliver their control record (Codex reports
`management control ingress is not enabled`; Claude skips them), so let them
deliver or clear them before switching back.

## Errors and desktop API

Peer errors use `{code,message,retryable,details}`; Management listener errors
use standard v2 `{error:{...}}`. Retry classification is local and never trusts
the remote `retryable` property. Remote codes pass a local whitelist; unknown
codes become `peer_request_failed` before audit, delivery reasons, or sync state
are written. A remote body cannot claim a local URL, transport, reader, or storage
failure to affect fallback or retry classification.

| Class | Handling |
| --- | --- |
| Connection failure, timeout, HTTP 5xx | Retryable; try addresses, then open circuit |
| Local URL/callback address rejected | Try next URL; if exhausted, `unreachable`, backoff and retain all queued work |
| Hello/feature mismatch, response decode error or oversized response | `versionMismatch`, retain outbox, probe hello |
| HTTP 400 `peer_protocol_error` rejecting an ask/op/event request | `peer_request_incompatible`; ordinary ask failure or event dead-letter; accepted stop becomes `outcomeUnknown` with a compensating close; peer stays usable |
| Identity mismatch | `identityChanged`, hold data and probe |
| Authentication 401 | `credentialRejected`; stop current data pass and probe with backoff |
| Rate limit 429 | Retryable; bounded `retryAfterSecs` (≤300) and backoff |
| Semantic policy 403, conflict 409, missing ask 404/410 | Fail open outgoing request or dead-letter incoming result as appropriate |
| Missing ask 404/410 for an expiry close | Acknowledge close: no remote execution remains |

Unknown and unexported targets both return `recipient_outside_boundary`.
Additional codes include `peer_credential_required`, `remote_credential_refused`,
`loopback_only`, `invalid_pairing_code`, `peer_already_active`, `pair_incomplete`,
`identity_changed`, `peer_unsupported`, `peer_protocol_error`, `peer_request_incompatible`,
`peer_response_too_large`, `peer_callback_forbidden`, `peer_url_insecure`,
`peer_unreachable`, `peer_rate_limited`, `peer_hops_exceeded`, `ask_id_conflict`,
`idempotency_conflict`, `ask_not_found`, `ask_gone` and `peer_user_unsupported`.

| Method and route | Contract |
| --- | --- |
| POST `/v2/peers/invites` | `{label?,targetUserId?}` → code, expiry, instance/host IDs, URLs |
| POST `/v2/peers` | `{url,code,label?,userId?,targetUserId?}` → `{peer}` |
| GET `/v2/peers` | `{peers:[...]}` with computed diagnostics |
| PATCH `/v2/peers/{id}` | partial `{label,urls,enabled,policy}` → `{peer}`; 1–8 URLs |
| GET `/v2/peers/{id}/inflight` | `{asks:[owner views]}` for open outgoing/incoming asks; use before delete confirmation |
| DELETE `/v2/peers/{id}` | best-effort remote unpair, local removal/ask cleanup → `{removed:true}` |
| GET `/v2/peers/{id}/agents` | refreshed directory or last cached directory on failure |
| GET `/v2/peers/{id}/audit?limit=N` | `{entries:[...]}`, newest first, current audit file only; default 100, maximum 1000 |
| POST `/peer/v1/pair` | code-authenticated exchange; reserved `targetUserId` selects the receiving binding |
| GET `/peer/v1/hello` | IDs, label, platform, protocol, features, build |
| POST `/peer/v1/confirm` | verify/activate pending peer; active retry is a local no-op |
| POST `/peer/v1/abort` | pending-only cleanup |
| POST `/peer/v1/unpair` | revoke relationship and run removal lifecycle |
| GET `/peer/v1/agents` | exported directory only |
| POST `/peer/v1/tells` | `WireTellV1` → `{tellId,messageId,deliveryKind,recipientState,target,deduplicated?}` |
| PUT `/peer/v1/asks/{askId}` | `WireCreateV1` → current `WireSnapshotV1` |
| GET `/peer/v1/asks/{askId}` | snapshot for owning peer |
| POST `/peer/v1/asks/{askId}/ops` | `{opId,op:stop\|close,outcome?,reason?}` → snapshot |
| POST `/peer/v1/events` | `{fromInstanceId,events:[{eventId,askId,revision,snapshot}]}` → `{acked:[eventId]}` |

Peer list entries include `id`, `peerId`, `instanceId` (the same instance ID),
`label`, `hostId`, `platform`, `userId`, `urls`, `enabled`, `protocol`, `state`,
`lastSeenAt`, `lastError`, `nextProbeAt`, `features:{declared,negotiated}`,
`outboxPending`, nullable `outboxOldestAt` and `outboxOldestAgeSecs`, `deadLetterCount`,
`inflight:{outbound,inbound}`, `policy` and `policyRevision`. States are
`pending`, `online`, `unreachable`, `credentialRejected`, `identityChanged`, `versionMismatch`,
`disabled`, `removedByPeer`. `lastError` is null or `{code,message,details}`;
missing features appear in `details.missingFeatures`.

`cutex/peers/changed {peerId,reason}` publishes to `cutex.host` for explicit and
worker-observed view changes, including outbox counts and stable `outboxOldestAt`. Worker observation is approximately once per second and borrows one cached ask-state projection for all peers. Advancing display age alone does not publish an event. Events go directly to the management host stream, never through persisted ask notices. Refresh the list to read current age.

Ask owner views/summaries add `hops`, `lateAnswer`, `resultUndeliverable` and
`remote:{peerId,label,targetName,syncState,lastError,interruptReason,remoteState}`.
Incoming requesters expose `kind:"peer"`, `peerId`, sanitized `name`/`label`,
remote `cutexSessionId` and `claimedCaps`. Private peer inbox/outbox data and
credentials are omitted. Schema exports are `peers`, `asks`, `cutex-methods-v2`.

`runtime/peers/peer-audit.jsonl` records direction/time, validated route, peer
identity/label, `actingUser`, ask ID, safe operation/event/tell IDs, status/code and byte count;
never bodies or credentials. Rotation retains one previous file after 4 MiB.
Cached directories live beside peer records.

## P1 user/grant migration boundary

P0.1 peer records reserve `userId`, fixed to the local `owner`; stored records
without the field decode as `owner`. Invites accept reserved `targetUserId`.
POST `/v2/peers` accepts `userId` for the local binding and `targetUserId` sent
to `/peer/v1/pair` for the receiving host's binding. All default to `owner`.
Explicit non-owner values return 400 `peer_user_unsupported`; malformed values
(including null/non-string) return 400 `invalid_request`, before code redemption,
network or peer-store writes. The CLI uses these defaults and adds no P0 user
selection flags.

This is an identity/migration attachment point only:
it grants no Management authority to `cxp1` and does not bypass exports,
`inbound.accept`, limits or the default review gate. Audit reserves
`actingUser` for the local user binding (null before a request is associated
with a peer); it is not added to agent-visible
messages or prompts.

Directory visibility and delivery permission are centralized in
`peers::policy::visible(peer, agent)` / `can_deliver(peer, agent)` so P1 can
replace the permission source with local user grants. P0.1 still enforces its
peer policy. Export discovery reads only the host's local directory: cached
foreign directories and mirror targets are never re-exported to a third peer.
Transitive local delegation still inherits the hop counter.

This release does **not** implement the full P1 user table, project/send grants,
user-bound client/device-token authorization, owner-pairing approval cards,
SSH pending assignment, route permission declarations or filtered SSE streams.
Existing Management client/device authorization remains unchanged. Peer-origin
agent delegation remains `requester.kind:"peer"`, shown as the remote agent
name and host; future human-user identity handling is separate.

P1 follow-up from the third review (not implemented by this follow-up): scope
remote error codes to their request route. The current shared whitelist still
accepts remote `identity_changed`, `peer_protocol_error` and `peer_unsupported`;
an authenticated peer can use them to put the local peer into an identity or
version hold until hello recovery. Accept remote `identity_changed` only on
pair/confirm routes and normalize it to `peer_request_failed` on data routes.
Also keep remote protocol/unsupported refusals on data routes from controlling
peer-wide compatibility state. Preserve the HTTP 400 `peer_protocol_error` →
`peer_request_incompatible` mutation rule, and preserve peer holds for locally
verified hello identity/features and response decoding errors. Acceptance needs
a route/status/code matrix plus worker assertions that a data refusal cannot
pause unrelated outboxes, while genuine hello/decode faults still hold them.

## Offline rollback and migration

Old binaries cannot safely read an unmodified peer ask store. New values include
`Requester::Peer`, peer holds (`peerUnreachable`, `peerIdentityChanged`,
`peerVersionMismatch`, `peerUnknown`), peer interruption reasons and remote sync/
outbox data. **An outgoing mirror alone can break old-version decoding** through
its top-level hold; incoming peer requesters are another incompatible case.
A daemon may start but return `asks_unavailable` when listing delegations.

Use the current peers-capable binary for one-time offline conversion:

1. While daemons still run, stop native agents executing incoming peer asks and
   resolve native launch claims. Live/unverifiable targets make strip refuse.
2. Stop both **Management and Agent Bus daemons for this instance**, preventing
   service-supervisor restarts throughout rollback.
3. Preserve the release's ordinary config/session/credential backup. With the
   same instance selected, run:

   ```text
   cutex human peers export-and-strip --out <new-private-export-directory>
   ```

   Omit `--out` for unique `runtime/peer-exports/<timestamp>-<uuid>`. An existing
   destination is refused. The command is offline: it never starts or contacts
   daemons or sends remote cancellation.
4. Inspect `README.txt`/`manifest.json`, switch binaries and start the old release
   with the converted store. Coordinate cancellation on other hosts separately
   if their remote work must stop too.

Conversion holds the exclusive migration lock, bus host lock, ask-store lock
and configured loopback ports. Current daemons hold a shared migration lock
and refuse to race it. All exports finish before the atomic store replacement:

| Export | Contents |
| --- | --- |
| `asks-v1.original.json` | Exact original ask-store bytes |
| `artifacts/<sha256>.blob` | Copies of every artifact referenced by the original store |
| `peer-records.json` | Original affected asks, inherited-hop descendants, and `cancelledCreates` cancellation-receipt metadata |
| `manifest.json` | Paths and each changed ask's direction, before/after state, `peerStripped` reason |
| `README.txt` | Human-readable conversion inventory |

Open peer asks become local terminal `execution:"interrupted", closed:"stopped"`;
active attempts use old `interruptReason:"stopped"`. `peerStripped` is diagnostic
text/delivery reference, not a new enum left for the old binary. Incoming peer
requesters become `owner`; provenance remains in the export. Peer holds,
routing, synchronization, late answers and hops are stripped. Affected leases,
questions and queued stop/cancel commands are cleaned up. Compatible terminal
outcomes are preserved. Ordinary local asks keep their state; local descendants
lose only peer ancestry data. `peerCancellations` is exported and removed from
the converted store.

The stripped store cannot synchronize these asks or expose removed provenance/
late answers. To restore with a peers-capable release, stop native work and
both daemons again, restore `asks-v1.original.json` and the artifact copies
together, then restart that release. This discards ask changes since strip;
conversion is not a replacement for the release's full rollback backup.

## Removed federation and debug hooks

Unauthenticated `/api/federation/agents` and `/api/federation/messages/send` are
removed and return 404 without federated dispatch. Bridgeboard bus-peer discovery,
cross-host bus forwarding, `agent list --all-hosts`, and send
`all_hosts`/`allHosts` fields are removed. Bus operations remain local;
cross-host delegation uses the authenticated peer protocol, with no legacy
federation endpoint or fallback.

Private debug instances can use `CUTEX_PEER_TEST_MAX_AGE_SECS=1..3600` to shorten
outbox age and `CUTEX_PEER_TEST_FEATURES=asks.v1,...` to advertise a feature subset.
They require an isolated instance and the private-home guard; release builds
ignore them. Commands and observed platform results are recorded separately in
[the latest review record](CROSS-HOST-VALIDATION-R226-R3.md); older evidence is in
[the validation archive](CROSS-HOST-VALIDATION.md).

Persisted `peers` settings and each peer policy layer (policy, inbound, outbound, exports, and assumed caps) tolerate future additive keys so older binaries can read the full config and peer store after rollback. Owner PATCH requests explicitly reject unknown keys at every policy layer, ambiguous export selectors, and simultaneous `asks` / `accept` aliases; existing type and range checks remain. Unauthenticated requests with no initialized peer store do not create `runtime/peers` merely for authentication or failure audit.

Additive-field tolerance does **not** cover new export selector variants.
P0.1's untagged `Export` understands only `agent`, `project` and `all`;
a selector such as `{"group":"..."}` cannot be decoded by this version and
makes the whole `peers.json` unreadable. Adding a selector therefore requires a
peer-store schema version bump with an explicit migration/rollback plan; it must
not be written under `cutex/peers/v1` as though it were an additive field.
An alternative for P1 is to first ship an Unknown variant that preserves the raw
selector while denying visibility/delivery; this follow-up uses the documented
schema boundary and does not add that variant.

Partial persisted `peers` settings now fill omitted fields from
`Settings::default()` (including `enabled:false`); a hand-written object such
as `{"enabled":true}` preserves other config settings. Explicit wrong types or
null fields remain errors, and owner PATCH keeps unknown-key and range checks.
