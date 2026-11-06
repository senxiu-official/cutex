# Signed component index and desktop API v1

Hosts advertise the matching component-index and external-runtime capabilities before clients use this contract. The public cutex 0.2.1 interface is additive; older hosts keep their advertised controls.

For component APIs, `managed` also includes an existing absolute base explicitly
adopted by importing a trusted signed component kit. That component-only proof
does not create a backend install record or enable backend public updates.

## Signed wire document

The public release tag `channel-components` carries `index.json` and
`index.json.minisig`. Verify the exact downloaded JSON bytes with the existing
release trust set (`upgrade::keys::trusted_keys` and `minisign::verify_text`).
There are no document-supplied keys. The JSON schema is
`component-index-v1.schema.json`; the example is deliberately unsigned fixture
data, not a publishable asset list. Schema validation alone does not establish trust.

The schema name is `cutex/components-index/v1`. `channel` is `public-stable` for
the public channel; private snapshots use a distinct named channel, such as
the opaque channel identifier in the configured signed index. `sequence` is a positive JSON safe integer, increasing within each
channel. `publishedAt` is UTC RFC3339, for display rather than a security clock.
There is no expiry that disables installed components.

Each component has a unique `name`, a nullable `recommended`, and releases with
unique `version`. Recommendation is explicit: never sort version labels or infer
an ordering relation from component names alone. Non-null recommendation must identify a non-yanked release
in that component. Releases carry:

| Field | Meaning |
| --- | --- |
| `version` | Immutable component version label, e.g. `dsh-r3`. |
| `payload` | `bundled-v1` or `adapter-v1`. Unknown payloads are incompatible. |
| `compatible.allCapabilities` | All required daemon featureList names. These describe code support, independently of whether a component is installed. |
| `assetTag` | Immutable release tag, normally `component-<name>-<version>`. |
| `platforms` | Platform to `{name, sha256, size, signatureName}` map. Platform names follow existing component manifests. |
| `yanked`, `yankReason` | Withdrawal of this release from new selection, with optional explanation. |
| `externalRuntime` | Exact `cutex/external-runtime/v1` object for adapter payloads; null for bundled payloads. |
| `externalRuntimeSha256` | SHA256 of the raw UTF-8 `external-runtime.json` file inside the archive; null for bundled payloads. |

Asset names and tags are safe labels following existing upgrade source rules;
asset names have a `.tar.gz` suffix, and `signatureName` is exactly `name +
".minisig"`. Both are fetched from the configured release source, not a URL
provided by the document. SHA256 is lowercase hex; size is the exact byte count,
at most the existing 4 GiB artifact limit. The archive and its minisig must both
verify before extraction. Existing safe extraction/receipt checks still apply.

For adapters, `externalRuntime.name/version/payload` must match the containing
release; its object must equal the parsed archive contract and the raw file SHA
must match. This reuses the adapter package contract without introducing another package
dependency language. Exact package versions and relative entries are checked by
the external-runtime module. No archive includes Node, node_modules, rg or PTY.

## Verification, cache and withdrawal

Accept a document only after signature, schema, expected channel, sequence,
unique names/versions, recommendation references, safe asset paths and external
contract bindings pass. Unknown compatibility constraints require a new schema;
do not silently ignore them. Unknown capability names make that release
incompatible, without invalidating other releases in an otherwise valid index.

Persist highest accepted sequence and exact document SHA256 per channel. Reject
lower sequence (`component_index_replay`) and equal sequence with different bytes
(`component_index_equivocation`). Equal sequence with identical bytes is a normal
refresh. Resetting a URL, restarting or changing source does not reset this state.
The global immutable identity ledger binds `(name, version, platform)` to archive
SHA256 and size across public index, private kit and verified legacy manifests;
conflicting bytes are `component_identity_conflict`, never an upgrade.

Download and verify outside the commit lock; re-read durable state under that
lock before acceptance. Persist verified content-addressed JSON/signature blobs
first, then atomically commit their pointer, watermark and identity ledger.
Interrupted writes cannot leave a higher watermark with a missing accepted
document. Corrupt state must be reported and retained for recovery, never treated
as a fresh empty channel. GET does not repair state.

A failed refresh (offline, malformed JSON, mismatched sidecar, signature failure,
replay or equivocation) keeps the last verified cache and existing selections.
It sets stale diagnostics, but does not make cached, non-yanked offers unusable.
With no accepted cache, the legacy verified manifest can supply offers. A valid
index without compatible releases may also fall back to the compatible manifest,
except that explicit `recommended:null` with no safe recommendation means no
index offer, and a known withdrawal or identity conflict cannot be bypassed via
the manifest. Report the actual effective offer source.

Withdraw using a higher signed sequence with `yanked:true`, a reason and an
explicit safe recommendation (or null). Keep accepted withdrawal identities in
the shared ledger so a source change or old manifest cannot resurrect them.
Publish a new version for a fix; v1 has no unyank operation. Existing selected
defaults and session pins remain unchanged and show the warning. Installing an
offer changes only the component default used for future agents; existing agent
pins never change. Service components retain their existing restart gate.

## Desktop capability and GET contract

Advertise `component-index-v1` only when this API is implemented. Advertise
`external-runtime-root-v1` only with implemented cached readiness and owner
check/configure API. Unknown gate means the desktop hides the corresponding UI.
Handler protocol compatibility must use static code-support capabilities, not
runtime flags which only appear after an adapter has already been installed.

Keep `GET /v2/components` and existing `managed`, `installFrom`, `operation`,
`selected`, `available`, `versions`, `origin`, `pins` and component state semantics.
In particular `available` retains its legacy manifest meaning. Add **per-component
`index`**; there is no separate GET /v2/components/index. GET reads cached state
only: no index download, Node spawn, npm invocation, dependency probe or repair.

```ts
interface ComponentIndexState {
  source: "componentIndex" | "releaseManifest" | "privateKit";
  channel: string | null;
  sequence: number | null;
  publishedAt: string | null;
  fetchedAt: string | null;
  stale: boolean;
  staleReason: string | null;
  recommended: string | null;
  selectedYanked: boolean;
  releases: ComponentIndexRelease[];
}
interface ComponentIndexRelease {
  version: string;
  yanked: boolean;
  yankReason: string | null;
  compatible: boolean;
  incompatibleReason: string | null;
  requirements: Record<string, string>;
  externalRuntime: object | null; // cutex/external-runtime/v1, not a second format
  externalRuntimeSha256: string | null;
}
```

`source` describes the effective offer policy. Manifest fallback reports
`releaseManifest`; frozen kit offers report `privateKit`. Null channel/sequence/
publication fields mean no signed index supplied those offers. `stale` is a
**channel refresh property**, repeated on each applicable component row;
`fetchedAt` is the last successful paired verification, not the last failed try.
No cache after a failed refresh has stale=true and nullable timestamps. A kit
with no refresh scheduled is not stale merely because it is old.

The daemon computes `compatible` and `incompatibleReason`, including capabilities,
supported payload/handler protocol and platform; the desktop never decides them.
Reasons are `capability:<name>`, `payload:<mode>`, `platform`, or daemon text.
Compatibility means this daemon understands the release. It is independent of
`yanked` and external dependency readiness. `recommended` is the effective safe
recommendation for this host; it can be null. Retain incompatible releases in
the list so the desktop can explain that a daemon upgrade is needed.
`requirements` is a daemon-produced display map (e.g. upstream, node, ws); it is
not authoritative input for installation. `selectedYanked` can be true even if
the withdrawn installed version is no longer present in the current document.

An adapter candidate may expose per-component `external` even while selected the previous bundled version
remains bundled. Add `external.version` to identify the adapter being checked.
With manifest-only offers, a verified staged archive supplies this contract even
before any default is selected. A blocked source build likewise retains its last
validated candidate contract in the external configuration cache. Neither path
creates a false selected Entry. An unselected candidate with known dependency
issues reports needsUser; a working bundled version keeps its existing state.
Absence means no adapter candidate or selected adapter; do not infer readiness.
Other fields retain ui-lead's draft:

```ts
interface ComponentExternal {
  version: string;
  root: string | null;
  node: string | null;
  rg: string | null;
  packages: string[];
  nodeRange: string | null;
  nodeRecommended: string | null;
  checkedAt: string | null;
  issues: {code: string; expected: string | null; actual: string | null;
           path: string | null; steps: string[]}[];
}
```

`checkedAt:null` is unverified, even when issues is empty. Readiness requires a
completed successful probe of that version/configuration and is revalidated
before selection/restore. A configuration change invalidates cached readiness.
The initial issue codes are external_root_missing, external_root_invalid,
upstream_version_mismatch, node_missing, node_version_unsupported,
kimi_pty_unavailable and kimi_rg_missing. Pass through unknown future codes.
The external-runtime module owns path, default and probe semantics.

## Owner actions and progress

All actions keep existing owner/root authorization. Install/remove/check/configure
retain existing component operation serialization and `upgrade.lock`. Index
refresh uses its own operation slot and cache-commit lock; it does not hold or
wait for `upgrade.lock`. Cache acceptance and final component selection both
hold the cache-commit lock while rechecking the accepted offer and committing
their respective state. This establishes whether selection preceded withdrawal
or withdrawal prevented selection; downloads and external probes hold no cache
lock. Never acquire upgrade.lock while holding the cache-commit lock.

| Route | Body | Result |
| --- | --- | --- |
| POST /v2/components/{name}/install | empty or `{}` | Existing verified release-manifest install. |
| POST /v2/components/{name}/install | `{ "version": "dsh-r3" }` | Select this version from the current effective verified offer source. |
| POST /v2/components/index/check | empty or `{}` | Explicit refresh of the configured signed index; no selection change. |
| POST /v2/components/{name}/check | empty or `{}` | Probe of the selected/candidate adapter; no npm install. |
| POST /v2/components/{name}/configure | `{externalRoot?, node?, rg?}` | Configuration update followed by a probe. Absolute host paths; null clears, absent unchanged. |
| POST /v2/components/{name}/remove | empty or `{}` | Existing removal and pin/use guards. |

The fixed `index/check` route is matched before generic component routes.
Do not accept arbitrary source URLs, keys, index JSON, local archive paths or
force/pin overrides in these request bodies. Source policy is deployment/CLI
configuration. Missing legacy policy defaults to `releaseManifest`; new public
installers explicitly choose `componentIndex`; kits explicitly choose `privateKit`.
Owner CLI opt-in to componentIndex changes offers only, never selections.

Explicit version install re-resolves accepted source state at preflight and
immediately before final selection. Unknown version => 409 component_offer_changed;
withdrawn => 409 component_yanked; incompatible => 409 component_incompatible.
Identity conflicts and newly withdrawn offers must also stop an in-flight install
before its selection commit. Archive identity is immutable, so a version-only
request is sufficient; an unrelated sequence increment does not invalidate it.
No automatic downgrade/switch occurs on refresh or backend update hooks. Legacy
empty install must refuse a cross-source retarget of an index/kit selection with
409 component_version_required; same selected-version repair is allowed.

Normal accepted actions return 202 `{operation}` with existing running/done/failed
states and current progress fields. Add action values indexCheck, check, configure.
Existing `cutex/components/updated` handles named component actions. GET adds a
top-level nullable `indexOperation`, separate from the existing component
`operation`, so a refresh cannot overwrite installation progress. Index refresh
emits additive `cutex/components/indexUpdated` with `{channel, sequence, stale,
staleReason, operation}`; the desktop invalidates GET, and no null component name
is sent to old clients. Index operation name is null; the index refresh 202
response still uses `{operation}`. Only one index refresh runs at a time.

Terminal component install/check/configure operations add
`result:{outcome:"selected"|"ready"|"needsUser",version,selected:boolean}`.
Dependency-blocked r3 can be verified/staged with `done`, outcome=needsUser and
selected=false. GET keeps a working bundled version selected and reports cached external issues.
`result.selected` means this operation committed a default selection, not that
some version was already selected before a check/configure action.
The desktop must not interpret done alone as selected. Failure retains existing
error/errorCode. Index refresh done means accepted refresh, failure preserves
last cache with stale diagnostics. Structured result is absent on old daemons.

## Receipts, canonical packages and kits

Preserve legacy `origin:"signed"|"source"` and receipt reading. Add `selectionSource`
provenance with kind releaseManifest/componentIndex/privateKit, channel/sequence
when applicable, accepted index SHA, actual trusted signing key ID and immutable
archive identity/SHA. Old receipts with no provenance remain readable; do not
invent historical key IDs or sequence values. Source checkouts remain development
origin and do not claim signed provenance.

Build dsh-r3/kimi-r3 once, publish immutable platform archives and minisigs, verify
downloads, then publish the signed index last under a serialized publisher.
Production signatures use the established release signing procedure, not a new
key or locally discovered credential. Published indexes bind canonical archive hashes and are verified independently from their unsigned schema examples.

Kit consumers reuse those exact archive/signature bytes. A kit lock records
canonical asset tag/name/size/SHA and optional verified index snapshot. Kit
preparation checks the identity ledger; it neither repacks adapters nor bundles
user npm prefixes. Kit activation with unready r3 keeps the working bundled default
and stages needsUser. Frozen privateKit policy is not silently replaced by a
public poll. Removing an adapter never deletes the external user root. Existing
agent pins and support for older bundled packages remain intact.

## Required implementation evidence before capability advertisement

Exercise real signed test fixtures through acceptance/cache/restart: valid,
tamper, sidecar mismatch, wrong channel, replay, equivocation, corrupt cache,
offline last-good cache, and two concurrent sequence commits. Exercise archive
and external-contract mismatches; cross-source same-identity/different-bytes;
higher-sequence yank during an install; null recommendation and old-manifest
fallback; no session/default mutation on refresh. Verify GET has no side effects,
gate-off compatibility, unknown future issue reasons, dependency-blocked r3
retaining the previous bundled version, and private kit/public canonical byte equality. Use marked private
homes and non-live ports. No real model call is required.

## External runtime response extension

See [component-external-ui-v1](component-external-ui-v1.md) for additive raw
configuration/effective-path views, compact diagnostics, candidate-before-download
semantics, fresh-host component discovery and source-build explicit-version guidance.
