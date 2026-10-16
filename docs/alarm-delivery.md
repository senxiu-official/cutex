# Alarm delivery reliability

How alarms (`alarm_set`) reach the target agent: durably through the Agent Bus ledger,
exactly once, after the recipient's current turn.

## Delivery contract

- `AlarmSystem` ordinary messages enter the durable Bus ledger before enqueue.
  The scheduler's `alarm-…` message ID is also the Bus message ID. Replays verify
  canonical content, reuse the record across Bus/runtime restarts and report
  `deduplicated`. Replays of accepted or terminal messages never reopen delivery.
  `fromCutexSessionId` stays null.
- `bus_bridge::alarm_inter_agent_params` is the shared stable system projection.
  Model text is `Message Type: ALARM\nPayload:\n<reminder>`, as in ExternalInput.
  It neither invents a sending agent nor depends on a recipient's display name.
  A fixed SHA-256 vector guards this projection.
- Claude receives source `{kind: "service", id: "Cutex Alarm"}`, event type
  `alarm`, and `after-turn` delivery. `claude_ingress::envelope` accepts this
  typed system projection. Native envelopes remain frozen and r7 known-ID
  status probing remains in place. Local inbox acceptance is not context
  persistence. Native timeline items are not used as context receipts; native
  receipt recovery remains authoritative. Legacy v2 alarm items retain their
  existing timeline-based context receipt semantics.
- The scheduler commits `queued` after Bus acceptance. A consumer racing that
  commit waits on `AlarmAwaitingQueueCommit`; exact content/target/provenance
  conflicts are `InvalidAlarmProvenance`. Store I/O and decode errors remain
  retryable rather than discarding reminders.
- Permanent legacy/ExternalInput failures with no durable record write
  `live_message_terminal` to the existing audit log before ACK. This avoids
  manufacturing a canonical record from an invalid live envelope. Audit-write
  failure retains the message. Durable failures continue to be quarantined.
- `caller_owner` excludes retired records (including retained `retired_at`)
  and records with a known dead runtime PID. Unknown PID is not proof of death.
  Remaining ambiguity names the candidate session IDs and points to
  `cutex human doctor --check`. This facade correction
  only reaches agents using the new bundle.

## Interfaces and integration

No Management route, public schema export, capability or store version change.
`AgentBusSendResponse` accepts the existing camelCase `queueDurability`,
`deliveryState`, `requiredAckLevel` fields as aliases; the wire response is
unchanged. `alarm_list.receipt.delivery_state` now contains the acceptance state;
it is not a continuously refreshed Bus delivery-status query.

Claude v2 views add internal schema `cutex.alarm.v1`. The adapter already accepts
service sources, arbitrary event types and structured views; it is unchanged.
The existing Rust repository `canonical_message` accessor becomes public so the
binary's send path can preserve the original creation time on durable retries.

Alarm delivery remains after-turn. `claude_message_is_settled` also withdraws
audited historical live-only alarms before any native submission.

## Older alarms

Alarms that fired under older releases can say `queued` without a durable Bus
record. There is no migration or replay for them; set the reminder again if it
did not arrive.

## Tests

Rust unit tests cover the scheduler, stable retry / redrive and the
ExternalInput projection, the frozen envelope and the receipt behaviour; a
loopback smoke drove the Claude adapter with a fake SDK (idle and busy
after-turn delivery, digest parity, deduplication, the legacy input contract).
