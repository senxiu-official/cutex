# Owner notification, safety and approval pages (S10)

Management capability `owner-notify-approval-pages-v1` in `featureList`
(`features.ownerNotifyApprovalPagesV1 = true`) advertises this contract.
Clients must test the capability on each host before sending cursors. An older
host keeps its previous list behavior. Authentication and owner permissions are
unchanged.

| GET route | Filters | Page request | Response |
|---|---|---|---|
| `/v2/approvals` | `state=pending` (default) or `all` | `limit=1..500`, optional `cursor` | `{hostId, approvals, nextCursor}` |
| `/v2/owner-notifications` | `filter=all` (default), `pending`, `alerting`; optional `agentId` | `limit=1..500`, optional `cursor` | `{notifications, nextCursor?}` |
| `/v2/safety-checks` | `state=open` (default) or `all`; optional `agentId` | `limit=1..500`, optional `cursor` | `{blocks, nextCursor?}` |

Send `limit` explicitly on the first approval request to opt into paging.
Follow a nonempty `nextCursor` with the same route, host, filters and scope.
Approval pages use descending insertion order; notification and safety pages
use descending `(createdAt, notificationId)`. Limits may change between pages.
Each request reads a current WAL snapshot; a cursor does not freeze the entire
result set across concurrent writes. Newer records can be reconciled through
the existing events or a fresh first page.

All pages have a 256 KiB serialized-row budget in addition to the row limit.
They may contain fewer rows than requested. Following S0, a single record
larger than the byte budget is returned alone so that pagination always makes
progress without dropping the record. A missing or null `nextCursor` means the
end of the range. Notification/safety omit that field at the end, preserving
the existing shape of a complete small response.

Malformed cursors return HTTP 400 `invalid_cursor`. A different host instance,
store, filter, agent scope or epoch returns HTTP 409 `resync_required`; discard
the cursor and start the same view again. Never retry a read cursor by
replaying an owner reply, approval decision or tool action.

Rule audit also has an optional keyset mode:
`GET /v2/approval-rules/audit?page=keyset&limit=1..200&after=<nextCursor>`
returns the existing audit rows plus `nextCursor`, with a 256 KiB page budget.
The existing numeric audit cursor remains available for older clients.

## Staged client compatibility

Approval requests with neither `limit` nor `cursor` retain the complete legacy list. That path collects matching SQL
pages into one result and remains explicit compatibility debt: it is not the
bounded public-page path. Clients should request bounded pages. Native summary callers requesting all
pending approvals remain explicit reducers over matching active rows.

Notification and safety responses now expose continuation when their row or
byte budget ends a page. Clients must follow it for a complete historical view;
do not silently treat a byte-limited first page as the entire history. Client
adoption must retain the pending/reply/decision behavior on older hosts.

## Storage and exact-previous rollback

Canonical stores are private `runtime/owner-notify/owner-notify.sqlite3` and
`runtime/approvals/approvals.sqlite3`. The original JSON is retained after the
atomic initial import and is no longer authoritative. Import includes replies,
closure metadata, action receipts, pending notices, approval decisions, rules,
audit, confirmation nonces (including consumption), and Director overrides.

A documented per-store recovery workflow can run these exports only with every daemon/store writer stopped:

```powershell
cutex storage notification-store --rollback --daemon-stopped
cutex storage approval-store --rollback --daemon-stopped
```

These commands export only the named store; they do not reverse the whole host migration or establish that a public downgrade is supported. Public recovery follows the signed migration metadata and the matching offline backup.

Each exports current state to its exact-previous JSON layout, including queued
or leased notices, checks that its SQL head did not change, and arms a reimport
marker. A fresh new process after previous-build writes reimports the current
JSON atomically and changes its store epoch. Ordinary export does not arm
reimport; stale retained JSON must never overwrite later SQL writes. Neither
migration nor cursor resync replays an approval, runtime wake, or owner action.

Delivery workers select only IDs and payload lengths before loading a batch.
Notification claims retain FIFO and block behind a leased head, with at most 100 items and
256 KiB per batch; approval orphan staging and claims use at most 128 items and
256 KiB per phase. The same single oversized-row exception preserves delivery
progress. Pending bodies are not all read just to discover a batch boundary.
