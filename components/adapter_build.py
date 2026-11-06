#!/usr/bin/env python3
"""Build a source-only adapter from committed Git bytes; no npm, downloads or CLI calls.

    python3 components/adapter_build.py NAME PLATFORM VERSION OUT

The flat component archive contains our JS, its external-runtime contract and
notes. Development dependencies and upstream packages are never packaged.
"""
from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import tarfile

NAMES = ("dsh-runtime", "kimi-runtime")
PLATFORMS = ("linux-x64", "windows-x64")
EPOCH = 1735689600
# Detailed README/setup syntax is maintained in the repository, outside the
# immutable payload. CLI documentation updates must not change an rN archive.
ROOT_FILES = ("component.json", "external-runtime.json", "LICENSE", "NOTICE", "package.json")


class BuildError(Exception):
    pass


def git(root: Path, *args: str) -> bytes:
    result = subprocess.run(["git", "-C", str(root), *args], capture_output=True)
    if result.returncode:
        raise BuildError(result.stderr.decode(errors="replace").strip())
    return result.stdout


def document(data: bytes, name: str) -> dict:
    try:
        value = json.loads(data)
    except (ValueError, UnicodeError) as error:
        raise BuildError(f"invalid {name}") from error
    if not isinstance(value, dict):
        raise BuildError(f"{name} must be an object")
    return value


def inputs(root: Path, name: str, platform: str, version: str) -> tuple[str, dict[str, bytes]]:
    if name not in NAMES or platform not in PLATFORMS:
        raise BuildError("unsupported adapter/platform")
    prefix = f"components/{name}/"
    match = re.fullmatch(r"(dsh|kimi)-r([0-9]+)", version)
    if not match or match[1] != name.split("-")[0] or int(match[2]) < 3:
        raise BuildError("source-only adapters require a new r3-or-later identity")
    watched = [prefix, "components/adapter_build.py"]
    if git(root, "status", "--porcelain", "--untracked-files=no", "--", *watched).strip():
        raise BuildError("tracked adapter/build inputs have local changes; commit before building")
    commit = git(root, "rev-parse", "HEAD").decode().strip()
    tree = git(root, "ls-tree", "-rz", "HEAD", "--", prefix).split(b"\0")
    files: dict[str, bytes] = {}
    for record in tree:
        if not record:
            continue
        meta, raw_path = record.split(b"\t", 1)
        mode, kind, oid = meta.split()
        path = raw_path.decode().removeprefix(prefix)
        selected = path in ROOT_FILES or path.startswith("src/")
        if not selected:
            continue
        if mode not in (b"100644", b"100755") or kind != b"blob":
            raise BuildError(f"adapter input is not a regular file: {path}")
        parts = PurePosixPath(path).parts
        if path.startswith("/") or any(p in ("", ".", "..") for p in parts) or any(c in path for c in "\\:\r\n\0"):
            raise BuildError(f"unsafe archive path: {path}")
        if path.startswith("src/") and not path.endswith(".mjs"):
            raise BuildError(f"only owned JavaScript belongs in src/: {path}")
        data = git(root, "cat-file", "blob", oid.decode())
        if b"\r" in data:
            raise BuildError(f"adapter source must use LF: {path}")
        files[path] = data
    missing = set(ROOT_FILES) - files.keys()
    if missing or "src/server.mjs" not in files:
        raise BuildError(f"missing adapter files: {sorted(missing)}; server.mjs required")
    metadata = document(files["component.json"], "component.json")
    if metadata != {"schema": "cutex/component/v1", "name": name, "version": version, "platforms": list(PLATFORMS)}:
        raise BuildError("component identity/platform metadata does not match requested build")
    contract = document(files["external-runtime.json"], "external-runtime.json")
    if any(contract.get(k) != v for k, v in {"schema": "cutex/external-runtime/v1", "name": name, "version": version, "payload": "adapter-v1"}.items()):
        raise BuildError("external-runtime contract identity mismatch")
    package = document(files["package.json"], "package.json")
    if package.get("private") is not True or package.get("type") != "module" or not isinstance(package.get("name"), str):
        raise BuildError("adapter must be a private ESM package")
    # Keep source-checkout test/dev metadata out of the installed adapter.
    public_package = {k: package[k] for k in ("name", "private", "type", "engines") if k in package}
    files["package.json"] = (json.dumps(public_package, indent=2, ensure_ascii=False) + "\n").encode()
    return commit, files


def archive_bytes(files: dict[str, bytes]) -> bytes:
    tar_bytes = io.BytesIO()
    with tarfile.open(fileobj=tar_bytes, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        for name, data in sorted(files.items()):
            entry = tarfile.TarInfo(name)
            entry.size = len(data)
            entry.mode = 0o644
            entry.mtime = EPOCH
            entry.uid = entry.gid = 0
            entry.uname = entry.gname = ""
            tar.addfile(entry, io.BytesIO(data))
    # GzipFile avoids embedding a filename, wall-clock time or platform OS marker.
    result = io.BytesIO()
    with gzip.GzipFile(filename="", mode="wb", fileobj=result, mtime=0, compresslevel=9) as stream:
        stream.write(tar_bytes.getvalue())
    return result.getvalue()


def build(root: Path, name: str, platform: str, version: str, out: Path) -> dict:
    commit, files = inputs(root, name, platform, version)
    archive = f"component-{name}-{version}-{platform}.tar.gz"
    sums = f"SHA256SUMS-component-{name}-{platform}"
    info_name = f"BUILD-INFO-component-{name}-{platform}.json"
    out = out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    payload = archive_bytes(files)
    digest = hashlib.sha256(payload).hexdigest()
    info = {"name": name, "version": version, "platform": platform, "commit": commit,
            "payload": "adapter-v1", "externalRuntime": True, "archiveFormat": "ustar",
            "files": len(files), "sha256": digest,
            "recipe": "cutex/source-adapter/v1", "mtime": EPOCH}
    outputs = {archive: payload, sums: f"{digest}  {archive}\n".encode(),
               info_name: (json.dumps(info, indent=2) + "\n").encode()}
    # Publish names with exclusive create, and roll back only files this call created.
    # Unrelated component outputs and existing canonical identities are never replaced.
    created: list[Path] = []
    try:
        for name, data in outputs.items():
            target = out / name
            with target.open("xb") as stream:
                created.append(target)
                stream.write(data)
    except BaseException:
        for target in created:
            target.unlink(missing_ok=True)
        raise
    print(f"{out / archive}\n{digest}")
    return info


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("name", choices=NAMES)
    ap.add_argument("platform", choices=PLATFORMS)
    ap.add_argument("version")
    ap.add_argument("out", type=Path)
    args = ap.parse_args()
    try:
        root = Path(git(Path(__file__).resolve().parent, "rev-parse", "--show-toplevel").decode().strip())
        build(root, args.name, args.platform, args.version, args.out)
    except (BuildError, OSError, ValueError, tarfile.TarError) as error:
        print(f"adapter build: {error}", file=__import__("sys").stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
