"""Vendor the runtime wheels of the Claude runtime for one platform and write a hash lock.

    python scripts/vendor_wheels.py --platform linux-x64|windows-x64|darwin-arm64 --out DIR
                                    [--requirements requirements-runtime.txt]

Writes DIR/wheels/*.whl and DIR/requirements.lock, which cutex installs with
    python -m pip install --no-index --find-links DIR/wheels --require-hashes -r DIR/requirements.lock
(components/claude-runtime/README.md). Used by build.sh (release archives) and by
`cutex component install claude-runtime --from-source` (source installs). Needs network (PyPI).

- Wheels only, for every supported CPython (SUPPORTED_PYTHONS) of the platform: a package with
  per-version wheels gets one hash per wheel, and a package only some versions need gets a
  `python_version` marker.
- claude-agent-sdk: Anthropic's PyPI wheels contain a copy of the proprietary Claude Code CLI,
  which must not be redistributed (the adapter always runs the user's own `claude`). The SDK's own
  code is MIT-licensed, so its sdist is built here into a CLI-free py3-none-any wheel with a pinned
  build backend and SOURCE_DATE_EPOCH, which makes the wheel reproducible. The sdist's sha256 is
  pinned in SDK_SDIST_SHA256.
- The other wheels are third-party packages redistributed under their own licenses (each wheel
  carries its license in its .dist-info).
"""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import shutil
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path

SUPPORTED_PYTHONS = ("3.11", "3.12", "3.13", "3.14")
PLATFORM_TAGS = {
    # cutex's Linux binaries target glibc 2.35 (Ubuntu 22.04); wheels up to manylinux_2_34.
    "linux-x64": ["manylinux2014_x86_64", "manylinux_2_17_x86_64", "manylinux_2_28_x86_64", "manylinux_2_34_x86_64"],
    "windows-x64": ["win_amd64"],
    # Apple silicon, macOS 11+ (pip also accepts the older and universal2 macOS tags these imply).
    "darwin-arm64": ["macosx_14_0_arm64"],
}
SDK = "claude-agent-sdk"
# Build backend of the SDK sdist (hatchling) and its dependencies, pinned for a reproducible wheel.
SDK_BUILD_CONSTRAINTS = """hatchling==1.32.4
packaging==26.3
pathspec==1.1.1
pluggy==1.7.0
tomlkit==0.15.1
trove-classifiers==2026.9.21.13
"""
# sha256 of the SDK sdist on PyPI per pinned version: a new SDK pin needs its sdist reviewed and added here.
SDK_SDIST_SHA256 = {
    "0.2.166": "6dbd129cf8bdae065e5504f3619176aa23c2d864afffc14464df387c72edde30",
}
# 2025-01-01T00:00:00Z: a fixed timestamp for the files inside the built SDK wheel.
SOURCE_DATE_EPOCH = "1735689600"
REQ = re.compile(r"^\s*([A-Za-z0-9][A-Za-z0-9._-]*)\s*==\s*([^\s;#]+)")


def canonical(name: str) -> str:
    return re.sub(r"[-_.]+", "-", name).lower()


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def pins(requirements: Path) -> dict[str, tuple[str, str]]:
    """canonical name -> (name as written, version)."""
    out = {}
    for line in requirements.read_text(encoding="utf-8").splitlines():
        m = REQ.match(line)
        if m:
            out[canonical(m.group(1))] = (m.group(1), m.group(2))
    return out


def wheel_name_version(path: Path) -> tuple[str, str]:
    parts = path.name[: -len(".whl")].split("-")
    return canonical(parts[0]), parts[1]


def pip(*args: str, env: dict | None = None) -> None:
    subprocess.run([sys.executable, "-m", "pip", "--disable-pip-version-check", "--no-input", *args],
                   check=True, env=env)


def build_sdk_wheel(version: str, sdist_sha256: str, out: Path, work: Path) -> Path:
    sdist_dir = work / "sdk-sdist"
    pip("download", "--no-deps", "--no-binary", SDK, f"{SDK}=={version}", "-d", str(sdist_dir))
    sdists = list(sdist_dir.iterdir())
    if len(sdists) != 1:
        raise SystemExit(f"expected one {SDK} sdist, got {[p.name for p in sdists]}")
    if sha256(sdists[0]) != sdist_sha256:
        raise SystemExit(f"{sdists[0].name}: sha256 {sha256(sdists[0])} is not the pinned {sdist_sha256}")
    constraints = work / "build-constraints.txt"
    constraints.write_text(SDK_BUILD_CONSTRAINTS, encoding="utf-8")
    env = dict(os.environ, PIP_CONSTRAINT=str(constraints), SOURCE_DATE_EPOCH=SOURCE_DATE_EPOCH)
    built = work / "sdk-wheel"
    pip("wheel", "--no-deps", "-w", str(built), str(sdists[0]), env=env)
    wheels = list(built.glob("*.whl"))
    if len(wheels) != 1 or not wheels[0].name.endswith("-py3-none-any.whl"):
        raise SystemExit(f"unexpected SDK build output {[p.name for p in wheels]}")
    with zipfile.ZipFile(wheels[0]) as z:
        bundled = [n for n in z.namelist() if "/_bundled/" in n and not n.endswith((".gitignore", "/"))]
    if bundled:
        raise SystemExit(f"the built SDK wheel contains bundled files {bundled}; refusing to vendor them")
    target = out / wheels[0].name
    shutil.copyfile(wheels[0], target)
    return target


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--platform", required=True, choices=sorted(PLATFORM_TAGS))
    ap.add_argument("--out", required=True, type=Path)
    here = Path(__file__).resolve().parents[1]
    ap.add_argument("--requirements", type=Path, default=here / "requirements-runtime.txt")
    args = ap.parse_args(argv)

    pinned = pins(args.requirements)
    if canonical(SDK) not in pinned:
        raise SystemExit(f"{args.requirements} does not pin {SDK}")
    sdk_version = pinned[canonical(SDK)][1]
    if sdk_version not in SDK_SDIST_SHA256:
        raise SystemExit(f"no reviewed sdist sha256 for {SDK} {sdk_version}: add it to SDK_SDIST_SHA256")
    wheels = args.out / "wheels"
    if wheels.exists():
        shutil.rmtree(wheels)
    wheels.mkdir(parents=True)
    with tempfile.TemporaryDirectory(prefix="claude-runtime-wheels-") as tmp:
        work = Path(tmp)
        others = work / "requirements-without-sdk.txt"
        others.write_text("".join(f"{name}=={version}\n" for key, (name, version) in sorted(pinned.items()) if key != canonical(SDK)),
                          encoding="utf-8")
        # Per Python version: which packages that version needs (pip resolves markers like
        # `pywin32; sys_platform == 'win32'` for the target platform).
        needed: dict[str, set[str]] = {}
        for py in SUPPORTED_PYTHONS:
            dest = work / f"py{py}"
            abi = "cp" + py.replace(".", "")
            platform_args = [a for tag in PLATFORM_TAGS[args.platform] for a in ("--platform", tag)]
            pip("download", "--only-binary=:all:", "--implementation", "cp", "--python-version", py,
                "--abi", abi, "--abi", "abi3", "--abi", "none", *platform_args,
                "-r", str(others), "-d", str(dest))
            for wheel in dest.glob("*.whl"):
                name, version = wheel_name_version(wheel)
                if name in pinned and pinned[name][1] != version:
                    raise SystemExit(f"{wheel.name}: pinned {pinned[name][1]}")
                needed.setdefault(py, set()).add(f"{name}=={version}")
                if not (wheels / wheel.name).exists():
                    shutil.copyfile(wheel, wheels / wheel.name)
        build_sdk_wheel(sdk_version, SDK_SDIST_SHA256[sdk_version], wheels, work)

    hashes: dict[str, list[str]] = {}
    versions: dict[str, str] = {}
    for wheel in sorted(wheels.glob("*.whl")):
        name, version = wheel_name_version(wheel)
        if versions.setdefault(name, version) != version:
            raise SystemExit(f"two versions of {name} vendored")
        hashes.setdefault(name, []).append(sha256(wheel))
    lines = [
        "# Generated by scripts/vendor_wheels.py: the Claude runtime's dependencies for "
        f"{args.platform}, vendored in wheels/.",
        f"# python-versions: {' '.join(SUPPORTED_PYTHONS)}",
        "# Install: python -m pip install --no-index --find-links wheels --require-hashes -r requirements.lock",
    ]
    for name in sorted(versions):
        spec = f"{name}=={versions[name]}"
        users = [py for py in SUPPORTED_PYTHONS if spec in needed.get(py, set())]
        marker = ""
        if name != canonical(SDK) and len(users) != len(SUPPORTED_PYTHONS):
            marker = " ; " + " or ".join(f'python_version == "{py}"' for py in users)
        hash_args = " ".join(f"--hash=sha256:{h}" for h in sorted(hashes[name]))
        lines.append(f"{spec}{marker} {hash_args}")
    (args.out / "requirements.lock").write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"vendored {sum(len(h) for h in hashes.values())} wheels of {len(versions)} packages for {args.platform}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
