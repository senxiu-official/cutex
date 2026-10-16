# `web_search` cutex tool (capability `profile-search-tool`)

## Why

Some providers refuse a Responses request that mixes the built-in `{"type":"web_search"}` tool
with function tools. Gemini behind an OpenAI-compatible relay answers 400 "Please enable
tool_config.include_server_side_tool_invocations to use Built-in tools with Function calling";
the relays do not forward that flag. Such profiles run with the native
`web_search = "disabled"` (Profile → Model → built-in web search), so their agents cannot search.

A probe against a Gemini relay showed that a request whose ONLY
tool is `{"type":"web_search"}` returns 200 with output items `message` + `web_search_call`, the
answer citing its source as a markdown link in the text (no `url_citation` annotation); the mixed
request still returns the 400.

## What

A per-profile switch `searchTool` (default off). On, agents on the profile get the cutex
MCP tool `web_search {query}`. Each call makes ONE separate, non-streaming Responses request to the
same profile's provider and model:

```json
{"model": "<wire model>", "input": [{"role": "user", "content": [{"type": "input_text", "text": "<query>"}]}],
 "tools": [{"type": "web_search"}], "stream": false, "store": false}
```

and returns `{ok, query, model, answer, citations: [{url, title, inline?}], sources, searches, note}`.
Citations are the `url_citation` annotations when present, else the markdown / bare links in the
answer text (`inline: true`). `sources` = `web_search_call.action.sources` URLs not already cited;
`searches` = the queries the provider ran. An SSE body (a relay that streams anyway) is read from
its `response.completed` event.

Generic only: no preset turns it on (Gemini included), nothing is keyed
on provider hosts, and parsing is provider-neutral.

- Model: the session's `model_defaults`, else the profile's `model`; a named model configuration
  sends its catalog `request_model`.
- Profile: the runtime binding's `launched_profile_id`, else the session's profile.
- Only Codex API-key profiles (`settings_api::search_tool_supported` = model discovery's rule);
  PATCH `searchTool: true` elsewhere is refused; Claude runtimes never get it.

## API

- `/v2/profiles` items: `searchTool` (effective bool); detail `editable.searchTool`.
- `PATCH /v2/profiles/{p}` `{"searchTool": true|false}` (no null). Stored top level in
  `accounts.json` as `search_tool` (omitted when false, so older binaries read the store and drop
  it on save).
- Listing is evaluated at launch (`tools/list` toolset / Claude descriptor): turning it on shows
  the tool when the agent next starts. The Bus checks the profile on every call, so turning it off
  applies at once (`search_tool_disabled`, nothing sent).

## Security and cost

- Agent Bus route `POST /api/web-search/v1`, fenced (thread + generation + per-agent credential);
  agents and the owner's quick chats (`owned_sessions`). Caller, profile and model come from the
  stores, never from arguments (`{"query"}` only, `deny_unknown_fields`).
- Key: read server-side from the profile auth file like model discovery (`catalog_api::api_key`);
  only in the request's Authorization header. Not in arguments, results, logs or environments;
  provider error text is scrubbed of it. The profile's effective proxy applies.
- Budget: one request per call, provider timeout 90 s (Bus wait 120 s), response ≤ 4 MiB, answer
  ≤ 32 000 chars, ≤ 50 citations, query ≤ 2000 chars.
- Rate limit: 20 calls per agent per sliding hour (attempts, failures included; Bus memory, a Bus
  restart starts a fresh window) → `rate_limited` with `retryAfterSeconds`.
- Usage: the provider's `usage` becomes a usage-ledger row (`usage:web-search:<uuid>`, profile id +
  name, provider, model, the calling turn when known), so `/v2/usage` and API quotas count it.
- Failure codes (all `status: no_write`): `invalid_arguments`, `search_tool_disabled`,
  `profile_unavailable`, `rate_limited`, `provider_error` (+ `providerStatus`, the provider's
  message), `provider_timeout`, `provider_unreachable`, `no_answer`.
- Results are untrusted web content; the result's `note` says so.
- Bus unreachable at `tools/list`: the fallback listing drops `web_search` (fail closed).

## Manual check (real provider)

1. Profile → Model → web search tool: on (keep the built-in web search off on a provider that
   refuses mixed tools). Save.
2. Restart (or create) an agent on that profile, ask it to "use web_search to find <something
   current> and cite the source".
3. Expect a `web_search` tool row whose result has `ok: true`, a non-empty `answer` and at least
   one `citations[].url`; the turn completes without a 400.
4. `GET /v2/usage` (Usage page) shows the extra tokens under the profile.
5. Off again → the agent's next `web_search` answers `search_tool_disabled`.
