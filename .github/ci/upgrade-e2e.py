#!/usr/bin/env python3
"""End-to-end check of the backend updater (.github/workflows/upgrade-e2e.yml, docs/backend-upgrade.md):
an existing cutex 0.1.0 managed install upgrades to 0.1.1 through `cutex upgrade check / stage / apply`,
on the platform's real service manager, and a broken later release rolls back by itself. The steps below
say 0.1.0 / 0.1.1 for the installed / new release; the same run checks 0.1.1 -> 0.1.2 and 0.1.2 -> 0.1.3
(the workflow's default since the 0.1.3 prep; 0.1.1 on already advertise `components`). With --new-native the
new release also brings a new cute-codex native, as `sign-and-upload.sh cutex --native-run` builds it (0.1.3
ships native 0.160.0-r52.3): the updater stages, preflights and selects it.

    python .github/ci/upgrade-e2e.py --from-cutex DIR --from-commit SHA --to-cutex DIR --to-commit SHA
        --components DIR --work DIR [--release-meta DIR] [--new-native none|stub|DIR] [--service-name LABEL]
        [--unit-dir DIR] [--claude-python PATH] [--rustc RUSTC] [--generator upgrade-manifest.py] [--no-cleanup]

--from-cutex / --to-cutex: directories with DEBUG `cutex` + `cutex-mcp` builds of the two versions (only
debug builds trust the run-time key CUTEX_UPGRADE_TEST_PUBKEY; a release build trusts only the embedded
release keys), built with CUTEX_RELEASE / CUTEX_COMMIT / CUTEX_CHANNEL=release so `cutex --version` names
(<release>, <commit7>) like a release build. --components: component archives from
components/<name>/build.sh of the TO tree (component-<name>-<version>-<platform>.tar.gz + SHA256SUMS +
BUILD-INFO). --release-meta: the release metadata the owner signs the real release with
(.github/ci/release-meta = the desktop repo's scripts/public-export/projects/cutex/release-meta, which
sign-and-upload.sh reads): `<to release>.json` is the generator's --meta, `capabilities-<from release>.json`
its --previous-capabilities (first checked against the FROM build's own list on this platform).
--new-native: the native the new release brings (the generator's --native-dir, as --native-run gives it):
`none` (default: like 0.1.1 / 0.1.2), `stub` (a second stub native, version 0.0.1-e2e-stub), or a directory
with a real `native-<version>-<platform>.tar.gz` of this platform (a cute-codex run's natives-<platform>
artifact, or the archive downloaded from a published cute-codex release; without a BUILD-INFO-<platform>.json
next to it, the archive's own cute-codex-build.json is used). Nothing executes the native: cutex stages,
checks the files of, hashes and selects it.

What it does (nothing here touches a real cutex home, the real service names or the real signing key):

1. A throwaway minisign key. Release archives of both builds (ustar .tar.gz, cutex + cutex-mcp at the
   root, BUILD-INFO with the build's `--version` line and capability list as build-cutex.sh writes them),
   a STUB native (codex-app-server / codex-code-mode-host / codex / schema files that are never executed:
   cutex only hashes and selects them) and a broken release (below). Manifests: 0.1.0 with sequence 1
   in the published 0.1.0 shape, native included (the generator refuses sequence 1); 0.1.1 and the
   broken release with the real generator and the REAL release's arguments, as sign-and-upload.sh runs
   it: --native-dir only with --new-native (0.1.1 / 0.1.2 brought none: no --native-run), --meta release-meta/<to>.json,
   --previous-capabilities release-meta/capabilities-<from>.json (else the FROM build's list); top-level
   `components`, sequence 2 / 3. All signed; served on http://127.0.0.1:<random>.
2. A private marked home (CUTEX_TEST_PRIVATE_HOME + HOME) and the install base under --work (a path
   with a space). The upgrade source is pre-set in <base>/upgrade/settings.json with the background
   check off, so the first daemon never reads the public channel. `cutex service install --channel
   release --base <base>` from the unpacked 0.1.0 archive, with drill service names
   (CUTEX_SERVICE_TEST_NAME, debug builds only):
   - Linux: systemd user units. <home>/.config/systemd/user (cutex's unit dir for a private home) is a
     link to --unit-dir (default $XDG_RUNTIME_DIR/systemd/user, which the user manager reads), so 0.1.0
     itself runs `systemctl --user daemon-reload` + `enable --now`. A drop-in per drill unit gives the
     daemons the private home (the units carry no HOME; the upgrade runner gets it from the CLI through
     `systemd-run --setenv`).
   - Windows: the per-user scheduled task `\\Cutex-<label>\\cutex-supervisor` that 0.1.0 registers and
     starts. The task runs with the user's registry environment, so the private home variables are set
     in HKCU\\Environment for the run (restored at the end). If the task does not bring the supervisor up
     (e.g. no interactive logon session on the runner), the supervisor is started with the same command
     line by this script and that is reported as a warning.
   Then 0.1.0's native as a real 0.1.0 managed install has it (AGENT-INSTALL.md B.3 / 5.3): the native
   archive unpacked into <base>/natives/<version>, releases/0.1.0/manifests/default.json (v4 bundle:
   that native + 0.1.0's cutex-mcp facade) selected with `cutex human install-runtime`.
3. Upgrade 0.1.0 -> 0.1.1 with the INSTALLED 0.1.0 binary: check offers 0.1.1 in the mode the real
   manifest has (0.1.1: `full`, from its meta), stage (preflight: the new binary's --version and
   selftest, ...; with --new-native also the staged native: `nativeVersion` offered, natives/<new> unpacked,
   preflight check `native`), apply (the 0.1.1 binary runs the run). Asserts committed in that
   mode, `cutex --version`, GET /v2/upgrade, capabilities only gained (incl. `components`),
   GET /v2/components, the services up, and what NEW agents get: full -> runtime/light/deployment.json
   selects releases/0.1.1/manifests/default.json = 0.1.0's native files (with --new-native: natives/<new>)
   + the 0.1.1 cutex-mcp facade;
   reload -> they keep releases/0.1.0/manifests/default.json (asserted and reported as a warning).
4. Components from the 0.1.1 manifest: claude-runtime -> needsUser claude_cli_missing (no `claude` on
   PATH); Linux: job-service -> ready (a systemd drill unit, socket answers; its Codex runtime is the
   selected stub native).
5. Rollback leg: a release whose `cutex` is a stub (passes --version and delegates every other command to
   the real 0.1.1 binary, but `management serve` and `agent serve` exit) stages with a clean preflight;
   its apply must end `rolled-back` on 0.1.1 by itself (not needs-owner: the runner waits for readiness,
   then switches back), with the services, components and (full mode) the 0.1.1 runtime selection back.
6. Logs (tokens scrubbed, config.json / accounts.json never copied) to <work>/logs; cleanup:
   `cutex service uninstall`, drop-ins, the unit-dir link, HKCU variables.

Needs: Python 3.10+ with `cryptography` (CI pins it), rustc (for the broken stub), and on Linux a
systemd user manager (CI: loginctl enable-linger).
"""

from __future__ import annotations

import argparse
import hashlib
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
BROKEN_LABEL = "0.1.2-e2e-broken"
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


def native_build(out: Path, version: str = NATIVE_VERSION) -> Path:
    """A stub native: the updater stages, hashes and selects these files; nothing executes them here."""
    stubs = out / "files"
    stubs.mkdir(parents=True)
    files = {}
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
        return native_build(out, NEW_NATIVE_STUB), NEW_NATIVE_STUB
    given = Path(spec)
    archives = sorted(given.glob(f"native-*-{PLATFORM}.tar.gz"))
    if len(archives) != 1:
        raise SystemExit(f"--new-native {given}: expected one native-*-{PLATFORM}.tar.gz, found {len(archives)}")
    info_path = given / f"BUILD-INFO-{PLATFORM}.json"
    if info_path.is_file():
        info = json.loads(info_path.read_text(encoding="utf-8"))
    else:
        with tarfile.open(archives[0]) as tar:
            member = tar.extractfile("cute-codex-build.json")
            if member is None:
                raise SystemExit(f"{archives[0]}: no cute-codex-build.json and no {info_path.name}")
            info = json.loads(member.read().decode("utf-8"))
    version = str(info.get("version", ""))
    if info.get("platform") != PLATFORM or archives[0].name != f"native-{version}-{PLATFORM}.tar.gz":
        raise SystemExit(f"{archives[0]}: build record says {version} / {info.get('platform')}")
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
                   new_native: str | None = None) -> None:
    phase(f"upgrade {from_label} -> {to_label} ({mode}{', native ' + new_native if new_native else ''}) "
          f"with the installed {from_label} binary")
    host.cli("upgrade", "config", "--source", url)
    _, available, _ = host.cli("upgrade", "check", show=True)
    available = available or {}
    check(available.get("release") == to_label, f"check offers {to_label} (got {available.get('release')})")
    check(available.get("sequence") == 2, f"{to_label} has sequence 2")
    check(available.get("mode") == mode, f"{from_label} classifies {to_label} as {mode} (got {available.get('mode')})")
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
    natives = sorted(p.name for p in (host.base / "natives").iterdir())
    if new_native:
        check(checks.get("native") is True, f"preflight check native passed (natives/{new_native} complete)")
        check(natives == sorted([NATIVE_VERSION, new_native]),
              f"natives/ holds {from_label}'s {NATIVE_VERSION} and the staged {new_native} (got {natives})")
        staged_dir = host.base / "natives" / new_native
        for name in (f"codex-app-server{EXE}", f"codex-code-mode-host{EXE}", "codex_app_server_protocol.schemas.json"):
            check((staged_dir / name).is_file(), f"natives/{new_native}/{name} unpacked")
        selected = host.selected_runtime()
        check(same_path(selected.get("bundle_manifest"), host.base / "releases" / from_label / "manifests" / "default.json"),
              f"staging selects nothing yet: new agents still use releases/{from_label}/manifests/default.json")
    else:
        check("native" not in checks, "no native preflight (the manifest brings no native)")
        check(natives == [NATIVE_VERSION], f"natives/ holds only {from_label}'s {NATIVE_VERSION} (got {natives})")
    t0 = time.time()
    code, _, out = host.cli("upgrade", "apply", "--max-wait", "600", show=True, ok=False)
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
    lost = sorted(before - after - HOST_DEPENDENT)
    check(status == 200 and not lost, f"capabilities only gained (lost: {lost})")
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

    no_claude = {"CUTEX_CLAUDE_PYTHON": args.claude_python}
    _, out, _ = host.cli("component", "install", "claude-runtime", extra=no_claude, path=ce.base_path(), show=True)
    comp = ce.component_of(out)
    check(comp.get("state") == "needsUser", f"claude-runtime: needsUser (got {comp.get('state')})")
    check(ce.needs_code(comp) == "claude_cli_missing", f"claude-runtime: needsUser claude_cli_missing (got {ce.needs_code(comp)})")
    version = versions["claude-runtime"]
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
            warn(f"no {meta_file}: the {to_label} manifest is generated without --meta, as sign-and-upload.sh would")
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
    cutex_build(rel / "b-to", to_label, args.to_commit, to_line, to_caps, pair(args.to_cutex))
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
    generate(generator, "manifest", "--cutex-dir", str(rel / "b-broken"), "--component-dir", str(args.components),
             "--sequence", "next", "--previous", str(rel / "m-to" / "manifest.json"), *meta_args,
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
                     "components": {n: c["version"] for n, c in to_doc.get("components", {}).items()}}


def main(argv: list[str] | None = None) -> int:
    # Windows runners print with cp1252; cutex output contains non-ASCII (e.g. "→").
    for stream in (sys.stdout, sys.stderr):
        stream.reconfigure(encoding="utf-8", errors="replace")
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
    ap.add_argument("--claude-python", default=sys.executable, help="Python >= 3.11 for the Claude runtime venv")
    ap.add_argument("--rustc", default="rustc")
    ap.add_argument("--generator", help="upgrade-manifest.py (default: next to this file)")
    ap.add_argument("--management-port", type=int, default=24770)
    ap.add_argument("--bus-port", type=int, default=24760)
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
            upgrade_to_011(host, url, to_label, args.to_commit, from_label, before, info["mode"], info["native"])
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
