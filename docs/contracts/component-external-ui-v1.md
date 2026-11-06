# External component UI contract (public 0.2.1)

Clarification of component-index-v1 / external-runtime-root-v1. Changes are additive owner/root-authorized component API fields.
Existing capabilities, action bodies, signed index/archive bytes and persisted
runtime/configuration/session pin schemas are unchanged. Old desktop readers
may ignore new fields; new desktop readers must tolerate their absence.

## Raw overrides and effective paths

Every successfully read `external` view from GET component rows and terminal
check/configure `result.external` adds:

```ts
storedConfig?: { externalRoot: string | null; node: string | null; rg: string | null };
resolved?: { externalRoot: string | null; node: string | null; rg: string | null };
```

storedConfig is the exact lexical persisted owner override, without canonicalize
or display rewriting. Null means that field has no explicit stored override and
uses environment/PATH/default discovery; it does not mean discovery failed.
Omission of the whole object on an old daemon or invalid cache means unknown,
not three known absent overrides. Forms edit storedConfig and offer Restore
Default only for known explicit fields. Save sends changed values only; null
clears and absent leaves the override unchanged.

resolved lists the effective host paths used for the current check, separately
from configured input. Existing external.root/node/rg remain effective paths for
compatibility. Effective paths and issue.path use owner-readable Windows display
forms: conventional verbatim drive prefixes become ordinary drive paths and
verbatim UNC becomes ordinary UNC. Unusual volume/device forms remain unchanged.
Internal probing, cache binding, default selection and saved session pins keep
canonical/pinned execution paths; presentation never rewrites those records.
An old stored override is returned as stored; original input cannot be invented
if an older client previously saved an already-canonicalized path.

GET performs no probe, subprocess, download or write. Changing discovery inputs
invalidates cached readiness as before. Configure still persists invalidation
before bounded probes, and never commits a component default selection.

## Compact issue text and diagnostics

Each external issue retains code/expected/actual/path/steps. actual is a compact,
single-line owner message, at most240Unicode characters. When the original
contains extra diagnostic information, an optional diagnostics:string carries
that original text for an expandable diagnostic area, capped at64KiB at a UTF-8
boundary with an explicit truncation marker. This is owner-only diagnostic data;
UI text must be escaped. Unknown issue codes still pass through. Old readers see
the compact actual; new readers tolerate missing diagnostics.

This is a wire presentation change. Existing saved issue/status schemas are not
rewritten or expanded solely by reading GET; internal validation stays strict.

## Candidate discovery and download order

versions lists locally installed/downloaded component archives. It is not an
offer list. index.releases contains daemon-verified candidate metadata, and
external.version identifies the dependency candidate. Thus needsUser with empty
versions is valid before download. With a verified signed externalRuntime,
configure/check may precede any archive download. An explicit install may also
verify/stage the archive first, then return needsUser/selected=false. Neither
case changes the existing default or session pins. When there is no compatible,
non-yanked effective offer, disable installation rather than inventing one.

The desktop renders the DSH/Kimi component card when component-index-v1 is
present and that component row exists, even without its installed runtime
capability. Dependency guidance additionally requires external-runtime-root-v1.
Use the daemon's compatibility/yank/effective-offer decisions for buttons.
Keep dsh-runtime (and the corresponding Kimi runtime gate) on Profile/start
controls; showing installation guidance must not pretend the runtime is usable.
This requires ui-lead's desktop implementation, not new backend runtime gates.

## Source builds and explicit selection

Legacy empty install keeps release-manifest semantics. A source build with no
manifest still refuses with409 component_source_only. When a compatible verified
index/kit candidate exists, error details add offeredVersion:string and
suggestedRequest:{version:string}; the message advises explicit selection rather
than a source-checkout build. The error changes no state. Without such a candidate,
legacy source-only guidance remains. No implicit source-policy retarget occurs.

The desktop adds component_source_only to friendly refusal handling. When the
user selects a verified index candidate, send POST install with {version}; do
not silently retry an empty install with a guessed version. component_version_required
retains its existing cross-source-retarget meaning. Unknown refusal codes retain
their raw diagnostic fallback. ui-lead owns client readers, forms, friendly
wording and component visibility; Wren owns these backend response fields.


## Additional host response fields

A base adopted by a signed kit remains component-managed even without install.json:
GET managed=true authorizes component check/configure and explicit offered-version
install. In that shape installFrom is source-only, meaning there is no backend
release-manifest install path. Empty install returns409 component_source_only with
verified offeredVersion/suggestedRequest when available; it never tries to read a
missing backend install record or creates one. Explicit install{version} continues
to use the verified index/kit. installFrom describes the legacy manifest mechanism;
index.source describes the effective independent offer source. The desktop must
not interpret source-only as disabling explicit index/kit installation. Debug
builds without a verified attached manifest intentionally use source-only; a debug
build with a verified fixture/attached manifest may still report release.

Compact issue actual also uses owner-display Windows paths inside error text,
including MODULE_NOT_FOUND. Expanded diagnostics retain the original bounded text.
A missing, unreadable or malformed package.json, invalid metadata or wrong package
name reports external_root_invalid with the package path and repair steps. Only a
valid matching package identity with a different valid version reports
upstream_version_mismatch.

The external view adds optional nodeVersion:string|null: the actual Node --version
output captured during explicit check/configure/prepare, never the recommended
version inferred from the contract. It is read from a separate display-only cache
bound to the exact checked state and Node binary fingerprint. Missing, corrupt or
stale observation yields null. GET never creates or repairs this cache, probes
Node, or changes readiness solely to fill a display version. Old cache data may
need an explicit check before an observed version becomes available.

Legacy dsh.node and kimi.node details keep minimum=24.9.0 as a scalar minimum,
add range for the full contract range, and expose that same observed version when
bound. The existing external.nodeRange remains the full supported range. Runtime
paths/defaults/session pins and strict saved external state remain unchanged.
