# Human Jobs API (Management v2)

Root-only bridge from Management v2 to the local Job Service
(`components/job-service`, Linux-only, private Unix socket
`~/.cutex/runtime/job-service/v1/job-service.sock`). Cutex authenticates
with the service's root `api.token`; no caller grants are involved — these
routes act for the human owner (desktop / `cutex` CLI), never for agents.

Code: `src/management/v2/human_jobs.rs`. Capability: `job-service`
(`features.jobService`), advertised **only on Linux** (no runner elsewhere; on
other platforms every route answers 503 `job_service_unavailable`).

All routes require the Management root credential (or a client token, which is
root-equivalent). Bodies are strict JSON objects: unknown keys, wrong types or
missing fields give 400 `invalid_request`. Socket calls use 2 s connect /
read / write timeouts and a 1 MiB reply cap.

## Routes

| Route | Socket method | Body | 200 result |
|---|---|---|---|
| `POST /v2/job-service/management-query` | `humanList` | `{query?: string<=256, cursor?: "<epochSecs>:<jobId>"\|null, limit?: u64\|null}` | `{schema: "cutex/job-list/v1", data: [row], nextCursor}` |
| `GET /v2/job-service/capabilities` | `capabilities` | — | the service's result verbatim (`schema`, `allowedLaunchers`, `independentExecutionCwd`, `humanOperations`, `completionDelivery`) |
| `POST /v2/job-service/cancel` | `humanCancel` | `{jobId: string(1..=160), expectedRevision: u64}` | the job record (`jobId`, `revision`, `state`, …) |
| `POST /v2/job-service/output` | `humanReadOutput` | `{jobId: string(1..=160), stream: "stdout"\|"stderr", offset?: u64 (default 0), maxBytes?: u64 (clamped 1..=8192, default 8192)}` | `{jobId, stream, fromOffset, nextOffset, text, encoding: "utf-8-lossy", pageLimited, gap, omittedBytes, eof, truncated}` |

List rows: `jobId, actionId, sessionId, agentName?, state, command, createdAt,
updatedAt, exitCode, cwd, stdout, stderr, execution, delivery, outputReference,
revision`. `command` is argv joined with spaces (no quoting), at most 300 chars;
`null` for jobs submitted before the service persisted summaries. `agentName` is
added by cutex (presentation only). No environment values or output bodies.

Cancel is revision-guarded: pass the row's `revision`. A terminal job is returned
unchanged. Output paging: request again with `offset = nextOffset` until `eof`;
`gap`/`omittedBytes` report bytes dropped by the bounded on-disk retention.
`maxBytes` is further bounded by the service's `max_read_bytes`.

## Errors

| Job Service reply | HTTP | `code` | retryable |
|---|---|---|---|
| `not_found` | 404 | `job_not_found` | no |
| `conflict` (revision mismatch, no local process) | 409 | `job_conflict` | no |
| `invalid` + message `unknown method` (older deployed service) | 501 | `job_service_operation_unsupported` | no |
| `unauthorized` | 403 | `job_service_unauthorized` | no |
| anything else, connect / timeout / malformed reply, non-Linux | 503 | `job_service_unavailable` | yes |

`management-query` and `capabilities` keep answering every failure with 503
`job_service_unavailable`.

The human operations (`humanCancel`, `humanReadOutput`, `humanOperations` in
capabilities, `command` on list rows) need a Job Service built from
`components/job-service` at or after the commit that added them; an older
service yields 501 for cancel/output and list rows without `command`.
