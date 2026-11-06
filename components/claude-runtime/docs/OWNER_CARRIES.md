# Owner-carry runtime contract

The selected runtime and host negotiate this capability. Existing pinned runtimes keep their own supported contract.

Bound subscription runtimes advertise top-level numeric `ownerInputCarries: 1`
in `initialize` and `claude/capabilities`, using ExternalInputService.capabilities.
Unbound/non-subscription runtimes omit it. Daemon negotiation must cache it by
runtime generation and must not substitute Boolean true for numeric 1.

`turn/start` and `turn/steer` accept optional `ownerCarries: [envelope, ...]`.
Each envelope is the canonical native ExternalInput v1/v2 shape: version,
ownerId, threadId, runtimeGeneration, message, semanticSha256, optional v2 view.
Admission fields are not envelope fields. Every binding, native digest, source,
view, and text limit uses the same validator as externalInput/submit. A batch is
at most 100 unique identities and 1 MiB serialized UTF-8 (per-text 65536 bytes,
per-view 16384 bytes, native depth/entry bounds unchanged).

Nonempty carries require a nonempty clientUserMessageId of at most 256 UTF-8
bytes, matching the existing owner client ID bounds. IDs of at most 200 UTF-8
bytes retain `owner-<clientUserMessageId>` / `steer-<clientUserMessageId>`.
For IDs of 201 through 256 UTF-8 bytes the bounded internal keys are
`owner-<sha256(full UTF-8 client ID)>` / `steer-<sha256(full UTF-8 client ID)>`,
using lowercase hexadecimal digests. Hashing never truncates the ID or changes
its visible `userMessage.clientId` or stage clientId. The daemon's source
inputId is unchanged; this key is local to the adapter inbox. All named
external identities remain their original envelope message.id. The full validated carrier payload is durably
hashed, so same-id/same-content retries return the original turn without
reclaiming notes. Changed content is -32010. An external identity already
submitted independently or attached to another owner is -32010; the daemon
must select carries before independently submitting them. No identity is moved
from a previous claimed, uncertain or interrupted delivery. Invalid envelope
and capacity/claim/conflict failures roll back the complete owner+carry batch.

One SQLite transaction enqueues the owner, stores each envelope and reserves
its note with metadata.ownerCarrier. Reserved notes are not pending passive
notes: ordinary turn starts and hooks cannot steal them. Idle/FIFO claim takes
only its carrier's notes into the owner's actual turn. SDK text blocks contain
external frames first, owner text LAST (then existing image attachments), so
only owner @path mentions expand and its visible user message stays plain.
For image-only owner input there is no typed owner text block: the SDK user
message sets `client_composed: true`, keeping every preceding external note
verbatim and disabling @path/slash interpretation. No empty/synthetic owner
text is added, and the timeline owner item contains only the original images.
This applies to direct start and the image-steer FIFO fallback.
Image steers retain existing FIFO fallback and carry the group to that turn.

Busy steer takes owner and reserved notes in the same authenticated PostToolUse
claim with no yield before the hook return. The hook context contains external
frames before that owner's steer frame. All external item identities, sources,
views and native envelopes remain intact. Items publish at offer, while receipts
require exact new native main-chain transcript proof of that hook's complete
returned context or the successful SDK Result of the actual consuming turn.
Native proof acknowledges only named members, not another hook's later group.
A receipt is stable and per external identity; owner-start/fallback receipt
turnId is that turn, early busy receipt turnId is the running turn.

Interrupted, failed, held or restarted carriers never release their notes as
free-standing pending input. Carried notes are excluded from the older passive
note settle heuristic (model output alone cannot prove persistence). Unknown
outcomes stay held and resubmission cannot rearm them. The unchanged runtime
owner autonomous-notification window holds the entire group; autonomous text,
tools and Result cannot be attributed to or acknowledge it.

Validation: tests/test_owner_carries.py plus existing ExternalInput, busy/steer,
autonomous isolation and native context receipt suites. Tests use a marked private HOME,
fake SDK/native transcript fixtures, and no live credentials or services.


