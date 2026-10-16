#!/usr/bin/env python3
"""Write the release manifest and the channel pointer that cutex's backend updates read
(upstream/cutex src/upgrade/manifest.rs, docs/backend-upgrade.md §2), from CI build artifacts.

    upgrade-manifest.py manifest --cutex-dir DIR [--native-dir DIR] [--component-dir DIR ...]
                                 --sequence N|next [--previous FILE] --out DIR
                                 [--release LABEL] [--meta meta.json] [--notes-en TEXT] [--notes-zh TEXT]
                                 [--mode reload|full] [--published-at ISO]
    upgrade-manifest.py channel --manifest DIR/manifest.json --out DIR

`--cutex-dir`: the downloaded `cutex-<platform>` artifacts of the cutex "Build binaries" run (any
nesting): `cutex-<release>-<platform>.tar.gz` + `BUILD-INFO-<platform>.json`. `--native-dir`: the
`natives-<platform>` artifacts of the cute-codex run (`native-<version>-<platform>.tar.gz` +
`BUILD-INFO-<platform>.json`); they are referenced under the release tag `native-<version>`. `--component-dir` (repeatable): the
`component-<name>-<platform>` artifacts (`component-<name>-<version>-<platform>.tar.gz`,
`SHA256SUMS-component-<name>-<platform>`, `BUILD-INFO-component-<name>-<platform>.json`); each becomes
`components.<name>` = {version, artifacts: {platform: {tag, name, sha256, size}}} with the tag
`component-<name>-<version>`. The Claude runtime is the component `claude-runtime`: the generator never
writes the older `claudeRuntime` part (cutex maps an old one to `components.claude-runtime`). The cutex
and component files may share one directory: their names never overlap (`component-` prefix).
`--previous`: the manifest.json or channel.json of the release this one follows; `--sequence` must be
higher than its sequence, and `--sequence next` takes its sequence + 1. `--previous-capabilities`: the
previous release's capability list (a JSON list, {platform: list}, a `cutex management schema export`
index.json or that release's capabilities.json); every capability it had that this build lacks must be
in capabilities.removed, else cutex's post-update verification rolls the update back.

Compatibility with the 0.1.0 updater (checked on every manifest written, see check_compat): schemas
cutex/release-manifest/v1 and cutex/channel/v1; mode reload|full; the existing migration classes;
sequence >= 2 (0.1.0 was 1); every artifact under cutex / native / components is a .tar.gz with a real
size (0 < size <= 4 GiB) and a sha256; components only under the top-level `components`, never in
cutex.artifacts or native.artifacts, and no `claudeRuntime` part at all (0.1.0 reads it as an optional
part and validates its entries); the cutex archive has cutex and cutex-mcp at its root; the build's
`cutex --version` names the release label and the commit's first 7 characters.
`--meta`: optional JSON with any of `notes` {en, zh-CN}, `capabilities` {added, removed},
`migrations` [{id, class, since, notes, newStores}], `upgradeFloor`, `desktop` {minCommit, branch},
`packageRunRecommended`, `mode` (command-line flags win).

Nothing is signed here: sign `manifest.json` and `channel.json` offline with minisign
(sign-and-upload.sh). The files written are exactly the bytes to sign and publish.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import hashlib
import json
import re
import sys
import tarfile
from pathlib import Path

PLATFORMS = ("linux-x64", "windows-x64")
LABEL = re.compile(r"^[A-Za-z0-9._-]{1,64}$")
COMPONENT_NAME = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")
CLAUDE_RUNTIME_VERSION = re.compile(r"^claude-r[0-9]+$")
COMPONENT_PREFIX = "component-"
MIGRATION_CLASSES = ("reload-safe", "rollback-safe", "one-way")
MANIFEST_SCHEMA = "cutex/release-manifest/v1"
CHANNEL_SCHEMA = "cutex/channel/v1"
MIN_SEQUENCE = 2  # 0.1.0 was published with sequence 1
MAX_ARTIFACT_SIZE = 4 << 30
# Capabilities a host advertises only while something outside the build is present (cutex
# src/management/v2/capabilities.rs: job-external with a live Job Service, job-source-nci with the
# NCI build and setup). A CI build never lists them, so they are not counted as removed.
HOST_DEPENDENT_CAPABILITIES = frozenset({"job-external", "job-source-nci"})


class ManifestError(Exception):
    pass


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def label(value: str, what: str) -> str:
    if not LABEL.match(value) or value in (".", ".."):
        raise ManifestError(f"invalid {what} `{value}` (expected [A-Za-z0-9._-], at most 64 chars)")
    return value


def find_builds(root: Path, prefix: str) -> dict[str, tuple[dict, Path]]:
    """platform -> (BUILD-INFO json, archive) under root; every platform at most once."""
    found: dict[str, tuple[dict, Path]] = {}
    for info_path in sorted(root.rglob("BUILD-INFO-*.json")):
        if info_path.name.startswith(f"BUILD-INFO-{COMPONENT_PREFIX}"):
            continue  # a component's, see find_components
        platform = info_path.stem[len("BUILD-INFO-"):]
        if platform not in PLATFORMS:
            raise ManifestError(f"{info_path}: unknown platform {platform}")
        info = json.loads(info_path.read_text(encoding="utf-8"))
        archives = [p for p in info_path.parent.glob(f"{prefix}-*-{platform}.tar.gz")
                    if not p.name.startswith(COMPONENT_PREFIX)]
        if len(archives) != 1:
            raise ManifestError(f"{info_path.parent}: expected one {prefix}-*-{platform}.tar.gz, found {len(archives)}")
        sums = info_path.parent / f"SHA256SUMS-{platform}"
        if sums.is_file():
            want = {line.split()[1].lstrip("*"): line.split()[0] for line in sums.read_text().splitlines() if line.strip()}
            if want.get(archives[0].name) != sha256(archives[0]):
                raise ManifestError(f"{archives[0]}: does not match {sums.name}")
        if platform in found:
            raise ManifestError(f"two {platform} builds under {root}")
        found[platform] = (info, archives[0])
    if not found:
        raise ManifestError(f"no BUILD-INFO-<platform>.json under {root}")
    return found


def find_components(roots: list[Path]) -> dict[str, dict]:
    """name -> {version, artifacts: {platform: {tag, name, sha256, size}}} from the component builds
    under roots; every (name, platform) at most once, one version per name."""
    builds: dict[str, dict[str, tuple[str, Path]]] = {}
    for root in roots:
        infos = sorted(root.rglob(f"BUILD-INFO-{COMPONENT_PREFIX}*.json"))
        if not infos:
            raise ManifestError(f"no BUILD-INFO-{COMPONENT_PREFIX}<name>-<platform>.json under {root}")
        for info_path in infos:
            info = json.loads(info_path.read_text(encoding="utf-8"))
            name, version, platform = info.get("name"), info.get("version"), info.get("platform")
            if not isinstance(name, str) or not COMPONENT_NAME.match(name) or len(name) > 64:
                raise ManifestError(f"{info_path}: invalid component name {name!r}")
            label(str(version), f"{name} version")
            if name == "claude-runtime" and not CLAUDE_RUNTIME_VERSION.match(version):
                raise ManifestError(f"{info_path}: the claude-runtime version must be its claude-rN label, got {version}")
            if platform not in PLATFORMS:
                raise ManifestError(f"{info_path}: unknown platform {platform}")
            if info_path.name != f"BUILD-INFO-{COMPONENT_PREFIX}{name}-{platform}.json":
                raise ManifestError(f"{info_path}: file name does not match name {name} / platform {platform}")
            if not re.fullmatch(r"[0-9a-f]{40}", str(info.get("commit", ""))):
                raise ManifestError(f"{info_path}: commit malformed: {info.get('commit')!r}")
            archive = info_path.parent / f"{COMPONENT_PREFIX}{name}-{version}-{platform}.tar.gz"
            if not archive.is_file():
                raise ManifestError(f"{info_path.parent}: missing {archive.name}")
            sums = info_path.parent / f"SHA256SUMS-{COMPONENT_PREFIX}{name}-{platform}"
            if not sums.is_file():
                raise ManifestError(f"{info_path.parent}: missing {sums.name}")
            want = {line.split()[1].lstrip("*"): line.split()[0] for line in sums.read_text().splitlines() if line.strip()}
            if want.get(archive.name) != sha256(archive):
                raise ManifestError(f"{archive}: does not match {sums.name}")
            per = builds.setdefault(name, {})
            if platform in per:
                raise ManifestError(f"two {name} {platform} builds ({per[platform][1]} and {archive})")
            per[platform] = (version, archive)
    out: dict[str, dict] = {}
    for name, per in sorted(builds.items()):
        version = one({v for v, _ in per.values()}, f"the {name} version")
        tag = f"{COMPONENT_PREFIX}{name}-{version}"
        out[name] = {"version": version,
                     "artifacts": {p: artifact(a, tag) for p, (_, a) in sorted(per.items())}}
    return out


def artifact(path: Path, tag: str | None = None) -> dict:
    out = {"name": path.name, "sha256": sha256(path), "size": path.stat().st_size}
    if tag:
        out = {"tag": tag, **out}
    return out


def check_cutex_archive(path: Path, platform: str) -> None:
    exe = ".exe" if platform.startswith("windows") else ""
    want = {f"cutex{exe}", f"cutex-mcp{exe}"}
    try:
        with tarfile.open(path, "r:gz") as tar:
            files = {m.name.removeprefix("./") for m in tar.getmembers() if m.isreg()}
    except (tarfile.TarError, OSError) as e:
        raise ManifestError(f"{path.name}: not a readable .tar.gz: {e}") from e
    if not want <= files:
        raise ManifestError(f"{path.name}: needs {sorted(want)} at the archive root, has {sorted(files)}")


def load_capabilities(path: str) -> dict[str, set[str]] | set[str]:
    doc = json.loads(Path(path).read_text(encoding="utf-8"))
    if isinstance(doc, dict) and isinstance(doc.get("capabilities"), list):
        doc = doc["capabilities"]  # schema export index.json
    if isinstance(doc, list) and all(isinstance(c, str) for c in doc):
        return set(doc)
    if isinstance(doc, dict) and all(isinstance(v, list) for v in doc.values()):
        return {p: set(v) for p, v in doc.items()}
    raise ManifestError(f"{path}: expected a capability list, {{platform: list}} or a schema index.json")


def check_capabilities(cutex: dict, declared: dict, previous: dict[str, set[str]] | set[str] | None) -> None:
    """declared added must be in every build; with `previous`, every capability it had and a build
    lacks must be declared removed."""
    lists = {p: i.get("capabilities") for p, (i, _) in cutex.items()}
    if previous is None and not declared.get("added"):
        return
    if any(not isinstance(v, list) for v in lists.values()):
        raise ManifestError("the cutex builds carry no `capabilities` list (BUILD-INFO from an older build-cutex.sh)")
    for platform, caps in lists.items():
        caps = set(caps)
        missing = sorted(set(declared.get("added", [])) - caps)
        if missing:
            raise ManifestError(f"capabilities.added names {missing}, which the {platform} build does not advertise")
        if previous is None:
            continue
        prev = previous.get(platform, set()) if isinstance(previous, dict) else previous
        lost = sorted(prev - caps - HOST_DEPENDENT_CAPABILITIES - set(declared.get("removed", [])))
        if lost:
            raise ManifestError(f"the {platform} build no longer advertises {lost}: list them in capabilities.removed "
                                f"(--meta), or cutex's post-update verification rolls the update back")


def check_compat(manifest: dict) -> None:
    """What the 0.1.0 updater requires of every manifest (it rejects the whole manifest otherwise)."""
    if manifest.get("schema") != MANIFEST_SCHEMA:
        raise ManifestError("schema must stay " + MANIFEST_SCHEMA)
    if manifest.get("mode") not in ("reload", "full"):
        raise ManifestError("mode must be reload or full")
    if not isinstance(manifest.get("sequence"), int) or manifest["sequence"] < MIN_SEQUENCE:
        raise ManifestError(f"sequence must be >= {MIN_SEQUENCE} (0.1.0 was 1)")
    if "claudeRuntime" in manifest:
        raise ManifestError("no claudeRuntime part: the Claude runtime is components.claude-runtime")
    for m in manifest["cutex"]["migrations"]:
        if m.get("class") not in MIGRATION_CLASSES:
            raise ManifestError(f"migration {m.get('id')}: class must be one of {MIGRATION_CLASSES}")
    parts = [("cutex", manifest["cutex"]["artifacts"], "cutex-")]
    if "native" in manifest:
        parts.append(("native", manifest["native"]["artifacts"], "native-"))
    for name, c in manifest.get("components", {}).items():
        parts.append((f"components.{name}", c["artifacts"], f"{COMPONENT_PREFIX}{name}-"))
    for where, artifacts, prefix in parts:
        if not artifacts:
            raise ManifestError(f"{where}: no artifacts")
        for platform, a in artifacts.items():
            if platform not in PLATFORMS:
                raise ManifestError(f"{where}: unknown platform {platform}")
            n = a.get("name", "")
            if not n.endswith(".tar.gz") or not n.startswith(prefix):
                raise ManifestError(f"{where}.{platform}: `{n}` is not a {prefix}*.tar.gz")
            if where in ("cutex", "native") and n.startswith(COMPONENT_PREFIX):
                raise ManifestError(f"{where}.{platform}: a component archive outside `components`")
            if not isinstance(a.get("size"), int) or not 0 < a["size"] <= MAX_ARTIFACT_SIZE:
                raise ManifestError(f"{where}.{platform}: size must be 1..4 GiB, got {a.get('size')}")
            if not re.fullmatch(r"[0-9a-f]{64}", str(a.get("sha256", ""))):
                raise ManifestError(f"{where}.{platform}: sha256 malformed")
            label(n, "artifact name")
            if "tag" in a:
                label(a["tag"], "release tag")


def one(values: set, what: str):
    if len(values) != 1:
        raise ManifestError(f"the builds disagree on {what}: {sorted(values)}")
    return next(iter(values))


def build_manifest(args) -> dict:
    meta = json.loads(Path(args.meta).read_text(encoding="utf-8")) if args.meta else {}
    for key in ("claudeRuntime", "components"):
        if key in meta:
            raise ManifestError(f"--meta cannot set `{key}`: it comes from the builds"
                                + (" (the Claude runtime is the component claude-runtime: --component-dir)"
                                   if key == "claudeRuntime" else ""))
    cutex = find_builds(Path(args.cutex_dir), "cutex")
    release = label(args.release or one({i["release"] for i, _ in cutex.values()}, "the release label"), "release")
    commit = one({i["commit"] for i, _ in cutex.values()}, "the cutex commit")
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ManifestError(f"cutex commit malformed: {commit}")
    for platform, (info, archive) in cutex.items():
        if info["release"] != release:
            raise ManifestError(f"{platform} was built as {info['release']}, not {release} (cutex --version must name it)")
        if archive.name != f"cutex-{release}-{platform}.tar.gz":
            raise ManifestError(f"unexpected archive name {archive.name}")
        if info.get("channel") != "release":
            raise ManifestError(f"{platform} was not built for the release channel")
        if f"({release}, {commit[:7]})" not in str(info.get("versionLine", "")):
            raise ManifestError(f"{platform}: `cutex --version` ({info.get('versionLine')!r}) does not name ({release}, {commit[:7]})")
        check_cutex_archive(archive, platform)
    missing = [p for p in PLATFORMS if p not in cutex]
    if missing:
        print(f"warning: no cutex build for {', '.join(missing)}", file=sys.stderr)

    notes = dict(meta.get("notes") or {})
    if args.notes_en:
        notes["en"] = args.notes_en
    if args.notes_zh:
        notes["zh-CN"] = args.notes_zh
    migrations = meta.get("migrations") or []
    for m in migrations:
        label(m["id"], "migration id")
        label(m["since"], "migration release")
        if m.get("class") not in MIGRATION_CLASSES:
            raise ManifestError(f"migration {m['id']}: class must be one of {MIGRATION_CLASSES}")
    check_capabilities(cutex, meta.get("capabilities") or {},
                       load_capabilities(args.previous_capabilities) if args.previous_capabilities else None)
    cutex_part = {
        "release": release,
        "commit": commit,
        "capabilities": {"added": [], "removed": [], **(meta.get("capabilities") or {})},
        "migrations": migrations,
        "artifacts": {p: artifact(a) for p, (_, a) in sorted(cutex.items())},
    }
    if meta.get("upgradeFloor"):
        cutex_part["upgradeFloor"] = label(meta["upgradeFloor"], "upgrade floor")

    manifest = {
        "schema": "cutex/release-manifest/v1",
        "channel": "release",
        "sequence": args.sequence,
        "publishedAt": args.published_at or _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "notes": notes,
        "cutex": cutex_part,
    }
    if args.native_dir:
        natives = find_builds(Path(args.native_dir), "native")
        version = label(one({i["version"] for i, _ in natives.values()}, "the native version"), "native version")
        ncommit = one({i["commit"] for i, _ in natives.values()}, "the native commit")
        tag = f"native-{version}"
        for platform, (_, archive) in natives.items():
            if archive.name != f"native-{version}-{platform}.tar.gz":
                raise ManifestError(f"unexpected archive name {archive.name}")
        manifest["native"] = {
            "version": version,
            "commit": ncommit,
            "artifacts": {p: artifact(a, tag) for p, (_, a) in sorted(natives.items())},
        }
    if args.component_dir:
        manifest["components"] = find_components([Path(d) for d in args.component_dir])
    manifest["desktop"] = meta.get("desktop") or {"branch": "release"}
    manifest["mode"] = args.mode or meta.get("mode") or ("full" if "native" in manifest or migrations else "reload")
    if manifest["mode"] not in ("reload", "full"):
        raise ManifestError("mode must be reload or full")
    manifest["packageRunRecommended"] = bool(meta.get("packageRunRecommended", "native" in manifest))
    check_compat(manifest)
    return manifest


def resolve_sequence(value: str, previous: str | None) -> int:
    prev = None
    if previous:
        doc = json.loads(Path(previous).read_text(encoding="utf-8"))
        if doc.get("schema") not in (MANIFEST_SCHEMA, CHANNEL_SCHEMA):
            raise ManifestError(f"{previous} is neither a release manifest nor a channel pointer")
        prev = doc["sequence"]
        if not isinstance(prev, int) or prev < 1:
            raise ManifestError(f"{previous}: sequence malformed")
    if value == "next":
        if prev is None:
            raise ManifestError("--sequence next needs --previous")
        return prev + 1
    try:
        seq = int(value)
    except ValueError:
        raise ManifestError(f"--sequence must be a number or `next`, got {value}") from None
    if seq < MIN_SEQUENCE:
        raise ManifestError(f"--sequence must be >= {MIN_SEQUENCE} (0.1.0 was 1) and higher than every earlier release's")
    if prev is not None and seq <= prev:
        raise ManifestError(f"--sequence {seq} is not higher than the previous release's {prev}")
    return seq


def cmd_manifest(args) -> int:
    args.sequence = resolve_sequence(args.sequence, args.previous)
    manifest = build_manifest(args)
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    data = (json.dumps(manifest, indent=2, ensure_ascii=False) + "\n").encode("utf-8")
    (out / "manifest.json").write_bytes(data)
    print(f"{out / 'manifest.json'}: cutex {manifest['cutex']['release']} sequence {manifest['sequence']} "
          f"mode {manifest['mode']}" + (f", native {manifest['native']['version']}" if "native" in manifest else "")
          + "".join(f", {n} {c['version']}" for n, c in manifest.get("components", {}).items()))
    print(f"  sha256 {hashlib.sha256(data).hexdigest()}; publish under tag cutex-{manifest['cutex']['release']}")
    return 0


def cmd_channel(args) -> int:
    path = Path(args.manifest)
    data = path.read_bytes()
    manifest = json.loads(data)
    if manifest.get("schema") != MANIFEST_SCHEMA:
        raise ManifestError(f"{path} is not a cutex release manifest")
    pointer = {
        "schema": CHANNEL_SCHEMA,
        "channel": manifest["channel"],
        "sequence": manifest["sequence"],
        "release": manifest["cutex"]["release"],
        "manifestSha256": hashlib.sha256(data).hexdigest(),
        "publishedAt": manifest.get("publishedAt"),
    }
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    (out / "channel.json").write_text(json.dumps(pointer, indent=2) + "\n", encoding="utf-8")
    print(f"{out / 'channel.json'}: channel {pointer['channel']} -> {pointer['release']} (sequence {pointer['sequence']}); "
          f"publish under tag channel-{pointer['channel']}")
    return 0


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    m = sub.add_parser("manifest")
    m.add_argument("--cutex-dir", required=True)
    m.add_argument("--native-dir")
    m.add_argument("--component-dir", action="append", default=[])
    m.add_argument("--sequence", required=True, help="N, or `next` (= --previous's sequence + 1)")
    m.add_argument("--previous", help="manifest.json or channel.json of the release this one follows")
    m.add_argument("--previous-capabilities", help="the previous release's capability list (see above)")
    m.add_argument("--release", help="release label (default: the one the binaries were built with)")
    m.add_argument("--meta")
    m.add_argument("--notes-en")
    m.add_argument("--notes-zh")
    m.add_argument("--mode", choices=("reload", "full"))
    m.add_argument("--published-at")
    m.add_argument("--out", required=True)
    c = sub.add_parser("channel")
    c.add_argument("--manifest", required=True)
    c.add_argument("--out", required=True)
    args = ap.parse_args(argv)
    try:
        return cmd_manifest(args) if args.cmd == "manifest" else cmd_channel(args)
    except (ManifestError, KeyError, ValueError) as e:
        print(f"upgrade-manifest: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
