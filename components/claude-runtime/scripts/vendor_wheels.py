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
- claude-agent-sdk: default builds use the MIT SDK sdist and refuse bundled binaries. With the
  explicit `--bundle-claude-cli` legal-review flag, use Anthropic's platform wheel unchanged;
  it contains the version-matched official Claude Code CLI and its exact sha256 is pinned.
- The other wheels are third-party packages redistributed under their own licenses (each wheel
  carries its license in its .dist-info).
"""

from __future__ import annotations

import argparse
import email.parser
import hashlib
import json
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
TARGET_ONLY = {
    # pip evaluates environment markers against the build host even with --platform. Keep the
    # platform split explicit and download a complete, pinned closure with --no-deps.
    "pywin32": {"windows-x64"},
}
SDK = "claude-agent-sdk"
SDK_BUILD_CONSTRAINTS = """hatchling==1.32.4
packaging==26.3
pathspec==1.1.1
pluggy==1.7.0
tomlkit==0.15.1
trove-classifiers==2026.9.21.13
"""
SDK_SDIST_SHA256 = {
    "0.2.166": "6dbd129cf8bdae065e5504f3619176aa23c2d864afffc14464df387c72edde30",
}
SOURCE_DATE_EPOCH = "1735689600"
# sha256 of Anthropic's platform wheels on PyPI. A new SDK pin needs all three wheels reviewed.
SDK_WHEEL_SHA256 = {
    ("0.2.166", "windows-x64"): "d53ba0e8152c88b2e9eaf61d684b2ed8621e3785cdc9f4be5378a6d0f86304be",
    ("0.2.166", "linux-x64"): "81d34634ef4fb4c0782fd7d5354de1b558b776e399a9be3aca771cc528c7ad2e",
    ("0.2.166", "darwin-arm64"): "35f2082ba76d851af76954e8a19c0bd9d5b81a49a3efc7864e2d9e6ff393e743",
}
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
        raise SystemExit(f"expected one {SDK} sdist, got {[path.name for path in sdists]}")
    actual = sha256(sdists[0])
    if actual != sdist_sha256:
        raise SystemExit(f"{sdists[0].name}: sha256 {actual} is not the pinned {sdist_sha256}")
    constraints = work / "build-constraints.txt"
    constraints.write_text(SDK_BUILD_CONSTRAINTS, encoding="utf-8")
    env = dict(os.environ, PIP_CONSTRAINT=str(constraints), SOURCE_DATE_EPOCH=SOURCE_DATE_EPOCH)
    built = work / "sdk-wheel"
    pip("wheel", "--no-deps", "-w", str(built), str(sdists[0]), env=env)
    wheels = list(built.glob("*.whl"))
    if len(wheels) != 1 or not wheels[0].name.endswith("-py3-none-any.whl"):
        raise SystemExit(f"unexpected SDK build output {[path.name for path in wheels]}")
    with zipfile.ZipFile(wheels[0]) as bundle:
        bundled = [name for name in bundle.namelist() if "/_bundled/" in name and not name.endswith((".gitignore", "/"))]
    if bundled:
        raise SystemExit(f"the built SDK wheel contains bundled files {bundled}; refusing to vendor them")
    target = out / wheels[0].name
    shutil.copyfile(wheels[0], target)
    return target


def download_sdk_wheel(platform: str, version: str, expected_sha256: str, out: Path, work: Path) -> Path:
    downloaded = work / "sdk-wheel"
    platform_args = [arg for tag in PLATFORM_TAGS[platform] for arg in ("--platform", tag)]
    pip("download", "--no-deps", "--only-binary=:all:", "--implementation", "cp",
        "--python-version", "3.13", "--abi", "none", *platform_args,
        f"{SDK}=={version}", "-d", str(downloaded))
    wheels = list(downloaded.glob("*.whl"))
    if len(wheels) != 1:
        raise SystemExit(f"expected one {SDK} wheel, got {[path.name for path in wheels]}")
    wheel = wheels[0]
    actual = sha256(wheel)
    if actual != expected_sha256:
        raise SystemExit(f"{wheel.name}: sha256 {actual} is not the pinned {expected_sha256}")
    expected_cli = "claude_agent_sdk/_bundled/claude.exe" if platform == "windows-x64" else "claude_agent_sdk/_bundled/claude"
    with zipfile.ZipFile(wheel) as bundle:
        bundled = [name for name in bundle.namelist() if "/_bundled/" in name and not name.endswith((".gitignore", "/"))]
        if bundled != [expected_cli]:
            raise SystemExit(f"{wheel.name}: expected only {expected_cli}, got {bundled}")
        if bundle.getinfo(expected_cli).file_size < 1_000_000:
            raise SystemExit(f"{wheel.name}: bundled Claude CLI is unexpectedly small")
        licenses = [name for name in bundle.namelist() if ".dist-info/licenses/" in name and not name.endswith("/")]
        if not licenses:
            raise SystemExit(f"{wheel.name}: the SDK wheel has no packaged license")
    target = out / wheel.name
    shutil.copyfile(wheel, target)
    return target


def validate_wheel_closure(platform: str, wheels: Path, pinned: dict[str, tuple[str, str]]) -> None:
    """Prove every active Requires-Dist is present and satisfies the exact pin."""
    from pip._vendor.packaging.markers import default_environment
    from pip._vendor.packaging.requirements import Requirement

    metadata: dict[str, list[str]] = {}
    for wheel in sorted(wheels.glob("*.whl")):
        name, _ = wheel_name_version(wheel)
        if name in metadata:
            continue
        with zipfile.ZipFile(wheel) as bundle:
            candidates = [entry for entry in bundle.namelist() if entry.endswith(".dist-info/METADATA")]
            if len(candidates) != 1:
                raise SystemExit(f"{wheel.name}: expected one METADATA file, got {candidates}")
            message = email.parser.BytesParser().parsebytes(bundle.read(candidates[0]))
            metadata[name] = message.get_all("Requires-Dist", [])

    target = {
        "windows-x64": {"sys_platform": "win32", "os_name": "nt", "platform_system": "Windows", "platform_machine": "AMD64"},
        "linux-x64": {"sys_platform": "linux", "os_name": "posix", "platform_system": "Linux", "platform_machine": "x86_64"},
        "darwin-arm64": {"sys_platform": "darwin", "os_name": "posix", "platform_system": "Darwin", "platform_machine": "arm64"},
    }[platform]
    for py in SUPPORTED_PYTHONS:
        env = default_environment()
        env.update(target)
        env.update({"python_version": py, "python_full_version": f"{py}.0", "extra": ""})
        for parent, requirements in metadata.items():
            for text in requirements:
                requirement = Requirement(text)
                if requirement.marker is not None and not requirement.marker.evaluate(env):
                    continue
                child = canonical(requirement.name)
                if child not in pinned:
                    raise SystemExit(f"{parent}: active dependency {text!r} for {platform} Python {py} is not pinned")
                version = pinned[child][1]
                if requirement.specifier and not requirement.specifier.contains(version, prereleases=True):
                    raise SystemExit(f"{parent}: {text!r} rejects pinned {child}=={version}")


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--platform", required=True, choices=sorted(PLATFORM_TAGS))
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument(
        "--bundle-claude-cli",
        action="store_true",
        help="include Anthropic's unmodified platform wheel and Claude Code binary after legal review",
    )
    here = Path(__file__).resolve().parents[1]
    ap.add_argument("--requirements", type=Path, default=here / "requirements-runtime.txt")
    args = ap.parse_args(argv)

    pinned = pins(args.requirements)
    if canonical(SDK) not in pinned:
        raise SystemExit(f"{args.requirements} does not pin {SDK}")
    sdk_version = pinned[canonical(SDK)][1]
    if args.bundle_claude_cli:
        sdk_hash = SDK_WHEEL_SHA256.get((sdk_version, args.platform))
        if sdk_hash is None:
            raise SystemExit(f"no reviewed wheel sha256 for {SDK} {sdk_version} on {args.platform}: add it to SDK_WHEEL_SHA256")
    elif sdk_version not in SDK_SDIST_SHA256:
        raise SystemExit(f"no reviewed sdist sha256 for {SDK} {sdk_version}: add it to SDK_SDIST_SHA256")
    wheels = args.out / "wheels"
    if wheels.exists():
        shutil.rmtree(wheels)
    wheels.mkdir(parents=True)
    with tempfile.TemporaryDirectory(prefix="claude-runtime-wheels-") as tmp:
        work = Path(tmp)
        others = work / "requirements-without-sdk.txt"
        others.write_text(
            "".join(
                f"{name}=={version}\n"
                for key, (name, version) in sorted(pinned.items())
                if key != canonical(SDK) and args.platform in TARGET_ONLY.get(key, {args.platform})
            ),
            encoding="utf-8",
        )
        # Per Python version: which packages that version needs (pip resolves markers like
        # `pywin32; sys_platform == 'win32'` for the target platform).
        needed: dict[str, set[str]] = {}
        for py in SUPPORTED_PYTHONS:
            dest = work / f"py{py}"
            abi = "cp" + py.replace(".", "")
            platform_args = [a for tag in PLATFORM_TAGS[args.platform] for a in ("--platform", tag)]
            pip("download", "--no-deps", "--only-binary=:all:", "--implementation", "cp", "--python-version", py,
                "--abi", abi, "--abi", "abi3", "--abi", "none", *platform_args,
                "-r", str(others), "-d", str(dest))
            for wheel in dest.glob("*.whl"):
                name, version = wheel_name_version(wheel)
                if name in pinned and pinned[name][1] != version:
                    raise SystemExit(f"{wheel.name}: pinned {pinned[name][1]}")
                needed.setdefault(py, set()).add(f"{name}=={version}")
                if not (wheels / wheel.name).exists():
                    shutil.copyfile(wheel, wheels / wheel.name)
        if args.bundle_claude_cli:
            download_sdk_wheel(args.platform, sdk_version, sdk_hash, wheels, work)
            (args.out / "claude-cli-runtime.json").write_text(
                json.dumps({"schema": "cutex/claude-cli-runtime/v1", "sdk": sdk_version, "bundled": True}, indent=2) + "\n",
                encoding="utf-8",
            )
        else:
            build_sdk_wheel(sdk_version, SDK_SDIST_SHA256[sdk_version], wheels, work)
            (args.out / "claude-cli-runtime.json").unlink(missing_ok=True)

    hashes: dict[str, list[str]] = {}
    versions: dict[str, str] = {}
    for wheel in sorted(wheels.glob("*.whl")):
        name, version = wheel_name_version(wheel)
        if versions.setdefault(name, version) != version:
            raise SystemExit(f"two versions of {name} vendored")
        hashes.setdefault(name, []).append(sha256(wheel))
    validate_wheel_closure(args.platform, wheels, pinned)
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
