#!/usr/bin/env python3
"""Optional components in the cutex CI (components/README.md §1-§3).

    python3 .github/ci/components.py matrix [--root DIR]
        Reads components/*/component.json and prints the build matrix as one line of JSON:
        {"include": [{"name", "version", "platform", "runner", "leak_policy"}, ...]} (empty
        `include` when there are no components). Fails on a malformed component.json.

    python3 .github/ci/components.py check <name> <version> <platform> <out-dir> [--commit SHA]
        Checks what `components/<name>/build.sh <platform> <version> <out-dir>` wrote: exactly
        component-<name>-<version>-<platform>.tar.gz, SHA256SUMS-component-<name>-<platform> and
        BUILD-INFO-component-<name>-<platform>.json (name, version, commit, platform); the sums
        match; the archive is `gzip -n` (no name, mtime 0) of a ustar tar with regular files and
        directories only, relative paths, owner 0:0 and one mtime.

The `component-` prefix keeps these files apart from the cutex archives (cutex-<release>-<platform>
.tar.gz, BUILD-INFO-<platform>.json, SHA256SUMS-<platform>) that the release scripts glob for.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import re
import sys
import tarfile
from pathlib import Path

SCHEMA = "cutex/component/v1"
# platform -> (runner, leak policy); same runners as the cutex build job.
PLATFORMS = {
    "linux-x64": ("ubuntu-22.04", "fail"),
    "windows-x64": ("windows-2022", "warn"),
}
# Platforms a component may declare that the release does not build yet (no cutex release build for
# them: macOS is an MVP backend host). Accepted in component.json, left out of the build matrix.
UNRELEASED_PLATFORMS = {"darwin-arm64"}
NAME = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")
VERSION = re.compile(r"^[A-Za-z0-9._-]{1,64}$")
CLAUDE_VERSION = re.compile(r"^claude-r[0-9]+$")
KEYS = {"schema", "name", "version", "platforms"}


class ComponentError(Exception):
    pass


def check_name(name: str) -> str:
    if not NAME.match(name) or len(name) > 64:
        raise ComponentError(f"component name `{name}` is not kebab-case [a-z0-9-]")
    return name


def check_version(name: str, version: str) -> str:
    if not isinstance(version, str) or not VERSION.match(version) or version in (".", ".."):
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
    if not isinstance(platforms, list) or not platforms or len(set(platforms)) != len(platforms):
        raise ComponentError(f"{path}: platforms must be a non-empty list without duplicates")
    for p in platforms:
        if p not in PLATFORMS and p not in UNRELEASED_PLATFORMS:
            raise ComponentError(f"{path}: unknown platform `{p}` (allowed: {', '.join([*PLATFORMS, *sorted(UNRELEASED_PLATFORMS)])})")
    for f in ("build.sh", "README.md"):
        if not (path.parent / f).is_file():
            raise ComponentError(f"{path.parent}: missing {f}")
    return raw


def matrix(root: Path) -> dict:
    include = []
    for path in sorted((root / "components").glob("*/component.json")):
        c = load_component(path)
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


def check_output(name: str, version: str, platform: str, out: Path, commit: str | None = None) -> None:
    check_name(name)
    check_version(name, version)
    if platform not in PLATFORMS:
        raise ComponentError(f"unknown platform {platform}")
    archive = f"component-{name}-{version}-{platform}.tar.gz"
    sums = f"SHA256SUMS-component-{name}-{platform}"
    info_name = f"BUILD-INFO-component-{name}-{platform}.json"
    present = sorted(p.name for p in out.iterdir())
    if present != sorted([archive, sums, info_name]):
        raise ComponentError(f"{out}: expected exactly {sorted([archive, sums, info_name])}, found {present}")
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
    head = path.read_bytes()[:10]
    if head[:3] != b"\x1f\x8b\x08":
        raise ComponentError(f"{path.name}: not gzip")
    if head[3] & 0x08 or head[4:8] != b"\0\0\0\0":
        raise ComponentError(f"{path.name}: gzip header carries a name or mtime (use gzip -n)")
    with gzip.open(path, "rb") as f:
        first = f.read(512)
    if first[257:265] != b"ustar\x0000":
        raise ComponentError(f"{path.name}: not a POSIX ustar archive (tar --format=ustar)")
    mtimes = set()
    count = 0
    with tarfile.open(path, "r:gz") as tar:
        if any(m.pax_headers for m in tar.getmembers()):
            raise ComponentError(f"{path.name}: not a ustar archive (pax headers)")
        for m in tar.getmembers():
            count += 1
            parts = m.name.split("/")
            if m.name.startswith("/") or ".." in parts or "\\" in m.name:
                raise ComponentError(f"{path.name}: unsafe member path {m.name}")
            if not (m.isreg() or m.isdir()):
                raise ComponentError(f"{path.name}: {m.name} is not a regular file or directory")
            if m.uid != 0 or m.gid != 0:
                raise ComponentError(f"{path.name}: {m.name} is owned by {m.uid}:{m.gid}, expected 0:0")
            mtimes.add(m.mtime)
    if count == 0:
        raise ComponentError(f"{path.name}: empty archive")
    if len(mtimes) != 1:
        raise ComponentError(f"{path.name}: members have {len(mtimes)} different mtimes (expected one fixed mtime)")


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    m = sub.add_parser("matrix")
    m.add_argument("--root", default=".")
    c = sub.add_parser("check")
    c.add_argument("name")
    c.add_argument("version")
    c.add_argument("platform")
    c.add_argument("out")
    c.add_argument("--commit")
    args = ap.parse_args(argv)
    try:
        if args.cmd == "matrix":
            print(json.dumps(matrix(Path(args.root)), separators=(",", ":")))
        else:
            check_output(args.name, args.version, args.platform, Path(args.out), args.commit)
            print(f"{args.name} {args.version} {args.platform}: output ok")
        return 0
    except (ComponentError, OSError, ValueError, tarfile.TarError) as e:
        print(f"components: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
