# Optional DSH runtime

`dsh-runtime` runs DeepSeek Harness over ACP. The component pins DSH and its
session catalog to `0.2.1-rc.3`, and ws to `8.22.0`. Platforms are Windows x64
and Linux x64 glibc. Stable Node >=24.9.0 is required; a host supplies Node or
an offline payload includes `runtime/node` (`runtime/node.exe` on Windows).

## Install and update

On a managed cutex install:

```sh
cutex component list
cutex component install dsh-runtime
cutex component status dsh-runtime
```

The signed cutex release manifest selects the tested component version.
Repeating install selects that version for new sessions. Source-only managed
installs use `cutex component install dsh-runtime --from-source <absolute-path>`;
the input must contain explicitly staged platform-correct npm dependencies.
Neither installation nor packaging runs npm or package lifecycle scripts.

If Node is absent from the daemon PATH, set `CUTEX_DSH_NODE` to an absolute
executable and install again. Missing/old Node reports `needsUser` without
preventing cutex or another runtime from working. Existing sessions retain
their own component and absolute Node pins when a new version is selected.

Since r262, absent DSH is not advertised as `dsh-runtime`. Desktop hides its
creation options, settings card and install prompt. Initial installation uses
the existing CLI/component API, without a new discovery page. Clients refresh
host capabilities after `cutex/components/updated`. Already-installed components
remain visible for repair when Node is missing. Existing data is retained.

## Removal and data

```sh
cutex component remove dsh-runtime
cutex component prune dsh-runtime --dry-run
```

Remove unselects new sessions and removes component registration. It does not
stop sessions, delete component versions/profiles/history or rewrite native
identity. Existing sessions can use their pinned runtime while its files and
Node remain present. External deletion produces a clear start error while
exact-scope stop/recovery remains possible.

Prune preserves versions referenced by durable DSH sessions, including archived
sessions. An unreadable pin store fails cleanup before deleting any version.
The selected version and running/previous manifest are additional guards.

## Offline payload and acceptance

`build.ps1` requires PowerShell 7; `build.sh` packages on Unix. Both accept an
explicit platform-correct tree: adapter `src/`, manifests, and
`runtime/package{,-lock}.json` plus `runtime/node_modules/`. Windows native npm
packages cannot substitute for Linux dependencies. A Linux cross-build must
preserve ELF helper executable modes (ripgrep and Landlock). Archives contain
no npm `.bin` shims or external links.

The adapter projects authoritative native logs into separate SQLite storage.
Recovery/replay never resends a prompt. Context occupancy is separate from
unavailable billing. Steer, native fork/revert/delete, images and unsupported
Codex features remain disabled; approvals use Management's one-shot broker.

Actual `test/` runtime tests require explicit private debug
`DSH_SPIKE_DAEMON`, matching `DSH_SPIKE_MCP`, and `DSH_SPIKE_INSTALL` paths.
They use offline provider fixtures and private homes/ports, never real keys.
Linux execution/native sandbox guarantees require separate target acceptance.
