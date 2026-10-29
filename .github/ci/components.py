#!/usr/bin/env python3
"""Optional components in the cutex CI (components/README.md §1-§3).

    python3 .github/ci/components.py matrix [--root DIR] [--policy FILE]
        Reads components/*/component.json and prints the build matrix as one line of JSON:
        {"include": [{"name", "version", "platform", "runner", "leak_policy"}, ...]} (empty
        `include` when there are no components). Fails on malformed metadata or release policy.
        Applies .github/ci/component-release-policy.json when present; without a policy discovers
        every supported component. --policy explicitly selects a policy and requires it to exist.

    python3 .github/ci/components.py check <name> <version> <platform> <out-dir> [--commit SHA] [--shared]
        Checks what `components/<name>/build.sh <platform> <version> <out-dir>` wrote: exactly
        component-<name>-<version>-<platform>.tar.gz, SHA256SUMS-component-<name>-<platform> and
        BUILD-INFO-component-<name>-<platform>.json (name, version, commit, platform); the sums
        match; the archive is `gzip -n` (no name, mtime 0) of normalized USTAR records with
        optional POSIX PAX `path` extensions, regular files/directories only, safe relative
        paths, owner 0:0 and one file mtime. No other PAX keys are accepted. --shared permits
        unrelated component outputs beside this component's three required files.

The `component-` prefix keeps these files apart from the cutex archives (cutex-<release>-<platform>
.tar.gz, BUILD-INFO-<platform>.json, SHA256SUMS-<platform>) that the release scripts glob for.
"""

from __future__ import annotations

import argparse
import base64
import gzip
import hashlib
import json
import os
import re
import shutil
import sys
import tarfile
from pathlib import Path

SCHEMA = "cutex/component/v1"
# release/r267 src/upgrade/archive.rs; a valid package must also be installable by that reader.
MAX_ARCHIVE_ENTRIES = 20_000
MAX_ARCHIVE_BYTES = 4 << 30
# platform -> (runner, leak policy); same runners as the cutex build job.
PLATFORMS = {
    "linux-x64": ("ubuntu-22.04", "fail"),
    "windows-x64": ("windows-2022", "warn"),
}
# Platforms a component may declare that the release does not build yet (no cutex release build for
# them: macOS is an MVP backend host). Accepted in component.json (cutex's own platform check: the
# component may be installed there with `--from-source`), left out of the build matrix, so a release
# never has their component-<name>-<version>-<platform> asset. cutex answers an install on such a host
# from a signed release with component_no_platform_package. Building them here only becomes useful
# together with a cutex release build (and signed manifest entry) for that platform.
UNRELEASED_PLATFORMS = {"darwin-arm64"}
NODE_COMPONENTS = {
    "dsh-runtime": ({"@deepseek-ai/dsh": "0.2.1-rc.3", "@deepseek-ai/dsh-session-format-catalog": "0.2.1-rc.3", "ws": "8.22.0"},
                    "24.9.0", "@deepseek-ai/dsh/lib/bin.js"),
    "kimi-runtime": ({"@moonshot-ai/kimi-code": "2.3.3", "ws": "8.22.0"},
                     "22.19.0", "@moonshot-ai/kimi-code/dist/main.mjs"),
}
NAME = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")
VERSION = re.compile(r"^[A-Za-z0-9._-]{1,64}$")
CLAUDE_VERSION = re.compile(r"^claude-r[0-9]+$")
KEYS = {"schema", "name", "version", "platforms"}


class ComponentError(Exception):
    pass


def check_name(name: str) -> str:
    if not isinstance(name, str) or not NAME.fullmatch(name) or len(name) > 64:
        raise ComponentError(f"component name `{name}` is not kebab-case [a-z0-9-]")
    return name


def check_version(name: str, version: str) -> str:
    if not isinstance(version, str) or not VERSION.fullmatch(version) or version in (".", ".."):
        raise ComponentError(f"{name}: invalid version `{version}` (expected [A-Za-z0-9._-], at most 64 chars)")
    if name == "claude-runtime" and not CLAUDE_VERSION.match(version):
        raise ComponentError(f"{name}: version must be the claude-rN label, got `{version}`")
    return version


def load_component(path: Path) -> dict:
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as e:
        raise ComponentError(f"{path}: {e}") from e
    if not isinstance(raw, dict):
        raise ComponentError(f"{path}: not a JSON object")
    extra = sorted(set(raw) - KEYS)
    missing = sorted(KEYS - set(raw))
    if extra or missing:
        raise ComponentError(f"{path}: keys must be exactly {sorted(KEYS)} (extra {extra}, missing {missing})")
    if raw["schema"] != SCHEMA:
        raise ComponentError(f"{path}: schema must be {SCHEMA}")
    name = check_name(raw["name"])
    if name != path.parent.name:
        raise ComponentError(f"{path}: name `{name}` differs from its directory `{path.parent.name}`")
    check_version(name, raw["version"])
    platforms = raw["platforms"]
    if (not isinstance(platforms, list) or not platforms or
            not all(isinstance(p, str) for p in platforms) or len(set(platforms)) != len(platforms)):
        raise ComponentError(f"{path}: platforms must be a non-empty list without duplicates")
    for p in platforms:
        if p not in PLATFORMS and p not in UNRELEASED_PLATFORMS:
            raise ComponentError(f"{path}: unknown platform `{p}` (allowed: {', '.join([*PLATFORMS, *sorted(UNRELEASED_PLATFORMS)])})")
    for f in ("build.sh", "README.md"):
        if not (path.parent / f).is_file():
            raise ComponentError(f"{path.parent}: missing {f}")
    return raw


def release_policy(path: Path, known_names: set[str]) -> dict[str, str]:
    def unique_object(pairs):
        value = {}
        for key, item in pairs:
            if key in value:
                raise ComponentError(f"{path}: duplicate policy key {key}")
            value[key] = item
        return value

    raw = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=unique_object)
    if (not isinstance(raw, dict) or set(raw) != {"schema", "omit"} or
            raw.get("schema") != "cutex/component-release-policy/v1" or not isinstance(raw.get("omit"), dict)):
        raise ComponentError(f"{path}: expected component-release-policy/v1 with exactly schema and omit")
    for name, reason in raw["omit"].items():
        check_name(name)
        if name not in known_names:
            raise ComponentError(f"{path}: unknown omitted component {name}")
        if not isinstance(reason, str) or not reason.strip() or len(reason) > 512:
            raise ComponentError(f"{path}: {name} needs a non-empty omission reason (at most 512 characters)")
    return raw["omit"]


def matrix(root: Path, policy: Path | None = None) -> dict:
    include = []
    discovered = [load_component(path) for path in sorted((root / "components").glob("*/component.json"))]
    default_policy = root / ".github" / "ci" / "component-release-policy.json"
    selected_policy = policy if policy is not None else (default_policy if default_policy.exists() else None)
    omitted = release_policy(selected_policy, {c["name"] for c in discovered}) if selected_policy is not None else {}
    for c in discovered:
        if c["name"] in omitted:
            continue
        for p in c["platforms"]:
            if p not in PLATFORMS:
                continue
            runner, leak = PLATFORMS[p]
            include.append({"name": c["name"], "version": c["version"], "platform": p,
                            "runner": runner, "leak_policy": leak})
    return {"include": include}


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def check_output(name: str, version: str, platform: str, out: Path, commit: str | None = None,
                 *, shared: bool = False) -> None:
    check_name(name)
    check_version(name, version)
    if platform not in PLATFORMS:
        raise ComponentError(f"unknown platform {platform}")
    archive = f"component-{name}-{version}-{platform}.tar.gz"
    sums = f"SHA256SUMS-component-{name}-{platform}"
    info_name = f"BUILD-INFO-component-{name}-{platform}.json"
    present = sorted(p.name for p in out.iterdir())
    required = {archive, sums, info_name}
    matches = required.issubset(present) if shared else set(present) == required
    if not matches:
        raise ComponentError(f"{out}: expected {'at least' if shared else 'exactly'} {sorted(required)}, found {present}")
    info = json.loads((out / info_name).read_text(encoding="utf-8"))
    for key, want in (("name", name), ("version", version), ("platform", platform)):
        if info.get(key) != want:
            raise ComponentError(f"{info_name}: {key} is {info.get(key)!r}, expected {want!r}")
    if not re.fullmatch(r"[0-9a-f]{40}", str(info.get("commit", ""))):
        raise ComponentError(f"{info_name}: commit malformed: {info.get('commit')!r}")
    if commit and info["commit"] != commit:
        raise ComponentError(f"{info_name}: commit {info['commit']} is not the checkout's {commit}")
    lines = [ln.split() for ln in (out / sums).read_text(encoding="utf-8").splitlines() if ln.strip()]
    if len(lines) != 1 or len(lines[0]) != 2 or lines[0][1].lstrip("*") != archive:
        raise ComponentError(f"{sums}: expected one line naming {archive}")
    if lines[0][0] != sha256(out / archive):
        raise ComponentError(f"{archive}: does not match {sums}")
    check_archive(out / archive)


def check_archive(path: Path) -> None:
    # Standalone Python makes these archives large; never read the payload just for its header.
    with path.open("rb") as f:
        head = f.read(10)
    if head[:3] != b"\x1f\x8b\x08":
        raise ComponentError(f"{path.name}: not gzip")
    if head[3] & 0x08 or head[4:8] != b"\0\0\0\0":
        raise ComponentError(f"{path.name}: gzip header carries a name or mtime (use gzip -n)")
    with gzip.open(path, "rb") as f:
        first = f.read(512)
    if first[257:265] != b"ustar\x0000":
        raise ComponentError(f"{path.name}: not USTAR records (optional POSIX PAX path extensions allowed)")
    # tarfile hides extension records. Check those separately: cutex skips global PAX,
    # so accepting a global path here could validate a different path than cutex extracts.
    with gzip.open(path, "rb") as stream:
        raw_count = raw_bytes = 0
        while True:
            header = stream.read(512)
            if not header or header == bytes(512):
                break
            if len(header) != 512 or header[257:265] != b"ustar\x0000":
                raise ComponentError(f"{path.name}: malformed/non-USTAR header")
            size = int(header[124:136].rstrip(b"\0 ") or b"0", 8)
            kind = header[156:157]
            if kind == b"x":
                if size > 64 * 1024:
                    raise ComponentError(f"{path.name}: PAX path header exceeds cutex's limit")
                data = stream.read(size)
                split = data.find(b" ")
                try:
                    length = int(data[:split])
                except ValueError:
                    length = -1
                if (split < 1 or length != len(data) or not data.endswith(b"\n") or
                        not data[split + 1:].startswith(b"path=")):
                    raise ComponentError(f"{path.name}: only one local PAX path record is allowed")
                if any(int(header[a:b].rstrip(b"\0 ") or b"0", 8) != 0
                       for a, b in ((108, 116), (116, 124))):
                    raise ComponentError(f"{path.name}: PAX header owner must be 0:0")
                stream.seek((-size) % 512, 1)
            elif kind in (b"0", b"\0", b"5"):
                if kind == b"5" and size:
                    raise ComponentError(f"{path.name}: directory entry carries data")
                raw_count += 1
                raw_bytes += size
                if raw_count > MAX_ARCHIVE_ENTRIES or raw_bytes > MAX_ARCHIVE_BYTES:
                    raise ComponentError(f"{path.name}: exceeds cutex r267's archive entry/size limits")
                stream.seek(((size + 511) // 512) * 512, 1)
            else:
                raise ComponentError(f"{path.name}: unsupported archive record {kind!r}")
    mtimes = set()
    count = 0
    total_bytes = 0
    names = set()
    with tarfile.open(path, "r:gz") as tar:
        for m in tar:
            if set(m.pax_headers) - {"path"}:
                raise ComponentError(f"{path.name}: PAX keys other than path are forbidden")
            count += 1
            total_bytes += m.size
            if count > MAX_ARCHIVE_ENTRIES or total_bytes > MAX_ARCHIVE_BYTES:
                raise ComponentError(f"{path.name}: exceeds cutex r267's archive entry/size limits")
            parts = m.name.split("/")
            if (m.name.startswith("/") or ".." in parts or "\\" in m.name or ":" in m.name or
                    any(c in m.name for c in ("\0", "\r", "\n"))):
                raise ComponentError(f"{path.name}: unsafe member path {m.name}")
            normalized = "/".join(part for part in parts if part not in ("", "."))
            if not normalized:
                raise ComponentError(f"{path.name}: empty member path {m.name}")
            if normalized in names:
                raise ComponentError(f"{path.name}: duplicate member path {m.name}")
            names.add(normalized)
            if not (m.isreg() or m.isdir()):
                raise ComponentError(f"{path.name}: {m.name} is not a regular file or directory")
            if m.uid != 0 or m.gid != 0:
                raise ComponentError(f"{path.name}: {m.name} is owned by {m.uid}:{m.gid}, expected 0:0")
            mtimes.add(m.mtime)
    if count == 0:
        raise ComponentError(f"{path.name}: empty archive")
    if len(mtimes) != 1:
        raise ComponentError(f"{path.name}: members have {len(mtimes)} different mtimes (expected one fixed mtime)")


def node_lock(name: str, runtime: Path) -> dict:
    """Reject mutable, unhashed, local or non-registry dependencies before npm runs."""
    if name not in NODE_COMPONENTS:
        raise ComponentError(f"unknown Node component {name}")
    manifest = json.loads((runtime / "package.json").read_text(encoding="utf-8"))
    lock = json.loads((runtime / "package-lock.json").read_text(encoding="utf-8"))
    if not isinstance(manifest, dict) or not isinstance(lock, dict) or not isinstance(lock.get("packages"), dict):
        raise ComponentError(f"{name}: malformed npm manifest/lockfile")
    pinned = NODE_COMPONENTS[name][0]
    if (set(manifest) != {"name", "version", "private", "dependencies"} or
            manifest.get("private") is not True or manifest.get("dependencies") != pinned):
        raise ComponentError(f"{name}: payload manifest must contain only the reviewed dependency pins and no scripts")
    if lock.get("lockfileVersion") != 3 or lock.get("packages", {}).get("", {}).get("dependencies") != pinned:
        raise ComponentError(f"{name}: lockfile does not match reviewed dependencies")
    for path, package in lock["packages"].items():
        if not path:
            continue
        if not isinstance(package, dict):
            raise ComponentError(f"{name}: malformed locked package {path}")
        if (not path.startswith("node_modules/") or any(p in ("", ".", "..") for p in path.split("/"))
                or "\\" in path or ":" in path or package.get("link")):
            raise ComponentError(f"{name}: unsafe lock package {path}")
        integrity = package.get("integrity", "")
        if not isinstance(integrity, str):
            raise ComponentError(f"{name}: malformed integrity for {path}")
        try:
            digest = base64.b64decode(integrity.removeprefix("sha512-"), validate=True)
        except ValueError:
            digest = b""
        if not integrity.startswith("sha512-") or len(digest) != 64:
            raise ComponentError(f"{name}: {path} lacks pinned SHA-512 integrity")
        url = package.get("resolved", "")
        if not isinstance(url, str) or not url.startswith("https://registry.npmjs.org/") or "?" in url or "#" in url:
            raise ComponentError(f"{name}: {path} is not an immutable npm registry tarball")
        if not package.get("version") or not package.get("license"):
            raise ComponentError(f"{name}: {path} lacks version/license metadata")
    for package, version in pinned.items():
        if lock["packages"].get("node_modules/" + package, {}).get("version") != version:
            raise ComponentError(f"{name}: wrong resolved version of {package}")
    return lock


def pack_node(name: str, version: str, platform: str, source: Path, out: Path, commit: str) -> None:
    """Package a clean exported adapter with npm-ci-verified dependencies; executes no package code."""
    if os.name == "nt" and not str(source).startswith("\\\\?\\"):
        source = Path("\\\\?\\" + str(source.resolve()))
    check_name(name)
    check_version(name, version)
    if platform not in PLATFORMS or not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ComponentError("pack-node: unsupported platform or invalid export commit")
    metadata = load_component(source / "component.json")
    if metadata["name"] != name or metadata["version"] != version or platform not in metadata["platforms"]:
        raise ComponentError("pack-node: component identity/platform mismatch")
    runtime = source / "runtime"
    if source.is_symlink() or any(p.is_symlink() for p in source.rglob("*")):
        raise ComponentError("pack-node: links in exported inputs or dependency tree")
    lock = node_lock(name, runtime)
    target_os = "win32" if platform == "windows-x64" else "linux"
    inventory = []
    missing_notices = []
    for path, locked in sorted(lock["packages"].items()):
        if not path:
            continue
        directory = runtime / path
        if not (directory / "package.json").is_file():
            if not locked.get("optional"):
                raise ComponentError(f"required locked package is absent: {path}")
            continue
        package = json.loads((directory / "package.json").read_text(encoding="utf-8"))
        if package.get("version") != locked["version"]:
            raise ComponentError(f"installed package differs from lock: {path}")
        for axis, want in (("os", target_os), ("cpu", "x64"), ("libc", "glibc")):
            values = locked.get(axis, [])
            positive = [v for v in values if not v.startswith("!")]
            if (positive and want not in positive) or "!" + want in values:
                raise ComponentError(f"wrong-platform installed package: {path} ({axis})")
        notices = sorted(p.relative_to(source).as_posix() for p in directory.iterdir()
                         if p.name.lower().startswith(("license", "licence", "copying", "notice")))
        if not notices:
            missing_notices.append(path)
        inventory.append({"path": path, "name": package.get("name"), "version": locked["version"],
                          "license": locked["license"], "resolved": locked["resolved"],
                          "integrity": locked["integrity"], "author": package.get("author"),
                          "repository": package.get("repository"), "noticePaths": notices})
    for package in NODE_COMPONENTS[name][0]:
        if not (runtime / "node_modules" / package / "package.json").is_file():
            raise ComponentError(f"missing direct dependency {package}")
    entry = runtime / "node_modules" / NODE_COMPONENTS[name][2]
    if not entry.is_file() or not (source / "src" / "server.mjs").is_file():
        raise ComponentError("missing runtime or adapter entrypoint")
    # Host ws is also imported by the adapter; copy the exact same locked bytes.
    adapter_modules = source / "node_modules"
    if adapter_modules.exists():
        raise ComponentError("pack-node: unexpected adapter node_modules")
    adapter_modules.mkdir()
    shutil.copytree(runtime / "node_modules" / "ws", adapter_modules / "ws", symlinks=True)
    (source / "NPM-PACKAGES.json").write_text(json.dumps(inventory, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    lines = ["# Third-party notices", "", "Runtime dependencies are pinned in runtime/package-lock.json.",
             "NPM-PACKAGES.json records each shipped package, license, tarball integrity and notice paths.",
             "All package files, including embedded license/source bundles, are preserved.",
             "Node.js is supplied by the host and is not redistributed by this archive.", "",
             "## Included packages", ""]
    lines.extend(f"- {p['name']} {p['version']}: {p['license']} ({p['path']})" for p in inventory)
    lines.extend(["", "## Packages without a top-level license/notice file", "",
                  "These registry payloads declare the listed license; consult their retained README, nested notices and repository metadata."])
    lines.extend(f"- {path}" for path in missing_notices)
    (source / "THIRD_PARTY_NOTICES.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    # TarInfo is created explicitly, so host umask, Windows attributes and npm mtimes cannot leak.
    files = []
    for item in ("component.json", "package.json", "package-lock.json", "README.md", "LICENSE", "NOTICE",
                 "THIRD_PARTY_NOTICES.md", "NPM-PACKAGES.json", "src", "runtime", "node_modules"):
        path = source / item
        if not path.exists():
            raise ComponentError(f"missing archive input: {item}")
        files.extend(path.rglob("*") if path.is_dir() else [path])
    selected = []
    for path in files:
        relative = path.relative_to(source)
        if path.is_symlink():
            raise ComponentError(f"archive input is a link: {relative}")
        if path.is_dir():
            continue
        if not path.is_file() or ".bin" in relative.parts:
            raise ComponentError(f"unsafe archive input: {relative}")
        if relative.as_posix() == "runtime/node_modules/.package-lock.json":
            continue  # npm's installation cache, not the reviewed lockfile
        selected.append((relative.as_posix(), path))
    if len(selected) > MAX_ARCHIVE_ENTRIES:
        raise ComponentError(f"{name}: {len(selected)} files exceed cutex r267's {MAX_ARCHIVE_ENTRIES}-entry archive limit; all files retained, no package emitted")
    if sum(path.stat().st_size for _, path in selected) > MAX_ARCHIVE_BYTES:
        raise ComponentError(f"{name}: payload exceeds cutex r267's 4-GiB archive limit")
    out.mkdir(parents=True, exist_ok=True)
    archive_name = f"component-{name}-{version}-{platform}.tar.gz"
    info_name = f"BUILD-INFO-component-{name}-{platform}.json"
    sums_name = f"SHA256SUMS-component-{name}-{platform}"
    own_names = (archive_name, info_name, sums_name)
    for path in out.iterdir():
        if any(path.name == n or path.name.startswith(n + ".") or
               path.name == "." + n or path.name.startswith("." + n + ".") for n in own_names):
            raise ComponentError(f"pack-node: own output already exists (including partial files): {path.name}")
    archive = out / archive_name
    archive_stream = archive.open("xb")
    try:
        with archive_stream as raw, gzip.GzipFile(filename="", fileobj=raw, mode="wb", mtime=0, compresslevel=9) as gz:
            with tarfile.open(fileobj=gz, mode="w|", format=tarfile.PAX_FORMAT) as tar:
                for relative, path in sorted(selected):
                    with path.open("rb") as stream:
                        head = stream.read(4)
                        stream.seek(0)
                        info = tarfile.TarInfo(relative)
                        info.size = path.stat().st_size
                        info.mtime = 1735689600
                        info.mode = 0o755 if head.startswith((b"#!", b"\x7fELF", b"MZ")) else 0o644
                        tar.addfile(info, stream)
        build = dict(name=name, version=version, platform=platform, commit=commit,
                     nodeMinimum=NODE_COMPONENTS[name][1], nodeMode="system",
                     runtimeLockSha256=sha256(runtime / "package-lock.json"),
                     lifecycleScripts=False, archiveFormat="ustar+pax-path",
                     packagesWithoutTopLevelNotices=missing_notices)
        with (out / info_name).open("x", encoding="utf-8", newline="\n") as stream:
            stream.write(json.dumps(build, indent=2) + "\n")
        with (out / sums_name).open("x", encoding="utf-8", newline="\n") as stream:
            stream.write(f"{sha256(archive)}  {archive.name}\n")
        check_output(name, version, platform, out, commit, shared=True)
    except Exception:
        archive.unlink(missing_ok=True)
        raise


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    m = sub.add_parser("matrix")
    m.add_argument("--root", default=".")
    m.add_argument("--policy", type=Path)
    c = sub.add_parser("check")
    c.add_argument("name")
    c.add_argument("version")
    c.add_argument("platform")
    c.add_argument("out")
    c.add_argument("--commit")
    c.add_argument("--shared", action="store_true", help="allow other components' outputs in this directory")
    n = sub.add_parser("node-lock")
    n.add_argument("name", choices=NODE_COMPONENTS)
    n.add_argument("runtime", type=Path)
    p = sub.add_parser("pack-node")
    p.add_argument("name", choices=NODE_COMPONENTS)
    p.add_argument("version")
    p.add_argument("platform", choices=PLATFORMS)
    p.add_argument("source", type=Path)
    p.add_argument("out", type=Path)
    p.add_argument("--commit", required=True)
    args = ap.parse_args(argv)
    try:
        if args.cmd == "matrix":
            print(json.dumps(matrix(Path(args.root), args.policy), separators=(",", ":")))
        elif args.cmd == "node-lock":
            node_lock(args.name, args.runtime)
        elif args.cmd == "pack-node":
            pack_node(args.name, args.version, args.platform, args.source, args.out, args.commit)
        else:
            check_output(args.name, args.version, args.platform, Path(args.out), args.commit, shared=args.shared)
            print(f"{args.name} {args.version} {args.platform}: output ok")
        return 0
    except (ComponentError, OSError, ValueError, tarfile.TarError) as e:
        print(f"components: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
