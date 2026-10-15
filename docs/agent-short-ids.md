# Agent short IDs

Agent tools and `cutex human` accept complete IDs, exact case-insensitive
session names, and unique ID prefixes or suffixes. A kind prefix (`cutex.`,
`ask-`, `q-`, `doc-`, `art-`) limits the search to that kind. Hex comparison
ignores case and hyphens; surrounding whitespace is ignored.

Complete exact IDs take priority. Otherwise name and abbreviated ID matches
are combined: a name and an ID fragment naming different objects are ambiguous.
This resolves the handoff brief's conflicting “name first” and “name versus
prefix is ambiguous” requirements in favor of its explicit ambiguity rule.
An initial `…` or `...` after the optional kind prefix means suffix only.
Without it both directions are considered. Below the minimum hex length,
only an exact name can resolve.

```json
{
  "agent_short_ids": { "display": "short", "min_len": 6 }
}
```

These are the defaults. `min_len` is clamped to 6–64; if a complete identifier
is shorter than the configured minimum, it stays complete. `display: "full"`
restores full-ID display without disabling abbreviated input. This setting is
in `config.json`; it is not a `/v2/settings` field. Discovery advertises
`agent-short-ids` / `agentShortIds` next to the agent tool roles capability.

Session display uses a host-unique suffix, for example `cutex.…3f9a1c`.
UUIDv7 session prefixes tend to collide for sessions created near each other.
Asks, questions and documents currently use UUIDv4, and artifacts use SHA-256,
so these use unique prefixes such as `ask-1a2b3c`. All host IDs of that kind
participate in display uniqueness, including invisible and archived objects;
only authorized objects participate in resolution or ambiguity candidates.
Session display indexes are cached against the shared store snapshot.
Ask rendering reads only an atomically replaced display snapshot, without
taking a second AskStore lock; authorization and mutations keep locked reads.
Document prefix candidates use SQLite key ranges, not `LIKE`; suffix lookup
uses the exact trailing hex. No document blob is opened for resolution.

Errors use `id_not_found`, `id_ambiguous`, and `id_too_short`. Ambiguity includes
at most ten authorized candidates with their full ID, display ID, kind, and
optional name. Exact session IDs retain the existing boundary/archived errors.
An external agent with reply-only authority must still use the complete reply
address: names and short IDs do not expand its visible scope. The reply address
in message instructions therefore stays complete.

The fenced, read-only Bus route `POST /api/asks/v1/resolve-ids` accepts
`{"tool":"agent_tell","arguments":{"to":"cutex.…3f9a1c","text":"hello"}}`
and returns canonical arguments. The facade calls it before default action ID
derivation and before management request/receipt hashing. The Bus canonicalizes
delegation arguments again before `once()` hashes or stores them. A short-ID
retry and its equivalent complete-ID retry therefore identify the same action.
Alarm resolution failures are returned as structured errors.

The current facade and refreshed tool descriptions require a rebuilt runtime
package/restart. Existing facades immediately gain Bus-side delegation parsing,
but their old automatically derived action IDs remain the old facade's
responsibility; an explicit stable `action_id` is available for retries.

Storage, owner Management HTTP, events, audit records, and management receipts
retain canonical full IDs. Management HTTP routes still require full IDs.
Model-facing tool results, delegation headers, overlength pointers, revisions,
harness identity and newly frozen message sender headers use display IDs.
Raw message bodies and content are unchanged. The legacy message digest text
is deliberately unchanged; a queued record using literal baseline text is
covered by the existing delivery regression test. Already frozen external
input envelopes also retain their original bytes.
