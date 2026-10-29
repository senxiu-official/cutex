#!/usr/bin/env python3
"""Backend upgrade debug drill (default public 0.1.5 -> 0.2.0).

    python upgrade-e2e.py --from-cutex DIR --from-commit SHA --to-cutex DIR --to-commit SHA
        --components DIR --work DIR --release-meta DIR --confirm-one-way 0.2.0
        [--new-native none|stub|DIR] [--service-name LABEL] [--unit-dir DIR]
        [--claude-python PATH] [--rustc RUSTC] [--generator upgrade-manifest.py] [--no-cleanup]

Both cutex directories must contain DEBUG cutex + cutex-mcp, built with the corresponding
CUTEX_RELEASE / CUTEX_COMMIT / CUTEX_CHANNEL=release identity. Only debug binaries trust
CUTEX_UPGRADE_TEST_PUBKEY and honour private-home / drill-service-name overrides. The
service drill does not accept formally signed public binaries as replacements for these.
Historical function names ending 010/011 mean the installed/target release respectively.

Build component archives using the target source's three-argument component build.sh.
--release-meta supplies <target>.json and capabilities-<installed>.json, exactly as the
real signing generator uses them. Declared capability removals are allowed; undeclared
losses fail. A one-way target requires --confirm-one-way RELEASE. The drill verifies the
installed updater refuses an unconfirmed apply before applying with explicit consent.
--new-native accepts none, a second stub, or a directory containing this platform's real
native archive with embedded cute-codex-build.json. Natives are never executed.
Published downloads use signed-native-preflight with the committed release public key;
release tag and archive version are separate inputs; r58 actually embeds 0.164.0-r58.

The drill creates a throwaway signing key, synthetic release archives and stub native,
serves the releases on a random loopback port, and installs under --work with a marked
private home. Linux uses drill systemd user units (--unit-dir defaults to the user runtime
dir; CI needs linger). Windows uses a drill scheduled task and temporarily writes private
home variables into HKCU Environment, restoring them on cleanup. A supervisor fallback
is reported if the runner cannot launch the task. Never run this service drill on a real
installation with release binaries: they ignore its isolation overrides.

After check/stage/apply, verify installed version, runtime selection for new agents,
capabilities, components and service readiness. A separate synthetic higher release fails
to start its daemons and must roll back to the target. That synthetic release requests a
full restart but no new migration; the real target retains its one-way migration metadata.
This tests rollback AFTER the target, never rollback across the one-way target migration.
Logs scrub tokens and omit config/accounts files. Cleanup uninstalls only drill services.
The empty home does not prove migration of pre-existing nonempty asks/delegation stores.

Read-only public-asset verification (no executable, service or registry writes):
    python upgrade-e2e.py signed-asset-preflight --assets DIR --public-key TRUSTED_KEY
        [--release 0.1.5] [--platform linux-x64|windows-x64]
This verifies actual signed assets, not real-binary upgrade acceptance. Its --help explains
the disposable-VM acceptance path and the remaining formal 0.2.0 signing dependency.

Needs Python 3.10+ with cryptography (CI pins it), rustc for the broken stub and Node >=24.9
for component builds. Choose a --work path with a space; Windows private homes need an
owner-private directory under the test user's profile.
"""

from __future__ import annotations

import argparse
from contextlib import closing
import base64
import hashlib
import datetime as dt
import socket
import sqlite3
from cryptography.exceptions import InvalidSignature
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import time
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent


def _load(name: str, file_name: str):
    spec = importlib.util.spec_from_file_location(name, HERE / file_name)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


ce = _load("component_e2e", "component-e2e.py")  # TestKey, sha256, serve, base_path

WINDOWS = os.name == "nt"
PLATFORM = "windows-x64" if WINDOWS else "linux-x64"
EXE = ".exe" if WINDOWS else ""
MARKER = ".cutex-test-private-home"
NATIVE_VERSION = "0.0.0-e2e-stub"
NEW_NATIVE_STUB = "0.0.1-e2e-stub"
BROKEN_LABEL = "999.0.0-e2e-broken"
# Capabilities that follow a live component (cutex runner.rs HOST_DEPENDENT_CAPABILITIES).
HOST_DEPENDENT = {"job-external", "job-source-nci"}
DROP_ENV = re.compile(r"^(ANTHROPIC_|CLAUDE_|CUTEX_)")
VERSION_LINE = re.compile(r"\((?P<label>[A-Za-z0-9._-]+), (?P<commit7>[0-9a-f]{7})\)")
READY_TIMEOUT = 240

failures: list[str] = []
warnings: list[str] = []


def log(message: str) -> None:
    print(message, flush=True)


def check(condition: bool, message: str) -> bool:
    log(("  ok    " if condition else "  FAIL  ") + message)
    if not condition:
        failures.append(message)
    return condition


def warn(message: str) -> None:
    log(f"  WARN  {message}")
    warnings.append(message)
    if os.environ.get("GITHUB_ACTIONS"):
        print(f"::warning::{message}", flush=True)


def phase(title: str) -> None:
    log(f"\n== {title}")


# ------------------------------------------------------------ builds / archives


def version_line(exe: Path) -> tuple[str, str, str]:
    out = subprocess.run([str(exe), "--version"], capture_output=True, text=True, timeout=120).stdout.strip()
    m = VERSION_LINE.search(out)
    if not m:
        raise SystemExit(f"{exe} --version does not name (<release>, <commit7>): {out!r} (build with CUTEX_RELEASE / CUTEX_COMMIT)")
    return out, m["label"], m["commit7"]


def capabilities(exe: Path, scratch: Path) -> list[str]:
    """featureList of `cutex management schema export` (no daemon), as build-cutex.sh records it."""
    home = scratch / "home"
    home.mkdir(parents=True, exist_ok=True)
    (home / MARKER).touch()
    out = scratch / "schema"
    env = {k: v for k, v in os.environ.items() if not DROP_ENV.match(k)}
    env.update({"HOME": str(home), "CUTEX_TEST_PRIVATE_HOME": str(home), "CUTEX_TEST_FORBID_REAL_HOME": "1"})
    subprocess.run([str(exe), "management", "schema", "export", "--out", str(out)], env=env, check=True,
                   capture_output=True, timeout=300)
    caps = sorted(json.loads((out / "index.json").read_text(encoding="utf-8"))["capabilities"])
    if not caps:
        raise SystemExit(f"{exe}: the schema export lists no capabilities")
    return caps


def pack(out: Path, files: dict[str, Path]) -> None:
    """ustar .tar.gz, regular files at the root (src/upgrade/archive.rs reads exactly this)."""
    out.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(out, "w:gz", compresslevel=1, format=tarfile.USTAR_FORMAT) as tar:
        for name, path in files.items():
            info = tar.gettarinfo(str(path), arcname=name)
            info.mode, info.uid, info.gid, info.uname, info.gname, info.mtime = 0o755, 0, 0, "", "", 1735689600
            with path.open("rb") as f:
                tar.addfile(info, f)


def write_sums(directory: Path, archive: Path, sums_name: str) -> None:
    (directory / sums_name).write_text(f"{ce.sha256(archive)}  {archive.name}\n", encoding="utf-8")


def cutex_build(out: Path, label: str, commit: str, line: str, caps: list[str], files: dict[str, Path]) -> Path:
    """A `cutex-<platform>` build dir as build-cutex.sh writes it."""
    out.mkdir(parents=True, exist_ok=True)
    archive = out / f"cutex-{label}-{PLATFORM}.tar.gz"
    pack(archive, files)
    info = {"release": label, "commit": commit, "channel": "release", "builtAt": "2025-01-01T00:00:00Z",
            "platform": PLATFORM, "versionLine": line, "capabilities": caps}
    (out / f"BUILD-INFO-{PLATFORM}.json").write_text(json.dumps(info, indent=2), encoding="utf-8")
    write_sums(out, archive, f"SHA256SUMS-{PLATFORM}")
    return archive


NOTICE_FILES = ("LICENSE", "NOTICE", "THIRD_PARTY_NOTICES.md")


def notice_files(directory: Path) -> dict[str, Path]:
    """Stand-ins for the license files build-cutex.sh / build-natives.sh put next to the executables. The
    FROM build's updater must stage an archive that carries them."""
    directory.mkdir(parents=True, exist_ok=True)
    files = {}
    for name in NOTICE_FILES:
        (directory / name).write_text(f"upgrade e2e stand-in for {name}\n", encoding="utf-8")
        files[name] = directory / name
    return files


def native_build(out: Path, version: str = NATIVE_VERSION, notices: bool = False) -> Path:
    """A stub native: the updater stages, hashes and selects these files; nothing executes them here."""
    stubs = out / "files"
    stubs.mkdir(parents=True)
    files = notice_files(stubs) if notices else {}
    for name in (f"codex-app-server{EXE}", f"codex-code-mode-host{EXE}", f"codex{EXE}"):
        (stubs / name).write_text(f"#!/bin/sh\n# cutex upgrade e2e stub {name} {version}: never run\nexit 1\n", encoding="utf-8")
        files[name] = stubs / name
    schema = stubs / "codex_app_server_protocol.schemas.json"
    schema.write_text("{}\n", encoding="utf-8")
    files[schema.name] = schema
    archive = out / f"native-{version}-{PLATFORM}.tar.gz"
    pack(archive, files)
    commit = "0" * 40 if version == NATIVE_VERSION else "1" * 40
    info = {"version": version, "commit": commit, "platform": PLATFORM}
    (out / f"BUILD-INFO-{PLATFORM}.json").write_text(json.dumps(info, indent=2), encoding="utf-8")
    write_sums(out, archive, f"SHA256SUMS-{PLATFORM}")
    return archive


def new_native_build(spec: str, out: Path) -> tuple[Path, str] | None:
    """The native the new release brings (--new-native), as a `natives-<platform>` build dir for the
    generator's --native-dir: None, a second stub, or a real archive of this platform."""
    if spec == "none":
        return None
    if spec == "stub":
        return native_build(out, NEW_NATIVE_STUB, notices=True), NEW_NATIVE_STUB
    given = Path(spec)
    archives = sorted(given.glob(f"native-*-{PLATFORM}.tar.gz"))
    if len(archives) != 1:
        raise SystemExit(f"--new-native {given}: expected one native-*-{PLATFORM}.tar.gz, found {len(archives)}")
    info = native_archive_info(archives[0], PLATFORM)
    info_path = given / f"BUILD-INFO-{PLATFORM}.json"
    if info_path.is_file() and json.loads(info_path.read_text(encoding="utf-8")) != info:
        raise SystemExit(f"{info_path}: differs from the archive's embedded build record")
    version = info["version"]
    if version in (NATIVE_VERSION, NEW_NATIVE_STUB):
        raise SystemExit(f"--new-native: {version} is a stub version")
    with tarfile.open(archives[0]) as tar:
        names = set(tar.getnames())
    missing = sorted({f"codex-app-server{EXE}", f"codex-code-mode-host{EXE}", "codex_app_server_protocol.schemas.json"} - names)
    if missing:
        raise SystemExit(f"{archives[0]}: missing {missing}")
    out.mkdir(parents=True, exist_ok=True)
    archive = out / archives[0].name
    shutil.copyfile(archives[0], archive)
    (out / f"BUILD-INFO-{PLATFORM}.json").write_text(json.dumps(info, indent=2), encoding="utf-8")
    write_sums(out, archive, f"SHA256SUMS-{PLATFORM}")
    log(f"  new native {version} ({str(info.get('commit', ''))[:12]}) from {archives[0]}")
    return archive, version


BROKEN_RS = r"""
// cutex upgrade e2e: a release whose daemons never come up. `--version` names the release; every other
// command runs the real binary (cutex.real next to this file, else in the release dir: on Windows the
// services run bin\cutex.exe, a copy of this stub), except `management serve` / `agent serve`, which
// exit at once.
use std::path::PathBuf;
use std::process::{exit, Command};

const VERSION_LINE: &str = "@VERSION_LINE@";
const LABEL: &str = "@LABEL@";
const REAL: &str = "@REAL@";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && args[0] == "--version" {
        println!("{VERSION_LINE}");
        return;
    }
    // Neither daemon starts (a delegated daemon would outlive this stub on Windows when the
    // supervisor stops it by PID).
    if args.len() >= 2 && ((args[0] == "management" && args[1] == "serve") || (args[0] == "agent" && args[1] == "serve")) {
        eprintln!("cutex upgrade e2e: broken release {LABEL}: {} {} exits", args[0], args[1]);
        exit(3);
    }
    let exe = std::env::current_exe().expect("current_exe");
    let mut real = exe.with_file_name(REAL);
    if !real.is_file() {
        if let Some(base) = std::env::var_os("CUTEX_INSTALL_BASE") {
            real = PathBuf::from(base).join("releases").join(LABEL).join(REAL);
        }
    }
    match Command::new(&real).args(&args).status() {
        Ok(status) => exit(status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!("cutex upgrade e2e stub: cannot run {}: {error}", real.display());
            exit(1);
        }
    }
}
"""


def broken_stub(out: Path, line: str, rustc: str) -> Path:
    src = out / "broken-stub.rs"
    src.parent.mkdir(parents=True, exist_ok=True)
    src.write_text(BROKEN_RS.replace("@VERSION_LINE@", line).replace("@LABEL@", BROKEN_LABEL)
                   .replace("@REAL@", f"cutex.real{EXE}"), encoding="utf-8")
    exe = out / f"cutex{EXE}"
    subprocess.run([rustc, "-O", "-o", str(exe), str(src)], check=True, cwd=str(out))
    return exe


def bundle_v4(native_dir: Path, facade: Path, shared_config: Path, commit: str) -> dict:
    """releases/<r>/manifests/default.json as AGENT-INSTALL.md 5.3 writes it (version 4 pins)."""
    def pin(path: Path) -> dict:
        path = Path(os.path.realpath(path))
        return {"path": str(path), "sha256": ce.sha256(path)}
    return {
        "version": 4,
        "upstream_commit": commit,
        "native_patch_commit": commit,
        "executable": pin(native_dir / f"codex-app-server{EXE}"),
        "cli": pin(native_dir / f"codex{EXE}"),
        "code_mode_host": pin(native_dir / f"codex-code-mode-host{EXE}"),
        "facade": pin(facade),
        "schema": pin(native_dir / "codex_app_server_protocol.schemas.json"),
        "shared_config": pin(shared_config),
    }


def _plain(path: str | Path) -> str:
    """Path text without the Windows verbatim prefix cutex writes for canonical paths (\\?\\C:\\…, \\?\\UNC\\…)."""
    text = str(path)
    if text.startswith("\\\\?\\UNC\\"):
        return "\\\\" + text[8:]
    if text.startswith("\\\\?\\"):
        return text[4:]
    return text


def same_path(a: str | Path | None, b: Path) -> bool:
    return bool(a) and Path(_plain(a)).resolve() == Path(_plain(b)).resolve()


# ------------------------------------------------------------- the release


def find_generator(given: str | None) -> Path:
    for p in ([Path(given)] if given else []) + [HERE / "upgrade-manifest.py", HERE.parents[2] / "upgrade-manifest.py"]:
        if p.is_file():
            return p.resolve()
    raise SystemExit("upgrade-manifest.py not found (--generator)")


def generate(generator: Path, *args: str) -> None:
    log(f"$ upgrade-manifest.py {' '.join(args)}")
    p = subprocess.run([sys.executable, str(generator), *args], capture_output=True, text=True)
    log("  " + (p.stdout + p.stderr).strip().replace("\n", "\n  "))
    if p.returncode != 0:
        raise SystemExit(f"upgrade-manifest.py failed ({p.returncode})")


class Release:
    def __init__(self, work: Path, key) -> None:
        self.served = work / "served"
        self.key = key

    def put(self, tag: str, path: Path) -> None:
        (self.served / tag).mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, self.served / tag / path.name)

    def sign_into(self, tag: str, path: Path) -> None:
        self.put(tag, path)
        data = path.read_bytes()
        (self.served / tag / f"{path.name}.minisig").write_text(self.key.sign(data, path.name), encoding="utf-8")

    def manifest(self, manifest_dir: Path) -> dict:
        doc = json.loads((manifest_dir / "manifest.json").read_text(encoding="utf-8"))
        self.sign_into(f"cutex-{doc['cutex']['release']}", manifest_dir / "manifest.json")
        return doc

    def channel(self, generator: Path, manifest_dir: Path, out: Path) -> None:
        generate(generator, "channel", "--manifest", str(manifest_dir / "manifest.json"), "--out", str(out))
        self.sign_into("channel-release", out / "channel.json")


def manifest_010(label: str, commit: str, archive: Path, native: Path) -> dict:
    """0.1.0 as published (sequence 1, no components); the generator refuses sequence 1."""
    return {
        "schema": "cutex/release-manifest/v1",
        "channel": "release",
        "sequence": 1,
        "publishedAt": "2026-10-15T16:04:18Z",
        "notes": {},
        "cutex": {
            "release": label,
            "commit": commit,
            "capabilities": {"added": [], "removed": []},
            "migrations": [],
            "artifacts": {PLATFORM: {"name": archive.name, "sha256": ce.sha256(archive), "size": archive.stat().st_size}},
        },
        "native": {
            "version": NATIVE_VERSION,
            "commit": "0" * 40,
            "artifacts": {PLATFORM: {"tag": f"native-{NATIVE_VERSION}", "name": native.name, "sha256": ce.sha256(native),
                                     "size": native.stat().st_size}},
        },
        "desktop": {"branch": "release"},
        "mode": "full",
        "packageRunRecommended": True,
    }


# ---------------------------------------------------------------- the host


def http(port: int, token: str, method: str, path: str, body: dict | None = None, timeout: float = 10) -> tuple[int, dict | None]:
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=data, method=method,
                                     headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            raw = response.read()
            return response.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as error:
        raw = error.read()
        try:
            return error.code, json.loads(raw)
        except ValueError:
            return error.code, None
    except (OSError, ValueError):
        return 0, None


def features(value: dict | None) -> set[str]:
    return set((value or {}).get("featureList") or [])


class Host:
    """The private home, the install base and the CLI of the installed release."""

    def __init__(self, args: argparse.Namespace, work: Path, key, url: str) -> None:
        self.args = args
        self.work = work
        self.home = work / "home"
        self.base = work / "base"
        self.label = args.service_name
        self.home.mkdir(parents=True)
        (self.home / MARKER).touch()
        cutex_dir = self.home / ".cutex"
        cutex_dir.mkdir()
        self.port, self.bus_port = args.management_port, args.bus_port
        self.token = os.urandom(32).hex()
        self.bus_token = os.urandom(32).hex()
        config = {"agent_bus_enabled": True, "agent_bus_token": self.bus_token, "management_api_token": self.token,
                  "management_port": self.port, "agent_bus_port": self.bus_port}
        (cutex_dir / "config.json").write_text(json.dumps(config), encoding="utf-8")
        if not WINDOWS:
            (cutex_dir / "config.json").chmod(0o600)
        # The source before anything starts: the daemon's first scheduled check would otherwise read the
        # public channel (and, once 0.1.1 is public, stage it with the embedded key).
        settings = self.base / "upgrade" / "settings.json"
        settings.parent.mkdir(parents=True)
        settings.write_text(json.dumps({"schema": "cutex/upgrade-settings/v1", "source": url, "checkIntervalHours": 0}),
                            encoding="utf-8")
        self.private_env = {
            "HOME": str(self.home),
            "CUTEX_TEST_PRIVATE_HOME": str(self.home),
            "CUTEX_TEST_FORBID_REAL_HOME": "1",
            "CUTEX_UPGRADE_TEST_PUBKEY": key.public_line,
            "CUTEX_SERVICE_TEST_NAME": self.label,
        }
        self.unit_dir: Path | None = None
        if not WINDOWS:
            runtime = os.environ.get("XDG_RUNTIME_DIR") or f"/run/user/{os.getuid()}"
            self.unit_dir = Path(args.unit_dir or f"{runtime}/systemd/user")
            self.private_env["CUTEX_COMPONENT_TEST_UNIT_DIR"] = str(self.unit_dir)
        env = {k: v for k, v in os.environ.items() if not DROP_ENV.match(k)}
        env.update(self.private_env)
        env["NO_COLOR"] = "1"
        self.env = env
        self.saved_registry: dict[str, tuple[str, int] | None] = {}
        self.supervisor_by_harness: subprocess.Popen | None = None
        self.native_home_before: str | None = None

    # -- names

    @property
    def bus_unit(self) -> str:
        return f"cutex-{self.label}-agent-bus.service"

    @property
    def management_unit(self) -> str:
        return f"cutex-{self.label}-management-api.service"

    @property
    def job_unit(self) -> str:
        return f"cutex-{self.label}-job-service.service"

    @property
    def task_name(self) -> str:
        return f"\\Cutex-{self.label}\\cutex-supervisor"

    @property
    def installed_cli(self) -> Path:
        return self.base / ("bin" if WINDOWS else "current") / f"cutex{EXE}"

    # -- running cutex

    def run(self, exe: Path, *args: str, extra: dict[str, str] | None = None, path: str | None = None,
            timeout: int = 1800) -> tuple[int, str, str]:
        env = dict(self.env)
        env.update(extra or {})
        if path is not None:
            env["PATH"] = path
        log(f"$ {exe.name if exe.parent.name != 'current' else 'current/' + exe.name} {' '.join(args)}")
        p = subprocess.run([str(exe), *args], env=env, capture_output=True, text=True, encoding="utf-8",
                           errors="replace", timeout=timeout)
        return p.returncode, p.stdout, p.stderr

    def cli(self, *args: str, ok: bool = True, show: bool = False, **kw) -> tuple[int, dict | None, str]:
        code, out, err = self.run(self.installed_cli, *args, **kw)
        value = None
        start = out.find("{")
        if start >= 0:
            try:
                value = json.loads(out[start:])
            except ValueError:
                value = None
        if show or code != 0:
            log("  " + out.strip()[-6000:].replace("\n", "\n  "))
            if err.strip():
                log("  stderr: " + err.strip()[-4000:].replace("\n", "\n  "))
        if ok:
            check(code == 0, f"cutex {' '.join(args)} exits 0 (got {code})")
        return code, value, out

    def get(self, path: str) -> tuple[int, dict | None]:
        return http(self.port, self.token, "GET", path)

    def wait_ready(self, release: str, timeout: int = READY_TIMEOUT) -> dict | None:
        deadline = time.time() + timeout
        last = None
        while time.time() < deadline:
            status, host = self.get("/v2/host")
            last = (status, (host or {}).get("cutexBuild"))
            if status == 200 and ((host or {}).get("cutexBuild") or {}).get("release") == release:
                return host
            time.sleep(2)
        log(f"  last /v2/host answer: {last}")
        return None

    def selected_runtime(self) -> dict:
        """runtime/light/deployment.json: the runtime bundle NEW agents start with."""
        try:
            return json.loads((self.home / ".cutex" / "runtime" / "light" / "deployment.json").read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return {}

    def install_record(self) -> dict:
        try:
            return json.loads((self.base / "install.json").read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return {}

    def systemctl(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(["systemctl", "--user", *args], capture_output=True, text=True)

    def active(self, unit: str) -> bool:
        return self.systemctl("is-active", "--quiet", unit).returncode == 0

    def supervisor_status(self) -> dict:
        try:
            return json.loads((self.base / "run" / "supervisor.json").read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return {}

    # -- platform plumbing

    def prepare_service_manager(self) -> None:
        if WINDOWS:
            import winreg
            with winreg.OpenKey(winreg.HKEY_CURRENT_USER, "Environment", 0, winreg.KEY_ALL_ACCESS) as key:
                for name, value in self.private_env.items():
                    try:
                        self.saved_registry[name] = winreg.QueryValueEx(key, name)
                    except FileNotFoundError:
                        self.saved_registry[name] = None
                    winreg.SetValueEx(key, name, 0, winreg.REG_SZ, value)
            log(f"  HKCU\\Environment: {', '.join(self.private_env)} set for the scheduled task")
            return
        assert self.unit_dir is not None
        self.unit_dir.mkdir(parents=True, exist_ok=True)
        for unit in (self.bus_unit, self.management_unit):
            if (self.unit_dir / unit).exists():
                raise SystemExit(f"{self.unit_dir / unit} exists: pick another --service-name")
        user_dir = self.home / ".config" / "systemd" / "user"
        user_dir.parent.mkdir(parents=True)
        user_dir.symlink_to(self.unit_dir, target_is_directory=True)
        lines = "".join(f"Environment={systemd_quote(f'{k}={v}')}\n" for k, v in self.private_env.items())
        for unit in (self.bus_unit, self.management_unit):
            d = self.unit_dir / f"{unit}.d"
            d.mkdir(exist_ok=True)
            (d / "50-cutex-upgrade-e2e.conf").write_text(
                "# cutex upgrade e2e: the drill units run in the test's private home\n[Service]\n" + lines, encoding="utf-8")
        log(f"  {user_dir} -> {self.unit_dir}; drop-ins with the private home for {self.bus_unit}, {self.management_unit}")

    def ensure_supervisor(self, release: str, timeout: int = 90) -> None:
        """Windows: the scheduled task should have started the supervisor, and its daemons should answer in
        the private home (the task gets HKCU\\Environment). Otherwise stop what the task started and run the
        supervisor with the task's command line from here (reported as a warning: the task start is then
        not covered)."""
        deadline = time.time() + timeout
        while time.time() < deadline and not self.supervisor_status().get("pid"):
            time.sleep(2)
        status = self.supervisor_status()
        if status.get("pid"):
            log(f"  supervisor started by the scheduled task (pid {status.get('pid')})")
            if self.wait_ready(release, timeout=150):
                log("  its daemons answer in the private home")
                return
            warn(f"the supervisor the task {self.task_name} started did not bring up {release} in the private home "
                 f"(supervisor.json {json.dumps(status)[:600]})")
            for pid in {status.get("pid"), (status.get("bus") or {}).get("pid"), (status.get("management") or {}).get("pid")}:
                if pid:
                    subprocess.run(["taskkill", "/F", "/T", "/PID", str(pid)], capture_output=True)
            time.sleep(3)
        else:
            warn(f"the scheduled task {self.task_name} did not start the supervisor within {timeout} s")
        query = subprocess.run(["schtasks", "/Query", "/TN", self.task_name, "/V", "/FO", "LIST"], capture_output=True,
                               text=True, errors="replace")
        log("  " + query.stdout.strip().replace("\n", "\n  "))
        warn("starting `bin\\cutex.exe service run` from the test instead (the task start is not covered by this run)")
        log_file = (self.work / "supervisor-by-harness.log").open("ab")
        flags = 0x00000008 | 0x00000200  # DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        command = [str(self.installed_cli), "service", "run"]
        try:
            self.supervisor_by_harness = subprocess.Popen(command, env=self.env, stdin=subprocess.DEVNULL, stdout=log_file,
                                                          stderr=log_file, creationflags=flags | 0x01000000,  # + BREAKAWAY_FROM_JOB
                                                          cwd=str(self.base))
        except OSError:
            self.supervisor_by_harness = subprocess.Popen(command, env=self.env, stdin=subprocess.DEVNULL, stdout=log_file,
                                                          stderr=log_file, creationflags=flags, cwd=str(self.base))

    def services_up(self, label: str) -> None:
        if WINDOWS:
            deadline = time.time() + 60
            while True:
                status = self.supervisor_status()
                states = ((status.get("bus") or {}).get("state"), (status.get("management") or {}).get("state"))
                if states == ("running", "running") or time.time() > deadline:
                    break
                time.sleep(2)
            check(bool(status.get("pid")) and status.get("desired") == "running",
                  f"{label}: the supervisor runs, desired running (pid {status.get('pid')}, {status.get('desired')})")
            check(states == ("running", "running"), f"{label}: supervisor children bus / management running ({states})")
        else:
            for unit in (self.bus_unit, self.management_unit):
                check(self.active(unit), f"{label}: {unit} is active")

    # -- logs and cleanup

    def collect_logs(self) -> None:
        out = self.work / "logs"
        out.mkdir(exist_ok=True)
        secrets = [self.token, self.bus_token]
        skip_dirs = {"releases", "natives", "cache", "backup", "displaced", "venv", "prepared", "served", "dl", "artifacts", "logs"}
        secret_names = {"config.json", "accounts.json", "auth.json", "credentials.json"}
        for root in (self.base, self.home, self.work / "release"):
            if not root.exists():
                continue
            for dirpath, dirnames, filenames in os.walk(root):
                dirnames[:] = [d for d in dirnames if d not in skip_dirs]
                for name in filenames:
                    path = Path(dirpath) / name
                    if name in secret_names or "token" in name.lower() or "secret" in name.lower():
                        continue
                    if not name.endswith((".json", ".jsonl", ".log", ".conf", ".service", ".xml")):
                        continue
                    try:
                        if path.stat().st_size > 8 << 20:
                            continue
                        data = path.read_bytes()
                    except OSError:
                        continue
                    if name.endswith(".xml"):  # the task XML is UTF-16
                        data = data.decode("utf-16", errors="replace").encode("utf-8")
                    text = data.decode("utf-8", errors="replace")
                    for s in secrets:
                        text = text.replace(s, "<redacted>")
                    dest = out / root.name / path.relative_to(root)
                    dest.parent.mkdir(parents=True, exist_ok=True)
                    dest.write_text(text, encoding="utf-8")
        if not WINDOWS:
            units = [self.bus_unit, self.management_unit, self.job_unit]
            for unit in units:
                j = subprocess.run(["journalctl", "--user", "--no-pager", "-o", "short-iso", "-u", unit, "-n", "400"],
                                   capture_output=True, text=True, errors="replace")
                text = j.stdout + j.stderr
                for s in secrets:
                    text = text.replace(s, "<redacted>")
                (out / f"journal-{unit}.log").write_text(text, encoding="utf-8")
            if self.unit_dir is not None:
                for p in self.unit_dir.glob(f"cutex-{self.label}-*"):
                    if p.is_file():
                        shutil.copyfile(p, out / p.name)
        log(f"  logs in {out}")

    def cleanup(self) -> None:
        phase("cleanup")
        if (self.base / "install.json").is_file():
            code, _, out = self.cli("service", "uninstall", ok=False)
            if code != 0:
                warn(f"cutex service uninstall exited {code}")
        if self.supervisor_by_harness and self.supervisor_by_harness.poll() is None:
            self.supervisor_by_harness.kill()
        if WINDOWS:
            subprocess.run(["schtasks", "/Delete", "/TN", self.task_name, "/F"], capture_output=True)
            if self.saved_registry:
                import winreg
                with winreg.OpenKey(winreg.HKEY_CURRENT_USER, "Environment", 0, winreg.KEY_ALL_ACCESS) as key:
                    for name, saved in self.saved_registry.items():
                        if saved is None:
                            try:
                                winreg.DeleteValue(key, name)
                            except FileNotFoundError:
                                pass
                        else:
                            winreg.SetValueEx(key, name, 0, saved[1], saved[0])
            return
        # Linux: every drill unit this run may have created (uninstall removes the ones it knows).
        listed = self.systemctl("list-units", "--all", "--plain", "--no-legend", f"cutex-{self.label}-*")
        for line in listed.stdout.splitlines():
            unit = line.split()[0] if line.split() else ""
            if unit.startswith(f"cutex-{self.label}-"):
                self.systemctl("disable", "--now", unit)
                self.systemctl("stop", unit)
        if self.unit_dir is not None:
            for p in self.unit_dir.glob(f"cutex-{self.label}-*"):
                if p.is_dir():
                    shutil.rmtree(p, ignore_errors=True)
                else:
                    p.unlink(missing_ok=True)
        link = self.home / ".config" / "systemd" / "user"
        if link.is_symlink():
            link.unlink()
        self.systemctl("daemon-reload")
        self.systemctl("reset-failed")
        left = self.systemctl("list-units", "--all", "--plain", "--no-legend", f"cutex-{self.label}-*").stdout.strip()
        check(not left, f"cleanup: no {self.label} drill unit left ({left!r})")


def systemd_quote(value: str) -> str:
    if all(c.isascii() and (c.isalnum() or c in "/._-+=:@%") for c in value):
        return value
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"').replace("%", "%%") + '"'


# ---------------------------------------------------------------- phases


TERMINAL_PHASES = {"committed", "rolled-back", "needs-owner", "deferred", "cancelled", "failed"}


def last_run(host: Host, timeout: float = 30) -> dict:
    """The newest run from GET /v2/upgrade/runs, waiting briefly for it to reach a terminal phase."""
    deadline = time.time() + timeout
    while True:
        status, runs = host.get("/v2/upgrade/runs?limit=5")
        items = (runs or {}).get("runs") or []
        run = items[0] if status == 200 and items else {}
        if run.get("phase") in TERMINAL_PHASES or time.time() >= deadline:
            return run
        time.sleep(1)


def assert_on(host: Host, label: str, commit: str, what: str) -> dict | None:
    code, out, _ = host.run(host.installed_cli, "--version")
    check(f"({label}, {commit[:7]})" in out, f"{what}: installed `cutex --version` names ({label}, {commit[:7]}) ({out.strip()})")
    host_info = host.wait_ready(label)
    check(host_info is not None, f"{what}: /v2/host answers with cutexBuild.release {label}")
    if host_info:
        check(str((host_info.get("cutexBuild") or {}).get("commit", "")).startswith(commit[:7]),
              f"{what}: cutexBuild.commit is {commit[:7]}… (got {(host_info.get('cutexBuild') or {}).get('commit')})")
    record = host.install_record()
    check(record.get("activeRelease") == label, f"{what}: install.json activeRelease {label} (got {record.get('activeRelease')})")
    if not WINDOWS:
        current = (host.base / "current").resolve()
        check(current == (host.base / "releases" / label).resolve(), f"{what}: current -> releases/{label} (got {current})")
    else:
        bin_exe, rel_exe = host.base / "bin" / "cutex.exe", host.base / "releases" / label / "cutex.exe"
        check(bin_exe.is_file() and rel_exe.is_file() and ce.sha256(bin_exe) == ce.sha256(rel_exe),
              f"{what}: bin\\cutex.exe is releases\\{label}\\cutex.exe")
    return host_info


def install_010(host: Host, args: argparse.Namespace, from_label: str, dl: Path) -> set[str]:
    phase(f"install {from_label} as a managed install ({'scheduled task + supervisor' if WINDOWS else 'systemd user units'})")
    host.prepare_service_manager()
    code, out, err = host.run(dl / f"cutex{EXE}", "service", "install", "--channel", "release", "--base", str(host.base))
    log("  " + (out + err).strip().replace("\n", "\n  "))
    if code != 0 and WINDOWS and (host.base / "install.json").is_file() and "schtasks" in out + err:
        # 0.1.0 writes install.json before it registers / starts the task: a runner without an interactive
        # logon may refuse the task. The upgrade itself only needs the supervisor (started below).
        warn(f"cutex {from_label} service install exited {code} at its scheduled task step: {(out + err).strip()[-500:]}")
    else:
        check(code == 0, f"cutex {from_label} service install exits 0 (got {code})")
    record = host.install_record()
    log(f"  install.json {json.dumps(record)}")
    check(record.get("activeRelease") == from_label, f"install.json activeRelease {from_label}")
    check(record.get("channel") == "release", "install.json channel release")
    if WINDOWS:
        check(record.get("serviceManager") == "cutex-supervisor", "Windows: serviceManager cutex-supervisor")
        check(record.get("taskName") == host.task_name, f"drill task name {host.task_name} recorded (got {record.get('taskName')})")
        query = subprocess.run(["schtasks", "/Query", "/TN", host.task_name], capture_output=True, text=True, errors="replace")
        if query.returncode != 0:
            warn(f"the scheduled task {host.task_name} is not registered")
        else:
            log(f"  ok    the scheduled task {host.task_name} is registered")
        host.ensure_supervisor(from_label)
    else:
        check(record.get("serviceManager") == "systemd", "Linux: serviceManager systemd")
        check((record.get("busUnit"), record.get("managementUnit")) == (host.bus_unit, host.management_unit),
              f"drill unit names recorded ({record.get('busUnit')}, {record.get('managementUnit')})")
        for unit in (host.bus_unit, host.management_unit):
            check(host.systemctl("is-enabled", "--quiet", unit).returncode == 0, f"{unit} enabled by cutex service install")
    host_info = host.wait_ready(from_label)
    check(host_info is not None, f"{from_label} Management API answers on 127.0.0.1:{host.port} with the private home's token")
    host.services_up(from_label)
    status, upgrade = host.get("/v2/upgrade")
    check(status == 200 and (upgrade or {}).get("managed") is True, f"GET /v2/upgrade: managed (status {status})")
    status, caps = host.get("/v2/capabilities")
    before = features(caps)
    check(status == 200 and len(before) > 0, f"{from_label} capabilities read ({len(before)})")
    # 0.1.0 predates components; 0.1.1 on (the default since the 0.1.2 release) already has them.
    log(f"  {from_label} {'advertises' if 'components' in before else 'does not advertise'} `components`")
    install_native_010(host, from_label)
    return before


def install_native_010(host: Host, from_label: str) -> None:
    """0.1.0's native as a real 0.1.0 managed install has it (AGENT-INSTALL.md B.3 + 5.3)."""
    phase(f"{from_label}'s native {NATIVE_VERSION} selected for new agents (cutex human install-runtime)")
    natives = host.base / "natives" / NATIVE_VERSION
    natives.mkdir(parents=True)
    with tarfile.open(host.work / "served" / f"native-{NATIVE_VERSION}" / f"native-{NATIVE_VERSION}-{PLATFORM}.tar.gz") as tar:
        tar.extractall(natives)
    codex_home = host.home / ".cutex" / "codex-home"
    codex_home.mkdir(parents=True, exist_ok=True)
    (codex_home / "config.toml").touch()
    bundle_path = host.base / "releases" / from_label / "manifests" / "default.json"
    bundle_path.parent.mkdir(parents=True, exist_ok=True)
    bundle = bundle_v4(natives, host.base / "releases" / from_label / f"cutex-mcp{EXE}", codex_home / "config.toml", "0" * 40)
    bundle_path.write_text(json.dumps(bundle, indent=2), encoding="utf-8")
    host.cli("human", "install-runtime", str(bundle_path), show=True)
    selected = host.selected_runtime()
    check(same_path(selected.get("bundle_manifest"), bundle_path),
          f"new agents use releases/{from_label}/manifests/default.json (got {selected.get('bundle_manifest')})")
    host.native_home_before = selected.get("native_home")


def upgrade_to_011(host: Host, url: str, to_label: str, to_commit: str, from_label: str, before: set[str], mode: str,
                   new_native: str | None = None, removed: list[str] | None = None, expected_one_way: bool = False) -> None:
    phase(f"upgrade {from_label} -> {to_label} ({mode}{', native ' + new_native if new_native else ''}) "
          f"with the installed {from_label} binary")
    host.cli("upgrade", "config", "--source", url)
    _, available, _ = host.cli("upgrade", "check", show=True)
    available = available or {}
    check(available.get("release") == to_label, f"check offers {to_label} (got {available.get('release')})")
    check(available.get("sequence") == 2, f"{to_label} has sequence 2")
    check(available.get("mode") == mode, f"{from_label} classifies {to_label} as {mode} (got {available.get('mode')})")
    if expected_one_way and not check(available.get("class") == "one-way", "installed updater classifies target migration as one-way"):
        return
    if new_native:
        check(available.get("nativeVersion") == new_native, f"native {new_native} to stage (got {available.get('nativeVersion')})")
    else:
        check(available.get("nativeVersion") is None, f"no native to stage (got {available.get('nativeVersion')})")
    _, staged, _ = host.cli("upgrade", "stage", show=True)
    staged = staged or {}
    checks = {c.get("name"): c.get("ok") for c in staged.get("preflight") or []}
    check(staged.get("preflightOk", staged.get("preflight_ok")) is True, f"preflight ok ({checks})")
    for name in ("integrity", "version", "selftest", "rollbackTarget"):
        check(checks.get(name) is True, f"preflight check {name} passed")
    for name in NOTICE_FILES:
        check((host.base / "releases" / to_label / name).is_file(), f"releases/{to_label}/{name} unpacked next to the executables")
    natives = sorted(p.name for p in (host.base / "natives").iterdir())
    if new_native:
        check(checks.get("native") is True, f"preflight check native passed (natives/{new_native} complete)")
        check(natives == sorted([NATIVE_VERSION, new_native]),
              f"natives/ holds {from_label}'s {NATIVE_VERSION} and the staged {new_native} (got {natives})")
        staged_dir = host.base / "natives" / new_native
        for name in (f"codex-app-server{EXE}", f"codex-code-mode-host{EXE}", "codex_app_server_protocol.schemas.json"):
            check((staged_dir / name).is_file(), f"natives/{new_native}/{name} unpacked")
        if new_native == NEW_NATIVE_STUB:
            for name in NOTICE_FILES:
                check((staged_dir / name).is_file(), f"natives/{new_native}/{name} unpacked next to the executables")
        selected = host.selected_runtime()
        check(same_path(selected.get("bundle_manifest"), host.base / "releases" / from_label / "manifests" / "default.json"),
              f"staging selects nothing yet: new agents still use releases/{from_label}/manifests/default.json")
    else:
        check("native" not in checks, "no native preflight (the manifest brings no native)")
        check(natives == [NATIVE_VERSION], f"natives/ holds only {from_label}'s {NATIVE_VERSION} (got {natives})")
    command = apply_arguments(available, host.args.confirm_one_way)
    if available.get("class") == "one-way":
        code, out, err = host.run(host.installed_cli, "upgrade", "apply", "--max-wait", "600")
        check(code != 0 and "one_way_unconfirmed" in out + err, "one-way apply without confirmation is refused")
        if failures:
            return
    t0 = time.time()
    code, _, out = host.cli(*command, show=True, ok=False)
    log(f"  apply took {time.time() - t0:.0f} s")
    check(code == 0, f"apply exits 0 (got {code})")
    run = last_run(host) if host.wait_ready(to_label) else {}
    check(run.get("phase") == "committed" and run.get("toRelease") == to_label,
          f"run {run.get('runId')} {run.get('fromRelease')} -> {run.get('toRelease')} ended {run.get('phase')} (error {run.get('error')})")
    log(f"  timeline: {[(t.get('phase'), t.get('detail')) for t in run.get('timeline') or []]}")
    log(f"  mode {run.get('mode')}, downtime {run.get('downtimeMs')} ms")
    check(run.get("mode") == mode, f"the run ran in mode {mode} (got {run.get('mode')})")
    assert_on(host, to_label, to_commit, "after the apply")
    status, upgrade = host.get("/v2/upgrade")
    upgrade = upgrade or {}
    check(status == 200 and (upgrade.get("installed") or {}).get("release") == to_label,
          f"GET /v2/upgrade installed {to_label} (got {(upgrade.get('installed') or {}).get('release')})")
    check(upgrade.get("previousRelease") == from_label, f"GET /v2/upgrade previousRelease {from_label} (got {upgrade.get('previousRelease')})")
    check(upgrade.get("staged") is None, "nothing staged after the commit")
    status, caps = host.get("/v2/capabilities")
    after = features(caps)
    lost = unexpected_capability_losses(before, after, removed or [])
    check(status == 200 and not lost, f"no undeclared capability losses (lost: {lost}; declared removed: {removed or []})")
    check("components" in after, "`components` capability after the update")
    log(f"  gained: {sorted(after - before)}")
    status, comps = host.get("/v2/components")
    names = sorted(c.get("name") for c in (comps or {}).get("components") or [])
    check(status == 200, f"GET /v2/components answers (status {status}; {names})")
    host.services_up(to_label)
    new_agents(host, from_label, to_label, mode, new_native)


def new_agents(host: Host, from_label: str, to_label: str, mode: str, new_native: str | None = None) -> None:
    """What NEW agents start with after the apply (runtime/light/deployment.json and its bundle)."""
    selected = host.selected_runtime()
    log(f"  deployment.json: {json.dumps(selected)}")
    target = host.base / "releases" / to_label / "manifests" / "default.json"
    if mode != "full":
        check(same_path(selected.get("bundle_manifest"), host.base / "releases" / from_label / "manifests" / "default.json"),
              f"reload: new agents keep releases/{from_label}/manifests/default.json (got {selected.get('bundle_manifest')})")
        warn(f"reload mode: NEW agents keep {from_label}'s runtime bundle and cutex-mcp facade; {to_label}'s facade reaches no agent")
        return
    if not check(same_path(selected.get("bundle_manifest"), target),
                 f"full: new agents use releases/{to_label}/manifests/default.json (got {selected.get('bundle_manifest')})"):
        return
    check(same_path(selected.get("native_home"), Path(host.native_home_before or "/nonexistent")),
          f"the native home is kept ({selected.get('native_home')})")
    bundle = json.loads(target.read_text(encoding="utf-8"))
    facade = host.base / "releases" / to_label / f"cutex-mcp{EXE}"
    pinned = bundle.get("facade") or {}
    check(same_path(pinned.get("path"), facade) and pinned.get("sha256") == ce.sha256(facade),
          f"its facade is {to_label}'s cutex-mcp (got {pinned.get('path')})")
    version = new_native or NATIVE_VERSION
    whose = f"the staged {to_label}" if new_native else f"{from_label}'s"
    native = host.base / "natives" / version
    for key, name in (("executable", f"codex-app-server{EXE}"), ("code_mode_host", f"codex-code-mode-host{EXE}"),
                      ("schema", "codex_app_server_protocol.schemas.json")):
        got = (bundle.get(key) or {}).get("path")
        check(same_path(got, native / name), f"its {key} is {whose} natives/{version}/{name} (got {got})")


def components(host: Host, args: argparse.Namespace, versions: dict[str, str]) -> None:
    phase("optional components from the signed manifest")
    _, listed, _ = host.cli("component", "list", "--json")
    names = [c.get("name") for c in (listed or {}).get("components") or []]
    check("claude-runtime" in names, f"component list names claude-runtime ({names})")

    version = versions["claude-runtime"]
    # Use the shipped payload metadata, including the supported CLI-free variant;
    # a release label alone does not tell us whether a CLI/Python is bundled.
    bundled_python, bundled_cli = ce.bundled_payload(args.components, version)
    prepare_env = {} if bundled_python else {"CUTEX_CLAUDE_PYTHON": args.claude_python}
    _, out, _ = host.cli("component", "install", "claude-runtime", extra=prepare_env, path=ce.base_path(), show=True)
    comp = ce.component_of(out)
    check(comp.get("state") == "needsUser", f"claude-runtime: needsUser (got {comp.get('state')})")
    expected_code = "claude_login_required" if bundled_cli else "claude_cli_missing"
    check(ce.needs_code(comp) == expected_code,
          f"claude-runtime: needsUser {expected_code} from payload metadata (got {ce.needs_code(comp)})")
    check((host.base / "components" / "claude-runtime" / version / "receipt.json").is_file(),
          f"claude-runtime {version} unpacked from the new release's manifest with a receipt")

    if not WINDOWS:
        _, out, _ = host.cli("component", "install", "job-service", show=True)
        _, value, _ = host.cli("component", "status", "job-service")
        comp = ce.component_of(value)
        running = (comp.get("detail") or {}).get("running") or {}
        check(comp.get("state") == "ready", f"job-service: ready (got {comp.get('state')}, steps {comp.get('steps')})")
        check(running.get("version") == versions["job-service"], f"job-service {versions['job-service']} answers on its socket ({running})")
        check(host.active(host.job_unit), f"{host.job_unit} is active")

    status, comps = host.get("/v2/components")
    states = {c.get("name"): c.get("state") for c in (comps or {}).get("components") or []}
    log(f"  /v2/components: {states}")
    check(status == 200 and states.get("claude-runtime") == "needsUser", "GET /v2/components: claude-runtime needsUser")
    if not WINDOWS:
        check(states.get("job-service") == "ready", "GET /v2/components: job-service ready")


def rollback_leg(host: Host, release: Release, generator: Path, work: Path, to_label: str, to_commit: str) -> None:
    phase(f"rollback leg: apply {BROKEN_LABEL} (its daemon exits) -> rolled-back on {to_label}")
    release.channel(generator, work / "release" / "m-broken", work / "release" / "ch-broken")
    _, available, _ = host.cli("upgrade", "check", show=True)
    check((available or {}).get("release") == BROKEN_LABEL, f"check offers {BROKEN_LABEL}")
    _, staged, _ = host.cli("upgrade", "stage", show=True)
    check((staged or {}).get("preflightOk", (staged or {}).get("preflight_ok")) is True,
          f"{BROKEN_LABEL} passes preflight (its --version and selftest work)")
    jobs_before = host.active(host.job_unit) if not WINDOWS else None
    t0 = time.time()
    code, _, out = host.cli("upgrade", "apply", "--max-wait", "600", show=True, ok=False)
    log(f"  apply took {time.time() - t0:.0f} s")
    check(code != 0, f"apply of {BROKEN_LABEL} exits non-zero (got {code})")
    run = last_run(host) if host.wait_ready(to_label) else {}
    log(f"  timeline: {[(t.get('phase'), t.get('detail')) for t in run.get('timeline') or []]}")
    check(run.get("toRelease") == BROKEN_LABEL and run.get("phase") == "rolled-back",
          f"run {run.get('runId')} ended rolled-back (got {run.get('phase')}, error {run.get('error')}, needsOwner {run.get('needsOwner')})")
    check(run.get("needsOwner") is None, "no needs-owner")
    phases = [t.get("phase") for t in run.get("timeline") or []]
    check("swapping" in phases and "rolling-back" in phases, f"it swapped and rolled back ({phases})")
    assert_on(host, to_label, to_commit, "after the rollback")
    status, upgrade = host.get("/v2/upgrade")
    check(status == 200 and ((upgrade or {}).get("installed") or {}).get("release") == to_label,
          f"GET /v2/upgrade still installed {to_label}")
    host.services_up(f"after the rollback ({to_label})")
    if run.get("mode") == "full":
        selected = host.selected_runtime()
        check(same_path(selected.get("bundle_manifest"), host.base / "releases" / to_label / "manifests" / "default.json"),
              f"after the rollback new agents use releases/{to_label}/manifests/default.json again (got {selected.get('bundle_manifest')})")
    if jobs_before:
        deadline = time.time() + 60
        while time.time() < deadline and not host.active(host.job_unit):
            time.sleep(2)
        check(host.active(host.job_unit), f"{host.job_unit} active again after the rollback")


# --------------------------------------------------------------------- main


def build_release(args: argparse.Namespace, work: Path, key, generator: Path) -> tuple[Release, dict]:
    phase("test release (throwaway key)")
    rel = work / "release"
    from_exe, to_exe = args.from_cutex / f"cutex{EXE}", args.to_cutex / f"cutex{EXE}"
    from_line, from_label, from7 = version_line(from_exe)
    to_line, to_label, to7 = version_line(to_exe)
    log(f"  from: {from_line}\n  to:   {to_line}")
    if from7 != args.from_commit[:7] or to7 != args.to_commit[:7]:
        raise SystemExit(f"--from-commit / --to-commit do not match the builds ({from7}, {to7})")
    from_caps = capabilities(from_exe, rel / "scratch-from")
    to_caps = capabilities(to_exe, rel / "scratch-to")
    (rel / "capabilities-from.json").write_text(json.dumps(from_caps), encoding="utf-8")
    log(f"  capabilities: from {len(from_caps)}, to {len(to_caps)}")
    # The real release's generator arguments (sign-and-upload.sh reads the same committed files).
    meta_args: list[str] = []
    meta: dict = {}
    prev_caps = rel / "capabilities-from.json"
    if args.release_meta:
        meta_file = args.release_meta / f"{to_label}.json"
        if meta_file.is_file():
            meta = json.loads(meta_file.read_text(encoding="utf-8"))
            meta_args = ["--meta", str(meta_file)]
            log(f"  release metadata {meta_file}: mode {meta.get('mode', '(generator default)')}")
        else:
            raise SystemExit(f"missing requested release metadata: {meta_file}")
        committed = args.release_meta / f"capabilities-{from_label}.json"
        if committed.is_file():
            doc = json.loads(committed.read_text(encoding="utf-8"))
            listed = set(doc[PLATFORM] if isinstance(doc, dict) else doc)
            check(listed == set(from_caps) - HOST_DEPENDENT,
                  f"{committed.name} {PLATFORM} = the {from_label} build's capabilities "
                  f"(only listed: {sorted(listed - set(from_caps))}, only built: {sorted(set(from_caps) - listed)})")
            prev_caps = committed
    else:
        warn("no --release-meta: the manifests are generated without the real release's metadata")
    expected_mode = meta.get("mode") or ("full" if meta.get("migrations") else "reload")

    def pair(d: Path) -> dict[str, Path]:
        return {f"cutex{EXE}": d / f"cutex{EXE}", f"cutex-mcp{EXE}": d / f"cutex-mcp{EXE}"}

    from_archive = cutex_build(rel / "b-from", from_label, args.from_commit, from_line, from_caps, pair(args.from_cutex))
    # The new release's archive carries the license files as the real build does (extra files at the root).
    cutex_build(rel / "b-to", to_label, args.to_commit, to_line, to_caps, {**pair(args.to_cutex), **notice_files(rel / "b-notices")})
    native = native_build(rel / "b-native")
    brought = new_native_build(args.new_native, rel / "b-new-native")
    new_native = brought[1] if brought else None
    native_args = ["--native-dir", str(rel / "b-new-native")] if brought else []
    broken_line = f"cutex 0.0.0 ({BROKEN_LABEL}, {args.to_commit[:7]})"
    stub = broken_stub(rel / "b-stub", broken_line, args.rustc)
    cutex_build(rel / "b-broken", BROKEN_LABEL, args.to_commit, broken_line, to_caps,
                {f"cutex{EXE}": stub, f"cutex.real{EXE}": to_exe, f"cutex-mcp{EXE}": args.to_cutex / f"cutex-mcp{EXE}"})

    release = Release(work, key)
    m010 = rel / "m-from"
    m010.mkdir(parents=True)
    (m010 / "manifest.json").write_text(json.dumps(manifest_010(from_label, args.from_commit, from_archive, native), indent=2) + "\n",
                                        encoding="utf-8")
    # As `sign-and-upload.sh cutex --run <to> --sequence next [--native-run <cute-codex run>]` builds it.
    generate(generator, "manifest", "--cutex-dir", str(rel / "b-to"), "--component-dir", str(args.components),
             "--sequence", "next", "--previous", str(m010 / "manifest.json"), "--previous-capabilities", str(prev_caps),
             *native_args, *meta_args, "--out", str(rel / "m-to"))
    broken_meta = rel / "broken-meta.json"
    broken_meta.write_text(json.dumps(rollback_metadata(meta)), encoding="utf-8")
    generate(generator, "manifest", "--cutex-dir", str(rel / "b-broken"), "--component-dir", str(args.components),
             "--sequence", "next", "--previous", str(rel / "m-to" / "manifest.json"), "--meta", str(broken_meta),
             "--notes-en", "upgrade e2e: a release whose daemon exits", "--out", str(rel / "m-broken"))
    docs = {}
    for name in ("m-from", "m-to", "m-broken"):
        docs[name] = release.manifest(rel / name)
    for d, label in ((rel / "b-from", from_label), (rel / "b-to", to_label), (rel / "b-broken", BROKEN_LABEL)):
        release.put(f"cutex-{label}", d / f"cutex-{label}-{PLATFORM}.tar.gz")
    release.put(f"native-{NATIVE_VERSION}", native)
    if brought:
        release.put(f"native-{new_native}", brought[0])
        expected_mode = meta.get("mode") or "full"
    to_doc = docs["m-to"]
    check(to_doc["sequence"] == 2, f"{to_label} manifest: sequence 2 (got {to_doc['sequence']})")
    check(to_doc["mode"] == expected_mode, f"{to_label} manifest: mode {expected_mode} like the real release (got {to_doc['mode']})")
    one_way = any(m.get("class") == "one-way" and m.get("since") == to_label
                  for m in to_doc["cutex"].get("migrations", []))
    if one_way:
        apply_arguments({"release": to_label, "class": "one-way"}, args.confirm_one_way)
    if new_native:
        check((to_doc.get("native") or {}).get("version") == new_native,
              f"{to_label} manifest: native {new_native} (got {(to_doc.get('native') or {}).get('version')})")
    else:
        check("native" not in to_doc, f"{to_label} manifest: no native part (the host keeps {from_label}'s {NATIVE_VERSION})")
    check("components" in to_doc and "claudeRuntime" not in to_doc, f"{to_label} manifest: top-level components, no claudeRuntime")
    if to_doc["mode"] == "reload":
        warn(f"the {to_label} manifest is `reload`: NEW agents will keep {from_label}'s runtime bundle and cutex-mcp facade")
    for name, comp in to_doc.get("components", {}).items():
        for a in comp["artifacts"].values():
            release.put(a["tag"], next(args.components.rglob(a["name"])))
    release.channel(generator, rel / "m-to", rel / "ch-to")
    return release, {"from": (from_label, from_line), "to": (to_label, to_line), "mode": to_doc["mode"], "native": new_native,
                     "removed": to_doc["cutex"]["capabilities"]["removed"], "one_way": one_way,
                     "components": {n: c["version"] for n, c in to_doc.get("components", {}).items()}}



def apply_arguments(available: dict, confirm_one_way: str | None) -> list[str]:
    """Use the installed updater's classification; never weaken its confirmation gate."""
    args = ["upgrade", "apply", "--max-wait", "600"]
    if available.get("class") == "one-way":
        release = available.get("release")
        if not release or confirm_one_way != release:
            raise SystemExit(f"one-way drill requires --confirm-one-way {release}; no automatic rollback")
        args += ["--confirm-one-way", release]
    return args


def unexpected_capability_losses(before: set[str], after: set[str], removed: list[str]) -> list[str]:
    return sorted(before - after - HOST_DEPENDENT - set(removed))


def rollback_metadata(meta: dict) -> dict:
    """A separate failed release after TO: no new stores/migrations, full service restart.

    The real TO manifest keeps its one-way migrations. Copying those into the synthetic
    failure would turn a rollback test into a needs-owner test, depending on release ordering.
    """
    return {"mode": "full", "capabilities": meta.get("capabilities") or {}}


def verify_minisign(path: Path, public_key: Path) -> None:
    """Verify both minisign signatures with an independently trusted key, never an asset key."""
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
    pub_lines = public_key.read_text(encoding="utf-8").splitlines()
    pub = base64.b64decode(pub_lines[1] if pub_lines[0].startswith("untrusted comment:") else pub_lines[0], validate=True)
    lines = Path(str(path) + ".minisig").read_text(encoding="utf-8").splitlines()
    if len(pub) != 42 or pub[:2] != b"Ed" or len(lines) != 4 or not lines[2].startswith("trusted comment: "):
        raise ValueError("invalid minisign key or signature structure")
    sig = base64.b64decode(lines[1], validate=True)
    if len(sig) != 74 or sig[:2] not in (b"ED", b"Ed") or sig[2:10] != pub[2:10]:
        raise ValueError("minisign algorithm or key ID mismatch")
    data = path.read_bytes()
    key = Ed25519PublicKey.from_public_bytes(pub[10:])
    key.verify(sig[10:], hashlib.blake2b(data, digest_size=64).digest() if sig[:2] == b"ED" else data)
    key.verify(base64.b64decode(lines[3], validate=True), sig[10:] + lines[2][len("trusted comment: "):].encode())



def native_asset_name(tag: str, version: str, platform: str) -> str:
    """The publishing tag is independent of the native's embedded/archive version."""
    if not re.fullmatch(r"cute-codex-v[0-9][A-Za-z0-9._-]{0,63}", tag):
        raise ValueError("invalid cute-codex release tag")
    if not re.fullmatch(r"[0-9][A-Za-z0-9._-]{0,63}", version):
        raise ValueError("invalid native archive version")
    if platform not in ("linux-x64", "windows-x64"):
        raise ValueError("invalid native platform")
    return f"native-{version}-{platform}.tar.gz"


def native_archive_info(archive: Path, platform: str) -> dict:
    # The archive's signature authenticates this record; an unsigned sidecar cannot override it.
    with tarfile.open(archive) as tar:
        records = [m for m in tar.getmembers() if m.name == "cute-codex-build.json"]
        if len(records) != 1 or not records[0].isreg() or records[0].size > 1 << 20:
            raise ValueError("native archive requires one regular cute-codex-build.json (<=1 MiB)")
        info = json.loads(tar.extractfile(records[0]).read().decode("utf-8"))
    version = info.get("version", "")
    if not isinstance(version, str) or not re.fullmatch(r"[0-9][A-Za-z0-9._-]{0,63}", version):
        raise ValueError("invalid embedded native version")
    if info.get("platform") != platform or archive.name != f"native-{version}-{platform}.tar.gz":
        raise ValueError("native archive name/platform differs from embedded build record")
    if not re.fullmatch(r"[0-9a-f]{40}", str(info.get("commit", ""))):
        raise ValueError("invalid embedded native commit")
    return info


def signed_native_preflight(argv: list[str]) -> int:
    """Verify a published native without executing it; tag and archive version are independent."""
    parser = argparse.ArgumentParser(description=signed_native_preflight.__doc__)
    parser.add_argument("--assets", required=True, type=Path)
    parser.add_argument("--public-key", required=True, type=Path)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--platform", choices=("linux-x64", "windows-x64"), default=PLATFORM)
    args = parser.parse_args(argv)
    name = native_asset_name(args.tag, args.version, args.platform)
    archive = args.assets / name
    verify_minisign(archive, args.public_key)
    verify_minisign(args.assets / "SHA256SUMS", args.public_key)
    digest = ce.sha256(archive)
    matches = [fields[0] for line in (args.assets / "SHA256SUMS").read_text(encoding="utf-8").splitlines()
               if len(fields := line.split()) == 2 and fields[1].lstrip("*") == name]
    if matches != [digest]:
        raise ValueError("native archive differs from signed SHA256SUMS")
    info = native_archive_info(archive, args.platform)
    if info["version"] != args.version:
        raise ValueError("native version differs from requested archive version")
    log(json.dumps({"evidence": "signed-native-preflight-only", "tag": args.tag,
                    "nativeVersion": info["version"], "commit": info["commit"],
                    "sha256": digest, "nativeExecuted": False}))
    return 0


def signed_asset_preflight(argv: list[str]) -> int:
    """Read-only evidence of actual published bytes. Does not install or claim upgrade acceptance.

    Real release builds ignore the debug key/home/service-name overrides used by Host.
    To exercise those bytes, use a fresh disposable VM per platform with no cutex install,
    the embedded release key, standard service names and a dedicated OS user/home. Mirror
    the unchanged signed channel, manifests and artifacts of BOTH releases (and components /
    natives) onto an isolated source, then install 0.1.5 and check/stage/apply 0.2.0 with
    --confirm-one-way 0.2.0. Never re-sign a manifest or inject a test trust key. The 0.2.0
    formal signatures/channel exist only after the public signing workflow has run.
    """
    parser = argparse.ArgumentParser(description=signed_asset_preflight.__doc__)
    parser.add_argument("--assets", required=True, type=Path)
    parser.add_argument("--public-key", required=True, type=Path, help="independently trusted release key (not downloaded with assets)")
    parser.add_argument("--release", default="0.1.5")
    parser.add_argument("--platform", choices=("linux-x64", "windows-x64"), default=PLATFORM)
    args = parser.parse_args(argv)
    root = args.assets
    for name in ("manifest.json", "capabilities.json", "SHA256SUMS"):
        verify_minisign(root / name, args.public_key)
    doc = json.loads((root / "manifest.json").read_text(encoding="utf-8"))
    if doc.get("schema") != "cutex/release-manifest/v1" or doc.get("channel") != "release" or doc["cutex"]["release"] != args.release:
        raise ValueError("signed manifest release/channel/schema mismatch")
    artifact = doc["cutex"]["artifacts"][args.platform]
    name = artifact["name"]
    if name != f"cutex-{args.release}-{args.platform}.tar.gz":
        raise ValueError("unexpected archive name")
    archive = root / name
    verify_minisign(archive, args.public_key)
    sums = []
    for line in (root / "SHA256SUMS").read_text(encoding="utf-8").splitlines():
        fields = line.split()
        if len(fields) == 2 and fields[1].lstrip("*") == name:
            sums.append(fields[0])
    digest = ce.sha256(archive)
    if sums != [digest] or artifact["sha256"] != digest or artifact["size"] != archive.stat().st_size:
        raise ValueError("archive differs from signed manifest or signed SHA256SUMS")
    log(json.dumps({"evidence": "signed-asset-preflight-only", "release": args.release,
                    "platform": args.platform, "sha256": digest, "upgradeExecuted": False}))
    return 0



# ------------------------------------------------------- formal production run

FORMAL_PUBLIC_KEY = "RWRH9INhAb2gACJAg/h5EBjbudos2LXj6K+5hRBEGeVLDKaKzE+FICmS"
FORMAL_NATIVE_VERSION = "0.164.0-r58"
FORMAL_RESERVED_PORTS = {24270, 24260, 25370, 25360, 24670, 24660, 24250}


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def formal_label(value: str) -> str:
    require(isinstance(value, str) and bool(re.fullmatch(r"[A-Za-z0-9._-]{1,128}", value))
            and value not in (".", ".."), f"unsafe mirror tag/asset: {value!r}")
    return value


def asset_candidates(name: str, roots: list[Path]) -> list[Path]:
    formal_label(name)
    return sorted({p.resolve() for root in roots for p in root.rglob(name) if p.is_file() and not p.is_symlink()})


def unique_asset(name: str, roots: list[Path]) -> Path:
    candidates = asset_candidates(name, roots)
    require(bool(candidates), f"missing asset {name} under {roots}")
    require(len({ce.sha256(p) for p in candidates}) == 1, f"conflicting copies of {name}")
    return candidates[0]


def copy_formal_signed(name: str, roots: list[Path], dest: Path, key: Path) -> Path:
    source = unique_asset(name, roots)
    signatures = asset_candidates(name + ".minisig", roots)
    require(bool(signatures), f"missing formal signature for {name}")
    dest.mkdir(parents=True, exist_ok=True)
    output = dest / name
    shutil.copyfile(source, output)
    # Re-signing identical bytes at two formal release steps can change the trusted
    # timestamp. Accept a valid pinned-key signature; never invent or rewrite one.
    for signature in signatures:
        shutil.copyfile(signature, dest / (name + ".minisig"))
        try:
            verify_minisign(output, key)
            return output
        except (ValueError, InvalidSignature):
            continue
    raise ValueError(f"no valid formal signature for {name}")


def verify_formal_artifact(path: Path, artifact: dict) -> None:
    require(path.name == artifact.get("name"), "artifact name mismatch")
    require(path.stat().st_size == artifact.get("size"), f"signed artifact size mismatch: {path.name}")
    require(ce.sha256(path) == artifact.get("sha256"), f"signed artifact hash mismatch: {path.name}")


def prepare_formal_mirror(args: argparse.Namespace, work: Path) -> dict:
    key_lines = args.public_key.read_text(encoding="utf-8").splitlines()
    key_line = key_lines[1] if len(key_lines) > 1 and key_lines[0].startswith("untrusted comment:") else (key_lines[0] if key_lines else "")
    require(key_line == FORMAL_PUBLIC_KEY, "formal mode requires committed release key 00A0BD016183F447")
    mirror = work / "mirror"
    docs = {}
    # Do not mix old/new manifests with the same filename during lookup.
    for release, roots in (("0.1.5", [args.old_assets]), ("0.2.0", [args.signed_assets])):
        dest = mirror / f"cutex-{release}"
        manifest_path = copy_formal_signed("manifest.json", roots, dest, args.public_key)
        doc = json.loads(manifest_path.read_text(encoding="utf-8"))
        require(doc.get("schema") == "cutex/release-manifest/v1" and doc.get("channel") == "release"
                and doc.get("cutex", {}).get("release") == release, f"wrong signed {release} manifest identity")
        require(re.fullmatch(r"[0-9a-f]{40}", str(doc["cutex"].get("commit", ""))) is not None,
                f"malformed {release} commit")
        caps = copy_formal_signed("capabilities.json", roots, dest, args.public_key)
        copy_formal_signed("SHA256SUMS", roots, dest, args.public_key)
        artifact = doc["cutex"]["artifacts"][PLATFORM]
        require(artifact["name"] == f"cutex-{release}-{PLATFORM}.tar.gz", "unexpected cutex archive name")
        artifact_roots = roots if release == "0.1.5" else [*args.build_assets, args.signed_assets]
        archive = copy_formal_signed(artifact["name"], [*artifact_roots, *roots],
                                     mirror / formal_label(artifact.get("tag", f"cutex-{release}")), args.public_key)
        verify_formal_artifact(archive, artifact)
        sums = [fields[0] for line in (dest / "SHA256SUMS").read_text(encoding="utf-8").splitlines()
                if len(fields := line.split()) == 2 and fields[1].lstrip("*") == archive.name]
        require(sums == [artifact["sha256"]], f"signed release SHA256SUMS mismatch: {release}")
        docs[release] = {"manifest": doc, "archive": archive, "dir": dest,
                         "caps": json.loads(caps.read_text(encoding="utf-8"))}
    target = docs["0.2.0"]["manifest"]
    require(target["cutex"]["commit"] == args.expected_commit, "signed target commit differs from expected Build/Sign SHA")
    require(target["sequence"] > docs["0.1.5"]["manifest"]["sequence"], "target sequence must increase")
    require(target.get("mode") == "full", "formal 0.2.0 must request full restart")
    require(any(m.get("class") == "one-way" and m.get("since") == "0.2.0"
                for m in target["cutex"].get("migrations", [])), "formal target lacks its one-way migration")
    channel_path = copy_formal_signed("channel.json", [args.signed_assets], mirror / "channel-release", args.public_key)
    channel = json.loads(channel_path.read_text(encoding="utf-8"))
    require(channel.get("schema") == "cutex/channel/v1" and channel.get("channel") == "release"
            and channel.get("release") == "0.2.0" and channel.get("sequence") == target["sequence"]
            and channel.get("manifestSha256") == ce.sha256(docs["0.2.0"]["dir"] / "manifest.json"),
            "signed channel does not bind the supplied target manifest")
    native = target.get("native") or {}
    require(native.get("version") == args.expected_native_version, "signed manifest native version differs from requested version")
    # Include all target assets for this platform (also optional component packages).
    parts = [native, *target.get("components", {}).values()]
    roots = [*args.build_assets, args.signed_assets, args.native_assets]
    for part in parts:
        artifact = part.get("artifacts", {}).get(PLATFORM)
        if artifact is None:
            continue
        tag = formal_label(artifact.get("tag", "cutex-0.2.0"))
        archive = copy_formal_signed(formal_label(artifact["name"]), roots, mirror / tag, args.public_key)
        verify_formal_artifact(archive, artifact)
        if part is native:
            info = native_archive_info(archive, PLATFORM)
            require(info["version"] == native["version"] and info["commit"] == native["commit"],
                    "signed native archive metadata differs from signed manifest")
    docs["mirror"] = mirror
    return docs


def formal_runner_home(args: argparse.Namespace, environ: dict | None = None) -> Path:
    env = os.environ if environ is None else environ
    require(env.get("GITHUB_ACTIONS") == "true" and env.get("RUNNER_ENVIRONMENT") == "github-hosted",
            "formal-assets runs only on fresh GitHub-hosted runners; never a developer/live host")
    require(sys.platform in ("linux", "win32"), "formal-assets supports Linux/Windows only")
    require(re.fullmatch(r"[0-9a-f]{40}", args.expected_commit) is not None, "expected commit must be a full SHA")
    require(args.confirm_one_way == "0.2.0", "formal-assets requires --confirm-one-way 0.2.0")
    require(args.management_port != args.bus_port and all(1024 <= p <= 65535 and p not in FORMAL_RESERVED_PORTS
            for p in (args.management_port, args.bus_port)), "invalid, colliding or reserved service ports")
    profile = env.get("USERPROFILE" if WINDOWS else "HOME", "")
    require(bool(profile) and Path(profile).is_absolute(), "runner must provide its default OS user home")
    home = Path(profile).resolve()
    require(not (home / ".cutex").exists(), "runner default .cutex must be absent before formal fixture creation")
    require(bool(env.get("RUNNER_TEMP")), "RUNNER_TEMP is required")
    root = Path(env["RUNNER_TEMP"]).resolve()
    require(args.work.resolve().is_relative_to(root) and args.work.resolve() != root, "work must be below RUNNER_TEMP")
    require(not args.work.exists() or not any(args.work.iterdir()), "formal work directory must be empty")
    return home


def extract_formal_release(archive: Path, dest: Path) -> dict[str, str]:
    require(not dest.exists(), f"extraction destination exists: {dest}")
    with tarfile.open(archive) as tar:
        members = tar.getmembers()
        require(len(members) <= 20_000 and sum(m.size for m in members) <= 4 << 30, "release archive exceeds production budget")
        names = set()
        for member in members:
            name = member.name.removeprefix("./").rstrip("/")
            require(bool(name) and not name.startswith("/") and not any(p == ".." or ":" in p for p in name.split("/"))
                    and "\\" not in name and "\0" not in name, "unsafe release archive path")
            require(member.isfile() or member.isdir(), "release archive links/devices are forbidden")
            require(name not in names, "duplicate release archive path")
            names.add(name)
        tar.extractall(dest, filter="data")
    hashes = {}
    for name in (f"cutex{EXE}", f"cutex-mcp{EXE}"):
        require((dest / name).is_file(), f"missing {name} in formal release")
        hashes[name] = ce.sha256(dest / name)
    return hashes


def formal_seed(now: dt.datetime | None = None) -> tuple[dict, dict[str, bytes]]:
    now = now or dt.datetime.now(dt.timezone.utc)
    timestamp = lambda value: value.isoformat(timespec="seconds").replace("+00:00", "Z")
    recent, historical = timestamp(now), timestamp(now - dt.timedelta(days=2))
    body = "Formal upgrade fixture: 保留委托内容 after restart."
    answer_bytes = "Formal signed upgrade retained answer. 保留答案。".encode()
    digest = hashlib.sha256(answer_bytes).hexdigest()
    artifact_id = "art-" + digest
    state = {"revision": 41, "nextSeq": 3, "asks": {}, "actions": {}}
    for seq, (ask_id, closed) in enumerate((("ask-formal-pending", False), ("ask-formal-completed", True)), 1):
        at = historical if closed else recent
        state["asks"][ask_id] = {
            "askId": ask_id, "actionId": "action-" + ask_id, "requestSha256": hashlib.sha256(ask_id.encode()).hexdigest(),
            "requester": {"kind": "owner"}, "targetCutexSessionId": "cutex.formal-offline-target",
            "title": "Formal fixture " + ask_id, "body": {"text": body, "textBytes": len(body.encode()),
            "textSha256": hashlib.sha256(body.encode()).hexdigest()}, "options": {"notify": "passive", "context": {"kind": "none"}},
            "seq": seq, "execution": "answered" if closed else "pendingReview", "attempts": [],
            "revision": 1, "createdAt": at, "updatedAt": at,
            **({"answerId": "answer-formal", "closed": "accepted", "closedAt": at} if closed else {})}
    state["answers"] = {"answer-formal": {"answerId": "answer-formal", "askId": "ask-formal-completed", "attempt": 1,
        "revision": 1, "status": "completed", "artifactId": artifact_id, "bytes": len(answer_bytes),
        "metadata": {}, "preview": answer_bytes.decode(), "createdAt": historical}}
    state["artifacts"] = {artifact_id: {"artifactId": artifact_id, "bytes": len(answer_bytes), "mediaType": "text/plain",
        "readers": ["cutex.formal-offline-target"], "asks": ["ask-formal-completed"], "createdAt": historical}}
    state["deliveries"] = {"delivery-formal": {"deliveryId": "delivery-formal", "kind": "answer", "askId": "ask-formal-completed",
        "refId": "answer-formal", "requesterCutexSessionId": "cutex.formal-offline-requester", "state": "acknowledged",
        "passive": True, "settledBy": "read", "createdAt": historical, "updatedAt": historical}}
    state["actions"]["owner\u001faction-formal"] = {"tool": "fixture", "requestSha256": "f" * 64,
        "result": {"retained": "formal-action"}, "at": recent}
    return state, {digest + ".blob": answer_bytes}


def formal_contains(actual, expected) -> bool:
    if isinstance(expected, dict):
        return isinstance(actual, dict) and all(k in actual and formal_contains(actual[k], v) for k, v in expected.items())
    return actual == expected



_FORMAL_OLD_DEFAULTS = {
    "asks": {"hops": 0, "targetReverted": False, "quotaRequeues": 0,
             "options": {"ifBusy": "queue", "review": False, "questions": "requester"},
             "body": {"attachments": [], "textArtifact": None}},
    "answers": {"metadata": {"ownerSteered": False, "sharedTurn": False, "compacted": False, "includedNotes": []}},
}


def formal_old_matches(actual, expected, defaults=None) -> bool:
    """Only explicitly known serde defaults may be added to the original seed."""
    if isinstance(expected, dict):
        defaults = defaults or {}
        return (isinstance(actual, dict)
                and all(k in actual and formal_old_matches(actual[k], v, defaults.get(k)) for k, v in expected.items())
                and all(k in defaults and actual[k] == defaults[k] for k in actual.keys() - expected.keys()))
    return actual == expected


def capture_formal_baseline(state_dir: Path, fixture: dict, blobs: dict[str, bytes]) -> dict:
    """Read and validate the persisted 0.1.5 baseline, without trusting arbitrary changes.

    The missing target can reconcile the pending seed exactly once: pendingReview/rev1
    -> failed/rev2/closed(completed). No attempt or answer is created. All other seed
    fields and records remain immutable, apart from known serialization defaults.
    An unchanged pending row is valid while startup is still settling; the driver
    waits for the reconciled row before starting the upgrade.
    """
    baseline = json.loads((state_dir / "runtime/asks/asks-v1.json").read_text(encoding="utf-8"))
    require(isinstance(baseline, dict) and baseline.get("nextSeq") == fixture["nextSeq"], "old baseline nextSeq changed")
    pending = baseline.get("asks", {}).get("ask-formal-pending") or {}
    original = fixture["asks"]["ask-formal-pending"]
    reconciled = pending.get("execution") == "failed"
    expected_pending = dict(original)
    if reconciled:
        closed_at = pending.get("closedAt")
        try:
            created = dt.datetime.fromisoformat(original["createdAt"].replace("Z", "+00:00"))
            closed = dt.datetime.fromisoformat(closed_at.replace("Z", "+00:00"))
            valid_time = (created.tzinfo is not None and closed.tzinfo is not None
                          and dt.timedelta(0) <= closed - created <= dt.timedelta(minutes=10))
        except (AttributeError, TypeError, ValueError):
            valid_time = False
        require(valid_time and pending.get("updatedAt") == closed_at, "invalid old reconciliation timestamps")
        expected_pending.update(execution="failed", revision=original["revision"] + 1,
                                closed="completed", closedAt=closed_at, updatedAt=closed_at)
    revision = baseline.get("revision")
    # fail_ask changes the store once, and its queued notice may be drained once.
    allowed_revisions = (fixture["revision"] + 1, fixture["revision"] + 2) if reconciled else (fixture["revision"],)
    require(type(revision) is int and revision in allowed_revisions, "invalid old baseline store revision")
    for collection in ("asks", "answers", "artifacts", "deliveries", "actions"):
        actual_rows = baseline.get(collection)
        require(isinstance(actual_rows, dict) and actual_rows.keys() == fixture[collection].keys(),
                f"old baseline lost or added {collection} records")
        for key, expected in fixture[collection].items():
            if collection == "asks" and key == "ask-formal-pending":
                expected = expected_pending
            require(formal_old_matches(actual_rows[key], expected, _FORMAL_OLD_DEFAULTS.get(collection)),
                    f"invalid old baseline {collection}/{key}: immutable content or lifecycle changed")
    for name, expected in blobs.items():
        require((state_dir / "runtime/asks/artifacts" / name).read_bytes() == expected,
                "old baseline answer artifact bytes changed")
    return baseline


def wait_formal_baseline(host: "FormalHost", fixture: dict, blobs: dict[str, bytes]) -> dict:
    deadline = time.monotonic() + 30
    while True:
        baseline = capture_formal_baseline(host.home / ".cutex", fixture, blobs)
        if baseline["asks"]["ask-formal-pending"]["execution"] == "failed":
            return baseline
        require(time.monotonic() < deadline, "old missing-target reconciliation did not settle before upgrade")
        time.sleep(1)


def verify_formal_state(state_dir: Path, fixture: dict, blobs: dict[str, bytes]) -> dict:
    database = state_dir / "runtime/management-v2/management-state.sqlite3"
    with closing(sqlite3.connect(database.resolve().as_uri() + "?mode=ro", uri=True)) as db:
        db.execute("BEGIN")  # one consistent snapshot across hot/cold collections
        version, verified = db.execute("SELECT format_version,verified FROM hot_store_meta WHERE store='asks'").fetchone()
        require(version == 2 and verified == 1, "asks migration must be verified format 2")
        require(db.execute("SELECT active FROM asks_archive_meta WHERE singleton=1").fetchone() == (1,), "asks archive policy inactive")
        rows = {}
        for collection, key, value in db.execute("SELECT collection,row_key,value_json FROM hot_store_rows WHERE store='asks'"):
            rows[(collection, key)] = json.loads(value)
        hot_keys = set(rows)
        cold_keys = set()
        cold_count = 0
        for collection, key, value in db.execute("SELECT collection,row_key,value_json FROM asks_archive_rows"):
            require((collection, key) not in rows, "duplicate authoritative hot/cold ask row")
            rows[(collection, key)] = json.loads(value)
            cold_keys.add((collection, key))
            cold_count += 1
        for collection in ("asks", "answers", "artifacts", "deliveries", "actions"):
            for key, expected in fixture[collection].items():
                actual = rows.get((collection, key))
                require(actual is not None, f"lost migrated {collection}/{key}")
                # Serialization may add defaults; retain every seeded immutable value.
                require(formal_contains(actual, expected), f"changed migrated {collection}/{key}")
        require(("asks", "ask-formal-completed") in cold_keys and ("asks", "ask-formal-pending") in hot_keys,
                "historical completed delegation must be cold and recent reconciled delegation hot")
    for name, expected in blobs.items():
        require((state_dir / "runtime/asks/artifacts" / name).read_bytes() == expected, "answer artifact bytes changed")
    return {"format": version, "verified": bool(verified), "coldRows": cold_count, "seededCollections": 5}


def formal_cli_json(output: str) -> dict | None:
    decoder = json.JSONDecoder()
    for match in re.finditer(r"\{", output):
        try:
            value, end = decoder.raw_decode(output[match.start():])
            if isinstance(value, dict) and not output[match.start() + end:].strip():
                return value
        except ValueError:
            continue
    return None


def verify_formal_selection(host: "FormalHost", target: dict) -> dict:
    selected = host.selected_runtime()
    bundle_path = host.base / "releases/0.2.0/manifests/default.json"
    require(same_path(selected.get("bundle_manifest"), bundle_path), "new agents do not select the actual target bundle")
    bundle = json.loads(bundle_path.read_text(encoding="utf-8"))
    native = target["native"]
    require(bundle.get("upstream_commit") == native["commit"], "selected native commit differs from signed manifest")
    native_dir = host.base / "natives" / native["version"]
    for field, path in (("executable", native_dir / f"codex-app-server{EXE}"),
                        ("code_mode_host", native_dir / f"codex-code-mode-host{EXE}"),
                        ("schema", native_dir / "codex_app_server_protocol.schemas.json"),
                        ("facade", host.base / "releases/0.2.0" / f"cutex-mcp{EXE}")):
        pin = bundle.get(field) or {}
        require(same_path(pin.get("path"), path) and pin.get("sha256") == ce.sha256(path),
                f"selected {field} does not pin the actual formal files")
    return {"nativeVersion": native_dir.name, "nativeCommit": bundle["upstream_commit"],
            "bundleManifest": str(bundle_path), "facadeSha256": bundle["facade"]["sha256"]}


class FormalHost(Host):
    """Only production-valid Host mechanics; no test env, custom service name or fallback."""
    def __init__(self, args: argparse.Namespace, home: Path):
        self.args, self.home, self.work = args, home, args.work
        self.base = self.work / "base"
        self.port, self.bus_port = args.management_port, args.bus_port
        self.token, self.bus_token = os.urandom(32).hex(), os.urandom(32).hex()
        self.env = {k: v for k, v in os.environ.items() if not DROP_ENV.match(k)}
        self.env["NO_COLOR"] = "1"
        self.created_state = False
        self.install_attempted = False
        self.upgrade_executed = False
        self.events: list[str] = []

    @property
    def bus_unit(self) -> str:
        return "cutex-agent-bus.service"

    @property
    def management_unit(self) -> str:
        return "cutex-management-api.service"

    @property
    def task_name(self) -> str:
        return "\\Cutex\\cutex-supervisor"

    def command(self, *args: str, exe: Path | None = None, allow_failure: bool = False) -> tuple[int, dict | None, str]:
        code, out, err = self.run(exe or self.installed_cli, *args)
        output = out + err
        for secret in (self.token, self.bus_token):
            output = output.replace(secret, "<redacted>")
        self.events.append(f"$ {' '.join(args)}\n{output}")
        require(allow_failure or code == 0, f"formal command {' '.join(args)} exited {code}: {output[-4000:]}")
        return code, formal_cli_json(out), output

    def assert_fresh_services(self) -> None:
        for port in (self.port, self.bus_port):
            with socket.socket() as probe:
                probe.bind(("127.0.0.1", port))
        if WINDOWS:
            result = subprocess.run(["schtasks", "/Query", "/FO", "CSV", "/NH"], env=self.env, capture_output=True,
                                    text=True, errors="replace", timeout=60)
            require(result.returncode == 0 and self.task_name.lower() not in result.stdout.lower(), "existing task or task query failed")
        else:
            for unit in (self.bus_unit, self.management_unit):
                result = self.systemctl("show", unit, "--property=LoadState", "--value")
                require(result.stdout.strip() == "not-found", f"existing/unqueryable service: {unit}")

    def initialize(self, mirror: Path, fixture: dict, blobs: dict[str, bytes]) -> None:
        state = self.home / ".cutex"
        state.mkdir(mode=0o700)  # exclusive; never overwrite an existing runner state
        self.created_state = True
        config = {"agent_bus_enabled": True, "agent_bus_token": self.bus_token, "management_api_token": self.token,
                  "management_port": self.port, "agent_bus_port": self.bus_port}
        (state / "config.json").write_text(json.dumps(config), encoding="utf-8")
        (state / "config.json").chmod(0o600)
        asks = state / "runtime/asks"
        (asks / "artifacts").mkdir(parents=True)
        (asks / "asks-v1.json").write_text(json.dumps(fixture, ensure_ascii=False), encoding="utf-8")
        (asks / "asks-v1.json").chmod(0o600)
        for name, data in blobs.items():
            (asks / "artifacts" / name).write_bytes(data)
        settings = self.base / "upgrade/settings.json"
        settings.parent.mkdir(parents=True)
        # Production settings shape; stop the first automatic check contacting public GitHub.
        settings.write_text(json.dumps({"schema": "cutex/upgrade-settings/v1", "source": str(mirror),
                            "checkIntervalHours": 0, "autoStage": False, "autoApply": False}), encoding="utf-8")

    def assert_services(self) -> None:
        if WINDOWS:
            query = subprocess.run(["schtasks", "/Query", "/TN", self.task_name], env=self.env,
                                   capture_output=True, timeout=60)
            require(query.returncode == 0, "production scheduled task missing; no harness supervisor fallback permitted")
            status = self.supervisor_status()
            require(bool(status.get("pid")) and status.get("desired") == "running"
                    and all((status.get(k) or {}).get("state") == "running" for k in ("bus", "management")),
                    "production task supervisor/children not running")
        else:
            require(all(self.active(unit) for unit in (self.bus_unit, self.management_unit)), "production services not active")

    def restart_services(self) -> None:
        # The production CLI has install/uninstall, not `management stop` or
        # `agent start`. Re-register through those actual commands on Windows;
        # on Linux use the installed systemd user units directly.
        if WINDOWS:
            self.command("service", "uninstall")
        else:
            stopped = self.systemctl("stop", self.management_unit, self.bus_unit)
            require(stopped.returncode == 0, "systemd stop failed")
        deadline = time.monotonic() + 60
        while host_status := self.get("/v2/host")[0]:
            require(time.monotonic() < deadline, f"management remains reachable after production stop ({host_status})")
            time.sleep(1)
        if WINDOWS:
            self.command("service", "install", "--channel", "release", "--base", str(self.base),
                         exe=self.base / "releases/0.2.0" / f"cutex{EXE}")
        else:
            started = self.systemctl("start", self.bus_unit, self.management_unit)
            require(started.returncode == 0, "systemd start failed")

    def cleanup(self) -> None:
        errors = []
        try:
            if self.install_attempted:
                require((self.base / "install.json").is_file(), "attempted install has no ownership record; retaining fixture state")
                record = self.install_record()
                require(record.get("base") in (None, str(self.base)), "install record belongs to another base")
                self.command("service", "uninstall")
        except Exception as error:
            errors.append(str(error))
        logs = self.work / "logs"
        logs.mkdir(exist_ok=True)
        (logs / "formal-commands.log").write_text("\n".join(self.events), encoding="utf-8")
        if self.created_state:
            for path in (self.home / ".cutex").rglob("*.log"):
                if path.is_file() and path.stat().st_size <= 8 << 20:
                    data = path.read_text(encoding="utf-8", errors="replace")
                    for secret in (self.token, self.bus_token):
                        data = data.replace(secret, "<redacted>")
                    target = logs / "state" / path.relative_to(self.home / ".cutex")
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_text(data, encoding="utf-8")
            # Retain fixture state on failure for diagnostics; never remove a home whose
            # service uninstall failed. The ephemeral runner is discarded by GitHub.
            if not errors:
                state = self.home / ".cutex"
                require(state.parent.resolve() == self.home.resolve() and not state.is_symlink(), "fixture state path changed")
                shutil.rmtree(state)
        require(not errors, "formal cleanup failed: " + "; ".join(errors))


def formal_assert_api(host: FormalHost, fixture: dict, blobs: dict[str, bytes], *, immutable_only: bool = False) -> None:
    for ask_id, expected in fixture["asks"].items():
        status, value = host.get(f"/v2/asks/{ask_id}")
        ask = (value or {}).get("ask") or {}
        if immutable_only and ask_id == "ask-formal-pending":
            expected = {k: v for k, v in expected.items() if k not in ("execution", "revision", "updatedAt", "closed", "closedAt")}
        require(status == 200 and formal_contains(ask, expected), f"formal API changed ask: {ask_id}")
    status, answer = host.get("/v2/asks/ask-formal-completed/answer")
    require(status == 200 and (answer or {}).get("text") == next(iter(blobs.values())).decode(), "formal API lost answer text")


def execute_formal_upgrade(host: FormalHost, docs: dict, fixture: dict, blobs: dict[str, bytes]) -> dict:
    old, target = docs["0.1.5"], docs["0.2.0"]
    old_dir, target_dir = host.work / "old-release", host.work / "target-reference"
    extract_formal_release(old["archive"], old_dir)
    target_hashes = extract_formal_release(target["archive"], target_dir)
    for name in ("manifest.json", "manifest.json.minisig"):
        shutil.copyfile(old["dir"] / name, old_dir / name)
    host.assert_fresh_services()
    host.initialize(docs["mirror"], fixture, blobs)
    _, _, line = host.command("--version", exe=old_dir / f"cutex{EXE}")
    match = VERSION_LINE.search(line)
    require(match is not None and match["label"] == "0.1.5" and match["commit7"] == old["manifest"]["cutex"]["commit"][:7],
            "original public 0.1.5 binary identity mismatch")
    host.install_attempted = True
    host.command("service", "install", "--channel", "release", "--base", str(host.base), exe=old_dir / f"cutex{EXE}")
    old_ready = host.wait_ready("0.1.5")
    require(old_ready is not None and (old_ready.get("cutexBuild") or {}).get("profile") == "release"
            and (old_ready.get("cutexBuild") or {}).get("commit") == old["manifest"]["cutex"]["commit"],
            "original production 0.1.5 did not start as a release build")
    host.assert_services()
    formal_assert_api(host, fixture, blobs, immutable_only=True)
    baseline = wait_formal_baseline(host, fixture, blobs)
    formal_assert_api(host, baseline, blobs)
    (host.work / "formal-old-baseline.json").write_text(json.dumps(baseline, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    status, before_caps = host.get("/v2/capabilities")
    require(status == 200 and bool(features(before_caps)), "cannot read original capabilities")
    host.command("upgrade", "config", "--source", str(docs["mirror"]), "--package-run", "none")
    _, available, _ = host.command("upgrade", "check")
    target_doc = target["manifest"]
    require(isinstance(available, dict) and available.get("release") == "0.2.0" and available.get("class") == "one-way"
            and available.get("sequence") == target_doc["sequence"] and available.get("nativeVersion") == host.args.expected_native_version,
            "embedded release trust/check did not offer exact formal target")
    _, staged, _ = host.command("upgrade", "stage")
    require((staged or {}).get("preflightOk", (staged or {}).get("preflight_ok")) is True, "formal preflight failed")
    require(all(ce.sha256(host.base / "releases/0.2.0" / name) == digest for name, digest in target_hashes.items()),
            "production staged binaries differ from signed archive")
    code, _, refusal = host.command("upgrade", "apply", "--max-wait", "600", allow_failure=True)
    require(code != 0 and "one_way_unconfirmed" in refusal, "production updater did not refuse missing one-way consent")
    host.command(*apply_arguments(available, host.args.confirm_one_way), "--package-run", "none")
    host.upgrade_executed = True
    ready = host.wait_ready("0.2.0")
    require(ready is not None and (ready.get("cutexBuild") or {}).get("commit") == host.args.expected_commit
            and (ready.get("cutexBuild") or {}).get("profile") == "release",
            "formal target service commit mismatch")
    state = json.loads((host.base / "upgrade/state.json").read_text(encoding="utf-8"))
    run = state.get("run") or {}
    require(run.get("phase") == "committed" and run.get("fromRelease") == "0.1.5" and run.get("toRelease") == "0.2.0"
            and run.get("class") == "one-way", "formal upgrade did not commit exact one-way transition")
    host.assert_services()
    first_state = verify_formal_state(host.home / ".cutex", baseline, blobs)
    formal_assert_api(host, baseline, blobs)
    status, after_caps = host.get("/v2/capabilities")
    removed = target_doc["cutex"].get("capabilities", {}).get("removed", [])
    require(status == 200 and not unexpected_capability_losses(features(before_caps), features(after_caps), removed),
            "formal upgrade lost undeclared capabilities")
    host.restart_services()
    restarted = host.wait_ready("0.2.0")
    require(restarted is not None and (restarted.get("cutexBuild") or {}).get("commit") == host.args.expected_commit
            and (restarted.get("cutexBuild") or {}).get("profile") == "release", "formal services failed exact-release restart")
    host.assert_services()
    formal_assert_api(host, baseline, blobs)
    restarted_state = verify_formal_state(host.home / ".cutex", baseline, blobs)
    selection = verify_formal_selection(host, target_doc)
    require(all(ce.sha256(host.installed_cli.parent / name) == digest for name, digest in target_hashes.items()),
            "running install bytes differ from formal signed target")
    return {"evidence": "formal-signed-managed-upgrade", "from": "0.1.5", "to": "0.2.0", "commit": host.args.expected_commit,
            "selectionAfterRestart": selection, "nativeVersion": selection["nativeVersion"], "runId": run.get("runId"), "phase": run["phase"],
            "stateAfterApply": first_state, "stateAfterRestart": restarted_state, "upgradeExecuted": True,
            "productionServices": True, "testTrustOverrides": False, "rollbackAttempted": False}


def formal_assets(argv: list[str]) -> int:
    """Actual formal signed 0.1.5 -> 0.2.0 managed upgrade, fresh hosted runner only.

    --build-assets may be repeated for additional native build artifacts. --signed-assets
    accepts the nested Sign dry-run artifact; --old-assets and --native-assets contain
    the original published archives and detached signatures. All signed JSON is copied
    byte-for-byte to a production Source::Dir mirror. No signing key, debug/test overrides,
    source rebuild, forced service replacement, model calls or rollback is used.
    The parent workflow must verify hosted-runner and Build/Sign run provenance first.
    """
    parser = argparse.ArgumentParser(description=formal_assets.__doc__)
    parser.add_argument("--build-assets", required=True, action="append", type=Path)
    parser.add_argument("--signed-assets", required=True, type=Path)
    parser.add_argument("--old-assets", required=True, type=Path)
    parser.add_argument("--native-assets", required=True, type=Path)
    parser.add_argument("--public-key", required=True, type=Path)
    parser.add_argument("--from-release", choices=("0.1.5",), default="0.1.5")
    parser.add_argument("--to-release", choices=("0.2.0",), default="0.2.0")
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--expected-native-version", default=FORMAL_NATIVE_VERSION)
    parser.add_argument("--work", required=True, type=Path)
    parser.add_argument("--management-port", default=24770, type=int)
    parser.add_argument("--bus-port", default=24760, type=int)
    parser.add_argument("--confirm-one-way", required=True)
    args = parser.parse_args(argv)
    args.work = args.work.resolve()
    home = formal_runner_home(args)  # before any subprocess, file mutation or service action
    args.work.mkdir(parents=True, exist_ok=True)
    result = {"evidence": "formal-signed-managed-upgrade", "passed": False, "upgradeExecuted": False}
    host = None
    try:
        docs = prepare_formal_mirror(args, args.work)
        fixture, blobs = formal_seed()
        host = FormalHost(args, home)
        result.update(execute_formal_upgrade(host, docs, fixture, blobs))
    except Exception as error:
        result["error"] = str(error)
        log("formal-assets failed: " + str(error))
    finally:
        if host is not None:
            result["upgradeExecuted"] = host.upgrade_executed
            try:
                host.cleanup()
            except Exception as error:
                result["cleanupError"] = str(error)
        result["passed"] = result.get("upgradeExecuted") is True and "error" not in result and "cleanupError" not in result
        (args.work / "formal-result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    log(json.dumps(result))
    return 0 if result["passed"] else 1


def main(argv: list[str] | None = None) -> int:
    # Windows runners print with cp1252; cutex output contains non-ASCII (e.g. "→").
    for stream in (sys.stdout, sys.stderr):
        if not hasattr(stream, "reconfigure"):
            continue
        stream.reconfigure(encoding="utf-8", errors="replace")
    argv = list(sys.argv[1:] if argv is None else argv)
    if argv and argv[0] == "formal-assets":
        return formal_assets(argv[1:])
    if argv and argv[0] == "signed-native-preflight":
        return signed_native_preflight(argv[1:])
    if argv and argv[0] == "signed-asset-preflight":
        return signed_asset_preflight(argv[1:])
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--from-cutex", required=True, type=Path, help="dir with the debug cutex + cutex-mcp of the installed release")
    ap.add_argument("--from-commit", required=True)
    ap.add_argument("--to-cutex", required=True, type=Path, help="dir with the debug cutex + cutex-mcp of the new release")
    ap.add_argument("--to-commit", required=True)
    ap.add_argument("--components", required=True, type=Path, help="component archives of the new release (build.sh output)")
    ap.add_argument("--work", required=True, type=Path, help="empty or missing work dir (use one with a space)")
    ap.add_argument("--release-meta", type=Path,
                    help="the real release's metadata: <to>.json (--meta), capabilities-<from>.json (--previous-capabilities)")
    ap.add_argument("--new-native", default="none",
                    help="the native the new release brings: none, stub, or a dir with a real native-<v>-<platform>.tar.gz")
    ap.add_argument("--service-name", default="e2e", help="drill label for CUTEX_SERVICE_TEST_NAME")
    ap.add_argument("--unit-dir", help="Linux: the user unit dir the drill units go to (default $XDG_RUNTIME_DIR/systemd/user)")
    ap.add_argument("--claude-python", default=sys.executable, help="fallback Python >= 3.11 for archives without bundled Python")
    ap.add_argument("--rustc", default="rustc")
    ap.add_argument("--generator", help="upgrade-manifest.py (default: next to this file)")
    ap.add_argument("--management-port", type=int, default=24770)
    ap.add_argument("--bus-port", type=int, default=24760)
    ap.add_argument("--confirm-one-way", metavar="RELEASE", help="explicit consent for a one-way test upgrade to this release")
    ap.add_argument("--no-cleanup", action="store_true")
    args = ap.parse_args(argv)
    for name in ("from_cutex", "to_cutex", "components", "release_meta"):
        if getattr(args, name) is not None:
            setattr(args, name, getattr(args, name).resolve())
    if args.new_native not in ("none", "stub"):
        if not Path(args.new_native).is_dir():
            raise SystemExit(f"--new-native {args.new_native}: none, stub or a directory")
        args.new_native = str(Path(args.new_native).resolve())
    if args.release_meta is not None and not args.release_meta.is_dir():
        raise SystemExit(f"--release-meta {args.release_meta} is not a directory")
    if not re.fullmatch(r"[0-9a-f]{40}", args.from_commit) or not re.fullmatch(r"[0-9a-f]{40}", args.to_commit):
        raise SystemExit("--from-commit / --to-commit: full 40-character commits")
    if {args.management_port, args.bus_port} & {24270, 24260, 25370, 25360}:
        raise SystemExit("never the live daemon ports")
    work = args.work.resolve()
    if work.exists() and any(work.iterdir()):
        raise SystemExit(f"{work} is not empty")
    work.mkdir(parents=True, exist_ok=True)
    if " " not in str(work):
        warn(f"the work dir has no space in its path ({work})")
    generator = find_generator(args.generator)
    key = ce.TestKey()
    release, info = build_release(args, work, key, generator)
    from_label, to_label = info["from"][0], info["to"][0]
    server, url = ce.serve(release.served)
    log(f"\nrelease source {url}; work {work}; drill label {args.service_name}")
    # The installed 0.1.0 comes from its signed archive, as a download would.
    dl = work / "dl" / from_label
    with tarfile.open(work / "served" / f"cutex-{from_label}" / f"cutex-{from_label}-{PLATFORM}.tar.gz") as tar:
        tar.extractall(dl)
    for name in ("manifest.json", "manifest.json.minisig"):
        shutil.copyfile(work / "served" / f"cutex-{from_label}" / name, dl / name)
    host = Host(args, work, key, url)
    try:
        before = install_010(host, args, from_label, dl)
        if not failures:
            upgrade_to_011(host, url, to_label, args.to_commit, from_label, before, info["mode"], info["native"], info["removed"], info["one_way"])
        if not failures:
            components(host, args, info["components"])
        if not failures:
            rollback_leg(host, release, generator, work, to_label, args.to_commit)
    except Exception as error:  # noqa: BLE001 - report, collect logs, clean up
        failures.append(f"exception: {error!r}")
        log(f"  FAIL  {error!r}")
        import traceback
        traceback.print_exc()
    finally:
        try:
            host.collect_logs()
        except Exception as error:  # noqa: BLE001
            log(f"  log collection failed: {error!r}")
        if not args.no_cleanup:
            host.cleanup()
        server.shutdown()
    log("")
    for w in warnings:
        log(f"warning: {w}")
    if failures:
        log(f"{len(failures)} check(s) failed:")
        for f in failures:
            log(f"  - {f}")
        return 1
    log("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
