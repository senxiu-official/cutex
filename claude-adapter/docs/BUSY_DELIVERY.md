# Claude busy wake and delegation result delivery

Task 07, based on Cutex `47cab2d`, extends the r8 external-input path.

Before accepting a new Agent Bus wake, Cutex uses the shared `wake_timing` /
`wake_busy_wait()` policy. A busy recipient holds an unfrozen message until the
configured deadline. An idle recipient receives it immediately. After the
deadline, a subscription adapter advertising `externalInputDeliveries: soon`
accepts a frozen native `soon` envelope. Its digest never changes on retries.
Existing r7 inbox identities and existing frozen r8 envelopes keep their route;
r7 and r8 adapters without the new capability retain after-turn delivery.

The adapter enqueues `soon` in the existing durable FIFO and adds a `busy_inputs`
record in the same transaction. A main-agent PostToolUse callback must match the
native session, current bridge binding and a PreToolUse-admitted tool id. Child,
unknown, revoked and repeated callbacks cannot claim a wake. Claiming atomically
removes the input from the FIFO and records its consuming turn. The queued
receipt's reserved `turnId` remains stable; the legacy result receipt exposes
`contextTurnId` separately. Source, framing, asks, attempt and structured view
remain external data. No owner `turn/start` or `turn/steer` route is used.
The claim and hook return run without yielding; a turn end that wins before the
claim leaves the input untouched for its FIFO fallback. Timeline notifications
for an injected wake are published with the later context acknowledgement.

A hook offer and ordinary item output do **not** count as context persistence.
The existing r8 successful SDK Result remains the acknowledgement boundary.
Native inputs then receive their ordinary stable context receipt; legacy result
status sets `contextPersisted`. Interrupted, crashed or unreturned offers stay
unknown/held, with no automatic requeue. A restart turns unfinished `injecting`
claims into `uncertain`. An input not claimed by a hook still starts exactly one
ordinary FIFO turn after the current turn. This includes long turns with no
tools. Receipt retention uses the consuming turn; reverting that turn removes
its proof of current context while retaining the identity and audit receipt.

Delegation results retain the metadata-bearing `claude/input/submit` protocol.
Only a controller-submitted ANSWER/QUESTION with service source `cutex-asks`,
ask ids, the stable `cutex-ask-soon-` identity, and `admission: next_step` may use
this path. ASK admission, owner input, Plan, pause and delegation authorization
remain unchanged. `input.durableNextStepResults` gates Cutex's use of this path.
On a runtime generation change, the asks scheduler queries the same durable
message id on the new generation. Timeout, missing inbox and unknown status
never authorize generating a new result identity.

`cutex-wake-read` keeps `idle_turn`, `turn_end` and `early_next_step`; delegation
results retain their existing log paths. The consuming turn determines whether
a queued soon input actually took the early path or fell back to a new turn.
An early receipt also feeds `cutex-wake-counterfactual`. As in r8, the read log
timestamp reflects confirmed persistence, which can be later than the hook
offer. Timing observations affect diagnostics only, never delivery decisions.

The new SQLite table is an additive schema-2 migration. Neither manifests nor
release versions are changed in this task. Before deployment, integrate the
AlarmSystem route from task 01 and the account/runtime fixes from task 04. When
integrating the separate owner-steer branch, preserve the terminal ordering:
close tool-context admission before awaiting terminal receipt publication.

The opt-in native probe uses the installed SDK's bundled CLI explicitly, a
private home and a fake Anthropic provider on loopback port 25970. It uses no
real credentials and prints only marker evidence. With a Read tool, two model
requests produce one turn and the second request contains the wake marker.
Without tools, two requests produce two turns and exactly one fallback delivery.
This verifies Windows SDK/CLI scheduling against a simulated model, not real
model behavior or the complete desktop/management deployment.

```powershell
$env:CUTEX_BUSY_DELIVERY_NATIVE_SMOKE = '1'
python -m pytest -q -s tests/test_busy_delivery_native.py
```

Run with the repository's required private HOME / CUTEX_TEST_PRIVATE_HOME and
marker. The probe is skipped by default, and refuses an occupied port by failing
the bind. It closes only the runtime and provider it starts.

## Validation (Windows, 2026-10-11)

| Check | Baseline `47cab2d` | Task 07 |
| --- | --- | --- |
| Adapter pytest | 380 passed, 6 skipped | 394 passed, 10 skipped |
| Rust library | 1534 passed, 10 failed, 13 ignored | 1535 passed, same 10 failed, 13 ignored |
| Rust cutex binary | 714 passed, 14 failed, 5 ignored | 717 passed, 13 failed, 5 ignored |
| Rust integration | 3 passed, 2 failed, 2 ignored | Same |
| Opt-in SDK/CLI loopback | Not present | 4 passed |

The new Rust timing, metadata/receipt/digest and durable restart tests pass.
The complete Rust suites remain red on pre-existing Windows path, manifest,
notification, profile and TUI cases. The parallel binary run additionally hit
`ordinary_redrive_targets_only_current_durable_runtime_generation` with a
removed temporary stream-state directory. Serial rerun of its unchanged module
passed that case (13 passed, 3 failed); all three remaining failures also appear
in the baseline. This is not a claim that the full suite is green.

Both revisions used `--locked` debug builds with private marked homes and the
handoff's two prescribed skips. Baseline `--tests` stopped at the existing lib
failures, so the integration targets and cutex-mcp were also run explicitly with
`--no-fail-fast`; the final task run used `--lib --bins --tests --no-fail-fast`.
The native probe used SDK `0.2.164`, CLI `2.1.292`, SHA-256
`eb95bb65955f8b1702e800815f9a2c0388a5de0f196c354c7ee1dd5cb9a2ba23`.
All four wake/result × tool/no-tool cases verified marker visibility in the
second provider request, confirmed context receipts, and one or two turns as
appropriate. No Linux, real model, full management daemon or desktop acceptance
is claimed.
