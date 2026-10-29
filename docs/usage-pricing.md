# Usage cost estimates and profile provider categories

Branch `feat/usage-pricing` (off `feat/desktop-api` de323a3). Capabilities:
`profile-provider-category-v2`, `usage-price-catalog`, `usage-currency`.

Every amount here is an **estimate** from published per-token prices, never a bill.

## 1. Profile provider category v2 (`profile-provider-category-v2`)

Values: `kimi`, `glm` (bigmodel.cn), `zai` (Z.ai international), `claude`, `deepseek`,
`codex` (formerly `chatgpt`), `other`. The legacy value `chatgpt` is read (and accepted on
PATCH) as `codex`.

`GET /v2/profiles[/{p}]`, per profile:

| Field | Meaning |
|---|---|
| `appearance.providerCategory` | owner's explicit category, `null` = unset |
| `appearance.effectiveProviderCategory` | explicit, else inferred |
| `effectiveProviderCategory` (profile root) | same value as `appearance.effectiveProviderCategory` |
| `detail.providerCategoryOptions` | the seven values |
| `detail.editable.providerCategory` | `true` |

The older `provider` field (legacy display category `chatgpt|deepseek|glm|claude|other`) is unchanged.

`PATCH /v2/profiles/{p}` `{"providerCategory": "<value>" | null}`; `null` returns to inference.

Inference when unset (in order): ChatGPT login (`authKind = chatgpt`) → `codex`;
Claude runtime (`cliKind = claude`) → `claude`; API base URL host (domain or subdomain):
`bigmodel.cn` → glm, `z.ai` → zai, `moonshot.cn` / `moonshot.ai` / `kimi.com` → kimi,
`deepseek.com` → deepseek, `anthropic.com` → claude, `openai.com` / `chatgpt.com` → codex;
then the configured `model_provider` name (`glm`/`zhipu*` → glm, `zai`/`z.ai` → zai,
`kimi`/`moonshot*` → kimi, `deepseek`, `anthropic`/`claude`, `openai`); else `other`.
(`openai.com` → codex and the `model_provider` fallback go beyond the original list: an
OpenAI API-key profile is Codex, and some profiles have no explicit base URL.)

### Storage and rollback

`accounts.json` (profile store):
- `options.provider` keeps the **legacy** enum and is always written with the nearest legacy
  value: codex → `chatgpt`, glm / zai → `glm`, kimi / other → `other`, deepseek, claude.
- The exact v2 value goes into a new account-level field `provider_category`.

r216 reads `options` with a strict enum inside a `deny_unknown_fields` struct; a value such
as `"codex"` or `"kimi"` there would make the whole profile store unreadable for r216, so
v2 values are never written there. `StoredAccount` itself has no `deny_unknown_fields`:
r216 ignores `provider_category` and **drops it on its next profile-store save**. After a
rollback r216 therefore shows the legacy value (zai as GLM, kimi as Other, codex as ChatGPT).
After rolling forward again, a dropped v2 value reads as the legacy one (zai → glm, kimi → other).

`provider_category` counts only while its legacy projection equals `options.provider`. If an
older binary or the TUI legacy picker changed `options.provider`, the legacy value wins (the
TUI also clears `provider_category` when it changes the provider).

## 2. Price library (`usage-price-catalog`)

### Settings: `config.json` `usage` (`/v2/settings` `usage`)

| Key (camelCase in `config.json` and the API) | Default | Values |
|---|---|---|
| `priceSource` | `litellm` | `litellm` \| `models.dev` \| `manual` (manual table only) |
| `catalogRefreshHours` | `24` | 1 – 720 |
| `catalogUrl` | absent | http(s) mirror of the selected library (same format); **loopback only** (403 `loopback_only` otherwise: the daemon fetches it) |
| `displayCurrency` | `USD` | see §3 |
| `exchangeRates` | `{"AUD": 1.52, "CNY": 7.12}` | see §3 |
| `exchangeRateAutoFetch` | `false` | see §3 |

`GET /v2/settings` returns `usage` (effective values), `usageDefault`, `usageOptions`
(`priceSource`, `displayCurrency` lists, `catalogRefreshHours` range).
`PATCH /v2/settings {"usage": {...partial...}}` is strict (unknown keys / values → 400).
The block is omitted from `config.json` while it equals the defaults. Unknown values written
by a newer cutex read as the defaults instead of failing (`UsageSettings::effective`).

Libraries:
- `litellm`: `https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json`
  (per-token USD; only `mode` chat / responses / completion or no mode; `*_above_<N>k_tokens`
  → long-context rates).
- `models.dev`: `https://models.dev/api.json` (per-1M USD; keys `provider/model`; preferred
  providers also answer the bare model id; `context_over_200k` → long-context rates).

### Fetching and cache

- Daemon thread `cutex-usage-prices` (management serve): first pass 2 min after start, then
  every 30 min it reads the settings and the cache metadata and fetches only what is due:
  the selected library after `catalogRefreshHours` (failed attempts retry after 1 h), the
  exchange rates daily when auto-fetch is on. Nothing is fetched for `priceSource = manual`.
- Conditional requests (`If-None-Match` / `If-Modified-Since` from the cached `ETag` /
  `Last-Modified`); 304 only updates `checkedAt`.
- Cache: `<cutex home>/runtime/usage-pricing/catalog-litellm.json`,
  `catalog-models.dev.json` (normalized: per-1M USD, lowercased keys; `fetchedAt`,
  `checkedAt`, `etag`, `lastModified`, `lastError`, `models`), `fx.json`.
- A failed fetch (offline, HTTP error, unparsable / empty body) keeps the previous models and
  records `lastError`. Reports never touch the network; they read the cache (memoized, re-parsed
  only when the file changes).
- `POST /v2/usage/prices/refresh` forces a fetch now (library + rates if auto-fetch is on)
  and returns the prices resource with `refreshErrors`.

### Estimate per sample (manual entries win)

0. Pricing model map (`modelMap`, §2a): a mapped sample is priced as its target model through
   steps 1 – 4 below.
1. Manual table entry (keys or aliases).
2. Library entry (unless `priceSource = manual`), with the OpenAI service-tier multipliers
   cutex already knows (priority / fast ×2 or ×2.5, ultrafast); an unknown non-default tier
   leaves the sample unpriced. Long-context rates apply when the request's input exceeds the
   library threshold.
3. cutex's built-in API rate cards (`builtin`, the `apiEquivalentUsd` cards; unless
   `priceSource = manual`).
4. Otherwise unpriced.

Cached and cache-write input use their own rates when present, else the input rate.

Model-name normalization (manual and library): ASCII-lowercase, drop a `[...]` suffix
(`opus[1m]`), try with and without a `vendor/` prefix (`deepseek/deepseek-chat`), with and
without a date suffix (`-20250929`, `-2025-09-29`). Library keys are tried as
`<prefix>/<name>` for the sample's provider name and its family's library prefixes
(codex → `openai`; claude → `anthropic`; deepseek; glm → `zhipuai`, `zai`; zai → `zai`,
`zhipuai`; kimi → `moonshot`, `moonshotai`, `moonshotai-cn`), then bare. When the sample's
provider name is not a known family, the profile's effective provider category decides.
Manual lookup: `<provider>/<model>`, then each normalized form as `<provider>/<form>` and bare.

### `GET /v2/usage` additions

Rows and `totals` (all present whenever pricing is available):
- `estimatedCost` — priced samples, in `currency` (was: manual table only, table currency).
- `unpricedTokens`, `unpricedRequests`.
- `priceSources` — sorted subset of `manual`, `litellm`, `models.dev`, `builtin`, plus
  `unpriced` when some samples had no price.
- `apiEquivalentUsd` — unchanged (built-in cards, USD).

Top level:
- `currency` — display currency (always present now).
- `unpricedModels` — `[{provider, model, pricedAs?, requests, tokens}]` in the range
  (`pricedAs`: §2a).
- `pricing` — `{priceSource, displayCurrency, exchangeRates: {USD: 1, AUD, CNY},
  exchangeRatesSource: "config" | "auto", exchangeRatesAutoFetch, exchangeRatesFetchedAt,
  exchangeRatesDate, exchangeRatesError, catalog: {source, url, fetchedAt, checkedAt,
  lastError, modelCount} | null}`.
- `warnings` — e.g. library not downloaded yet / unavailable, legacy price file changed.

### `GET|PUT /v2/usage/prices`

Response: `{contractVersion: 2, currency, models, currencies: ["USD","AUD","CNY"], pricing
(as above), library, refreshErrors, warnings}`.
- `GET ?library=<substring>` adds `library: {source, currency: "USD", query, models: {key:
  {input, cachedInput?, cacheWriteInput?, output, longContext?}}, total, truncated}` (first
  200 matches of the cached library; read-only — override by adding a manual entry).
- `PUT {currency, models}` as before; entries may add `cacheWriteInput`, `currency`
  (USD/AUD/CNY, default: table currency) and `aliases` (≤ 32, unique with keys ignoring case).
  The table currency must now be USD, AUD or CNY.
- `modelMap` (GET and PUT): §2a.

### Manual table storage and rollback

- `<cutex home>/usage-prices-v2.json` — authoritative `{version: 2, currency, models,
  legacySha256}`.
- `<cutex home>/usage-prices.json` — rewritten on every save in the **r216 format**
  (`{currency, models: {k: {input, cachedInput?, output}}}`): entries converted to the table
  currency at the rates in effect, aliases expanded to extra keys (≤ 256, case-unique),
  `cacheWriteInput` dropped. r216 reads it unchanged (its `deny_unknown_fields` structs would
  reject the v2 entry fields, hence the separate file).
- If `usage-prices.json` no longer matches `legacySha256` (an older cutex or a hand edit
  changed it), it is imported and wins until the next save; reports / GET carry a warning.

## 2a. Pricing model map (`usage-model-map`)

"Price model X as model Y": `modelMap: {"<from>": "<to>"}` next to `models` in
`GET|PUT /v2/usage/prices`. The target's price comes from wherever Y has one (manual entry,
the selected library, the built-in cards), so the owner never retypes a library price for a
relay's renamed model (e.g. `gemini-3.8-flash-tiere` → `gemini-3.8-flash`) or for the bare
Claude Code aliases some old claude-r5 rows recorded (`sonnet` → `claude-sonnet-4-5`).
Generic owner-maintained data; nothing is provider-specific.

- Matching: the sample's pricing model (`requestModel`, else `model`), then its `model`, each
  as `<provider>/<model>` and through the normalized forms of §2 (lowercase, no `[...]`
  suffix, with / without `vendor/` prefix and date suffix), each `<provider>/<form>` and bare.
  Keys ignore ASCII case. A match prices the sample as `to` (normal lookup of `to`, no
  `requestModel`); one hop only.
- A mapped sample is never priced under its own name: if `to` has no price it is unpriced and
  `unpricedModels[]` names `pricedAs: "<to>"`.
- Validation (PUT, 400): ≤ 256 entries; names like model keys (trimmed, non-empty, ≤ 160 bytes,
  no control characters); `from` unique ignoring case; `from` ≠ `to`; a `from` must not be a
  manual key or alias (that price would be unreachable); a `to` must not be another `from`
  (no chains or cycles).
- `PUT` without a `modelMap` key (clients without `usage-model-map`) keeps the stored map,
  minus `from` names the new table now prices directly. `PUT {"modelMap": {}}` clears it.
- Reports: rows keep the original model name; rows and `totals` gain `pricedAs: [<to>, ...]`
  (sorted, absent when no sample of the row was mapped). `apiEquivalentUsd` stays the
  built-in estimate of the original model.

### Storage and rollback

The map is stored in `usage-prices-v2.json` (`modelMap`, omitted while empty), not in
`config.json` `usage`:
- it belongs to the price table and is saved by the same `PUT` (one atomic table write);
- r216 and older never write `usage-prices-v2.json`, while they drop the unknown parts of
  `config.json` on *any* config save;
- the r216 projection `usage-prices.json` also gets every `from` whose target has a manual
  price as an extra key (like aliases, within the 256 cap), so r216 keeps pricing those
  mapped names. Library / built-in targets have no r216 equivalent (r216 shows them unpriced).
- r217 – this branch's predecessors read the v2 file (no `deny_unknown_fields` there) and
  ignore `modelMap`; their next price-table save drops it. Roll-forward then shows no map
  (re-enter it). If an older binary changed only `usage-prices.json` (`legacy-changed`), the
  imported table keeps the v2 file's map, minus names the imported table prices directly
  (these are the projected manual-target names: same price).

No conversion either way.

## 3. Currency (`usage-currency`)

- Display currency `usage.displayCurrency`: USD (default), AUD, CNY. Libraries and built-in
  cards are USD; manual entries in their own currency; everything converts through USD.
- `usage.exchangeRates`: units per 1 USD, owner-entered; defaults AUD 1.52, CNY 7.12
  (approximate, October 2026). `USD` may only be given as 1.
- `usage.exchangeRateAutoFetch` (default **off**): daily fetch of
  `https://api.frankfurter.dev/v1/latest?base=USD&symbols=AUD,CNY` (Frankfurter, ECB
  reference rates, no key), cached in `fx.json`. While on, the last successful fetch is used
  (`exchangeRatesSource: "auto"`); before the first success or when off, the configured rates.
- Reports return `currency` and `pricing.exchangeRates` / `exchangeRatesSource`.

## 4. Rollback summary (this branch → r216)

| Data | r216 behaviour |
|---|---|
| `accounts.json` `options.provider` | readable (legacy values only) |
| `accounts.json` `provider_category` | ignored, dropped on r216's next profile-store save |
| `config.json` `usage` block | ignored, dropped on r216's next config save (like `owner_notify` for r210) |
| `usage-prices.json` | r216 format; prices converted, aliases as keys |
| `usage-prices-v2.json` | ignored; kept. After roll-forward it is used again unless r216 changed `usage-prices.json` |
| `usage-prices-v2.json` `modelMap` | ignored by r216 (names mapped to manual entries are priced through the projection); dropped by r217+ binaries without `usage-model-map` on their next price save (§2a) |
| `runtime/usage-pricing/` | ignored; can be deleted any time (re-fetched) |

No conversion step is needed either way.

## 5. Not implemented: actual spend

Balance snapshots (`quota-history` already records `balance` for balance gauges) and
subscription fees are out of scope. Nothing needs reserving now: a later "actual spend"
can live in its own file / settings block (e.g. `usage.subscriptions[]` with
`{profile, amount, currency, period}`) and reuse `Currency` / `ExchangeRates`; reports can add
an `actualSpend` object without changing `estimatedCost`.

## 6. Extra usage sources (`usage-external-sources`)

Token usage of the owner's own Claude Code and plain Codex CLI runs (outside cutex), read
from their transcript directories, and of Zhipu ZCode (kind `zcode`, capability
`usage-source-zcode`), read from its local usage database, shown in `GET /v2/usage` next to
cutex's rows. Code: `src/management/v2/usage/external.rs` (+ `external_zcode.rs`).

### Configuration and routes (root only)

`config.json` `usage.extraSources[]`: `{id, kind: "claude-code"|"codex"|"zcode", path, label,
billing: "subscription"|"api", enabled}`. Edited only through these routes (not part of the
`/v2/settings` `usage` view or patch):

| Route | |
|---|---|
| `GET /v2/usage/sources` | `{sources: [{id, kind, path, label, billing, enabled, supported, status}], defaults: [{kind, path, exists}], kinds, billing, scanIntervalSecs}`; `status` = `{lastScanAt, lastScanMs, files, rows, lastRows, errors, lastError, scanning}` |
| `POST /v2/usage/sources` | strict `{kind, path, label?, billing? (default subscription), enabled? (default true), id?}`; `~/` expands; the path must be an existing directory. 201 + the source; a first scan starts in the background |
| `GET\|PATCH /v2/usage/sources/{id}` | PATCH strict `{path?, label?, billing?, enabled?}` |
| `DELETE /v2/usage/sources/{id}[?purge=true]` | removes the entry; `purge` also deletes its rows (`{deleted, purged}`) |
| `POST /v2/usage/sources/{id}/rescan` | 202 `{…source, started}` (false: a scan is already running); 409 `usage_source_disabled` |

Errors: 409 `usage_source_overlaps_cutex` (`details.cutexPath`) when the path equals, is
inside or contains a directory cutex records itself (the cutex home with its profiles,
`codex-home` and `claude-accounts/*`, and the Claude deployment's `claude_home`); 409
`usage_source_overlaps_source` for another source's directory; 409 `usage_source_exists`
for a taken `id`; 404 `usage_source_not_found`.

### Import

- Rows: `runtime/management-v2/usage-ledger-external/<id>/YYYY-MM.jsonl` (same `UsageSample`
  format plus `source {id, kind, label, billing, agentName, project}`), merged by the reader
  after cutex's own rows; an entry id cutex has wins. Scan state (byte offset of the last
  complete line per file, keyed by relative path and size / mtime / inode) is
  `<same dir>/scan-state.json`, so `purge` = removing that directory.
- Scanned by management serve 30 s after start and then every 5 minutes, and on
  create / PATCH / rescan; never inside a `GET /v2/usage` request. Only
  `projects/**/*.jsonl` (Claude Code) and `sessions/**/rollout-*.jsonl` +
  `archived_sessions/rollout-*.jsonl` (Codex) are read; links are not followed; credentials,
  auth and settings files are never opened; line contents are never logged.
- ZCode (`zcode`, path = the ZCode home, default `~/.zcode`): only
  `<path>/cli/db/db.sqlite` (SQLite, WAL) is opened, for one SELECT per scan, never writing or
  checkpointing. While ZCode runs (its `-wal` / `-shm` exist) the connection is read-only with
  normal locking (`SQLITE_OPEN_READ_ONLY`, `query_only`, 2 s busy timeout) and committed rows
  still in ZCode's `-wal` are seen. While ZCode is not running (no `-wal` / `-shm`) a read-only
  connection would create `db.sqlite-wal` / `-shm` and leave them behind, so the file is read
  as `file:…?mode=ro&immutable=1` instead, which creates nothing; that is only safe without a
  writer, so the main file's size / mtime and the absence of `-wal` / `-shm` are compared before
  and after, and a changed result is discarded and read again the locked way. A linked path is refused. Only these `model_usage` columns are read: `id`,
  `session_id`, `turn_id`, `logical_request_id`, `provider_id`, `model_id`, `completed_at` and
  the five token counts; prompts, messages, error texts, `raw_usage_json`, other tables and
  `provider_config.json` never are. One row per `id` with `status = 'completed'` (running /
  error / cancelled rows are skipped; a running row counts once it completes). Incremental by
  a `completed_at` watermark (epoch milliseconds) with a 15 min look-back; ids imported inside
  the window are kept in the scan state (`zcode {watermark, recent}`), so a late commit is
  still imported and nothing is appended twice. Mapping: `inputTokens` = `input_tokens`
  (ZCode's input already includes cache reads; when it is smaller than cache read + cache
  creation, both are added), `cachedInputTokens` = `cache_read_input_tokens`,
  `cacheWriteInputTokens` = `cache_creation_input_tokens`, `outputTokens` = `output_tokens`,
  `reasoningOutputTokens` = `reasoning_tokens`, total = input + output; model = `model_id`
  (e.g. `GLM-5.3`), provider = `provider_id` (e.g. `account:zai-individual-coding-plan`),
  time = `completed_at`, thread = `session_id`, turn = `turn_id` (else
  `logical_request_id`, else `id`). Entry id `usage:ext:zcode:<id>`. No working directory is
  read, so all rows are one agent `ZCode` (`ext:<source>:zcode`) without a project.
- Claude Code: assistant lines with non-zero `message.usage`, one row per `message.id` (the
  last streamed line; settled when another message or a user line follows, or when it is at
  least 60 s old). Mapping as the Claude adapter: `inputTokens` = input + cache creation +
  cache read, `cachedInputTokens` = cache read, `cacheWriteInputTokens` = cache creation;
  provider `anthropic`; turn = the prompt's `promptId`. Entry id `usage:ext:claude:<message.id>`.
- Codex: `token_usage_record` rows (`usage:ext:codex:<response_id>`); a rollout without them
  falls back to `event_msg` `token_count` `last_token_usage`, counted only when
  `total_token_usage.total_tokens` moves (`usage:ext:codex:<session>:<total>`). Usage lines
  younger than 60 s wait for the next scan. Sessions whose id is already a `threadId` in
  cutex's ledger, or whose originator starts with `cutex`, are skipped.
- Agent `ext:<source>:<project>` shown as `Claude Code · <dir>` / `Codex · <dir>` (the run's
  working directory name); project `ext:<source>:<dir>`; no profile. ZCode: agent `ZCode`, no
  project.

### `GET /v2/usage` additions

- `?source=cutex|external|<source id>` filters the rows.
- groupBy `source`: key `source` (`cutex` or the id), `sourceLabel` (null for cutex),
  `sourceKind` (`cutex` | `claude-code` | `codex` | `zcode`), `sourceBilling` (`subscription` | `api`,
  null for cutex). Label and billing come from the current configuration (the stored values
  for a source deleted without purge). Costs are API-price estimates as for every row; for a
  subscription source clients show them as an API-equivalent value, not spend.
- Rows of a disabled source are kept but not counted.

### Rollback

An older cutex ignores `usage-ledger-external/` and drops `usage.extraSources` on its next
config save (§4); cutex's own ledger is unchanged, so no conversion is needed.

`zcode` (`usage-source-zcode`): a cutex with extra sources but without this kind keeps a stored
`zcode` entry in `config.json` (it round-trips the raw entry) but cannot use it: the entry is
listed with `supported: false`, never scanned, PATCH answers 400 "written by a newer cutex",
DELETE (with `?purge=true`) still works, and groupBy `source` shows it with the label / kind
stored in its rows. Its rows already in `usage-ledger-external/<id>/` stay readable and counted in
`GET /v2/usage` (the reader does not check the kind). Its `scan-state.json` keeps the `zcode`
block until that cutex rewrites the state, which it never does for an unsupported source; a
newer cutex resumes from it. No conversion is needed either way.

## Requests and context per step (token audit 2026-10-22)

Every `/v2/usage` row and `totals` carry `requests` (model requests; a Claude turn counts the SDK
steps the adapter reports as `tokenUsage.modelRequests`, older adapters count 1 per turn) and
`maxRequestInput` (the largest input one request re-read: for a Claude turn its average per step).
Tokens per turn = `input / turns`, context per step = `input / requests`. A ledger sample stores
`modelRequests` only when it covers more than one request.
