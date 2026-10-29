"""Vendor the pinned python-build-standalone runtime into a component stage.

The upstream archive is verified by byte size and SHA-256, then carried as one regular-file payload
inside the component. cutex safely materialises its links on the target host before creating the
venv. Keeping the payload intact also permits cross-building case-sensitive Unix trees on Windows.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import tarfile
import tempfile
import urllib.request
from pathlib import Path

BASE_URL = "https://github.com/astral-sh/python-build-standalone/releases/download"
METADATA = "python-runtime.json"
NOTICES = "PYTHON-THIRD-PARTY-NOTICES.md"
PAYLOAD = "python-runtime.tar.gz"
LICENSE = "PYTHON-LICENSE.txt"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def stage_payload(archive: Path, out: Path, asset: dict) -> None:
    """Keep the signed upstream tar intact for target-side extraction and expose its PSF licence."""
    shutil.copyfile(archive, out / PAYLOAD)
    with tarfile.open(archive, "r:gz") as bundle:
        member = bundle.getmember(asset["license"])
        source = bundle.extractfile(member)
        if source is None or not member.isfile():
            raise ValueError(f"cannot read CPython licence {asset['license']!r}")
        with source, (out / LICENSE).open("wb") as dest:
            shutil.copyfileobj(source, dest)


def copy_limited(source, dest, expected: int) -> None:
    total = 0
    while block := source.read(min(1024 * 1024, expected + 1 - total)):
        total += len(block)
        if total > expected:
            raise ValueError(f"download exceeds pinned size {expected}")
        dest.write(block)


def acquire(asset: dict, release: str, explicit: Path | None, cache: Path | None, work: Path) -> Path:
    if explicit is not None:
        return explicit
    if cache is not None:
        candidate = cache / asset["name"] if cache.is_dir() else cache
        if candidate.is_file():
            return candidate
    target = work / asset["name"]
    url = f"{BASE_URL}/{release}/{asset['name']}"
    print(f"downloading {url}")
    with urllib.request.urlopen(url, timeout=60) as source, target.open("wb") as dest:
        length = source.headers.get("Content-Length")
        if length is not None and int(length) != asset["size"]:
            raise ValueError(f"download Content-Length {length}, expected {asset['size']}")
        copy_limited(source, dest, asset["size"])
    return target


def verify(path: Path, asset: dict) -> None:
    size = path.stat().st_size
    if size != asset["size"]:
        raise ValueError(f"{path.name}: size {size}, expected {asset['size']}")
    actual = sha256(path)
    if actual != asset["sha256"]:
        raise ValueError(f"{path.name}: sha256 {actual}, expected {asset['sha256']}")


def main(argv: list[str] | None = None) -> int:
    here = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", required=True, choices=("linux-x64", "windows-x64", "darwin-arm64"))
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--manifest", type=Path, default=here / "python-build-standalone-assets.json")
    parser.add_argument("--archive", type=Path, help="verified local asset; never access the network")
    args = parser.parse_args(argv)

    manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    asset = manifest["platforms"][args.platform]
    cache_value = os.environ.get("CUTEX_PBS_CACHE")
    cache = Path(cache_value) if cache_value else None
    args.out.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="claude-runtime-python-") as tmp:
        archive = acquire(asset, manifest["release"], args.archive, cache, Path(tmp))
        verify(archive, asset)
        stage_payload(archive, args.out, asset)
    metadata = {
        "schema": manifest["schema"],
        "platform": args.platform,
        "pythonVersion": manifest["pythonVersion"],
        "release": manifest["release"],
        "flavour": manifest["flavour"],
        "asset": asset,
        "source": manifest["source"],
    }
    (args.out / METADATA).write_text(json.dumps(metadata, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    (args.out / NOTICES).write_text(
        "# Bundled Python notices\n\n"
        f"This component bundles CPython {manifest['pythonVersion']} from Astral's "
        f"python-build-standalone release {manifest['release']} ({manifest['flavour']}).\n\n"
        "- CPython is distributed under the Python Software Foundation License. The complete "
        "license and attribution history is in `PYTHON-LICENSE.txt` and remains inside the "
        "target-side `prepared/python/` tree.\n"
        "- python-build-standalone build tooling is distributed under MPL-2.0; source and build "
        f"materials: {manifest['source']}.\n"
        "- Licenses shipped by CPython's bundled third-party libraries remain beside their "
        "respective files under `python/`. Python package wheel notices are in "
        "`THIRD_PARTY_NOTICES.md`.\n",
        encoding="utf-8",
    )
    print(f"vendored Python {manifest['pythonVersion']} for {args.platform} from {asset['name']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
