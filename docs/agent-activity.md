# Agent Activity Projection

## Objective

Record useful agent activity independently from Agent Bus liveness and durable
session configuration. For each local managed session, Cutex exposes the most
recent agent output, completed agent reply, completed turn, and explicit file
change timestamps. The wide Cutex selector shows the latest output without
changing narrow layouts.

## Boundaries

- Agent Bus `last_seen` remains a heartbeat and is not activity.
- `cutex-sessions.json` and its durable revision are not changed by activity.
- Native app-server events remain the source of truth. No native cute-codex
  protocol change is required.
- Explicit `fileChange` items are recorded. Shell side effects that are not
  reported by the app-server cannot be attributed to an agent.
- Activity is host-local. Federated presentation requires a later typed host
  aggregation contract.
- Activity is a derived cache. An unreadable projection warns once and projects
  null activity instead of blocking Management session/lifecycle operations.
- An open Cutex TUI rereads only the local activity projection once per second;
  it does not poll Agent Bus, cute-alden, or Management HTTP for this column.

## Storage And Projection

- Store activity in a private, atomically replaced Management v2 projection
  file keyed by durable `cutex_session_id`.
- Update the in-process latest-output accumulator for every
  `item/agentMessage/delta` notification.
- Persist the first output delta, bounded periodic output checkpoints, and all
  terminal boundaries. Always flush pending output on agent-message or turn
  completion.
- Merge timestamps monotonically so stale runtime generations or concurrent
  writers cannot move activity backwards. Persist the latest runtime
  generation and reject events from older generations.
- Expose nullable timestamps under the Management v2 session `activity`
  object together with the source runtime generation. Missing state is valid
  for legacy, non-app-server, and inactive sessions.
- Exclude activity from the session fingerprint so activity updates cannot
  advance any durable or projection session revision.

## Current Stage 1 Plan

| Task | Status | Notes |
| --- | --- | --- |
| Reconcile Retire and TUI source lineages | Completed | Activity branch includes `8078308` and `90192f1`. |
| Add typed activity model and private projection store | Completed | Commits `29ba1d4`, `5ecb7a1`, and `c2ec4b4`; missing state, exact JSON, generation fencing, monotonic timestamps, private atomic writes, and bounded retry tests pass. |
| Consume native output, turn, and file-change events | Completed | Commits `29ba1d4`, `ef35596`, and `5ecb7a1`; deltas are rate bounded and terminal events flush. |
| Expose Management v2 session activity | Completed | Commits `ef35596` and `cf18aef`; active and retired resources expose nullable activity without changing or blocking durable session operations. |
| Show latest output in responsive TUI | Completed | Commit `4a55289` plus the post-integration live refresh; wide and extra-wide layouts show `LAST OUTPUT`, refresh locally once per second, narrow layout is unchanged, and system rows show `-`. |
| Final validation and integration | Completed | The in-place activity refresh passed all final gates and shared `main` was fast-forwarded through source candidate `0615b3b`. |

## Validation

- Focused activity projection unit tests.
- Management v2 session projection and server tests.
- Cutex TUI responsive rendering tests.
- Changed-file formatting and `git diff --check` per stage.
- Full locked test suite and baseline-allowlisted Clippy once for the final
  candidate.

## Focused Evidence

- `cargo test management::v2::activity --locked`: 8 passed, including a
  lower-runtime-generation rejection fixture.
- `cargo test management::v2::session --locked`: 7 passed.
- `cargo test management::v2::server --locked`: 19 passed.
- `cargo test cli_app::session_tui --locked`: 126 passed.
- `cargo clippy --all-targets --locked --message-format short`: exact existing
  baseline (`21/24` library and `4/26` binary), with no new activity lint.
- `cargo test -q --locked`: 258 library, 331 binary, and 0 doc failures.
- `cargo build --release --locked`: passed. `target/release/cutex` reports
  `cutex 0.1.0`; SHA-256 is
  `9089607046a7e855c61b2e2fcf199903b6604e8ab644f5f8bbfa7e0e5baed861`.
- `cargo check --locked --target x86_64-pc-windows-gnu`: passed with only the
  existing conditional unused-import warnings.
- `git diff --check`: passed. No deployment, restart, live-session mutation,
  Windows release build, or Task Scheduler action occurred.

## Integration Result

The source-only fast-forward through `0615b3b` preserved Retire/Restore
`8078308` and TUI follow-up `90192f1`. Deployment and manual live-event
acceptance remain separate owner-approved operations.
