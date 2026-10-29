#!/usr/bin/env python3
"""End-to-end check of optional components through the real `cutex component` CLI
(.github/workflows/component-e2e.yml; components/README.md).

    python .github/ci/component-e2e.py claude-runtime --cutex PATH --archives DIR --work DIR
        [--old-python PATH] [--npm-claude-prefix DIR]
    python .github/ci/component-e2e.py job-service --cutex PATH --archives DIR --work DIR

Each run:

1. generates a throwaway minisign key (Ed25519, prehashed `ED`) and writes a signed test release:
   `<work>/served/component-<name>-<version>/<archive>` and a managed install base
   `<work>/base` (install.json, releases/<label>/manifest.json + .minisig). The manifest names the
   component archives found in --archives (component-<name>-<version>-<platform>.tar.gz);
2. serves <work>/served on http://127.0.0.1:<random port> from a thread of this process;
3. runs a DEBUG cutex (it trusts CUTEX_UPGRADE_TEST_PUBKEY; a release build does not) in a fresh
   private marked home (CUTEX_TEST_PRIVATE_HOME + HOME) with CUTEX_INSTALL_BASE = <work>/base. The
   work dir may contain spaces (the workflow uses one that does);
4. drives `cutex upgrade config --source`, `cutex component install|status|list|remove|prune
   --dry-run` and asserts the states and needsUser codes / steps.

claude-runtime covers bundled standalone Python with empty PATH, the bundled CLI default,
CLI-free archives, explicit missing/system CLI overrides, optional npm CLI, Python-too-old
overrides, remove and prune. Legacy archives retain system Python / `py -3` discovery checks.
No login or model request is made (only version and unauthenticated auth-status probes).

Needs: Python 3.10+ with `cryptography` (CI pins it), a debug cutex built from this checkout.
Nothing here touches a real cutex home: the private home is created empty under --work.
"""

from __future__ import annotations

import argparse
import base64
import functools
import hashlib
import http.server
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import threading
from pathlib import Path

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

WINDOWS = os.name == "nt"
PLATFORM = "windows-x64" if WINDOWS else "linux-x64"
RELEASE = "e2e-0.0.0"
MARKER = ".cutex-test-private-home"
# Variables that change what the adapter / Claude CLI / cutex would do; never inherited.
DROP_ENV = re.compile(r"^(ANTHROPIC_|CLAUDE_|CUTEX_|PYTHON|VIRTUAL_ENV$)")

failures: list[str] = []


def log(message: str) -> None:
    print(message, flush=True)


def check(condition: bool, message: str) -> bool:
    log(("  ok    " if condition else "  FAIL  ") + message)
    if not condition:
        failures.append(message)
    return condition


# ------------------------------------------------------------------ minisign


class TestKey:
    """A minisign key pair: public key line `Ed`+id+pk, prehashed `ED` signatures."""

    def __init__(self) -> None:
        self.secret = Ed25519PrivateKey.generate()
        self.key_id = os.urandom(8)
        pk = self.secret.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
        self.public_line = base64.b64encode(b"Ed" + self.key_id + pk).decode()

    def sign(self, data: bytes, file_name: str) -> str:
        signature = self.secret.sign(hashlib.blake2b(data, digest_size=64).digest())
        trusted = f"timestamp:1735689600\tfile:{file_name}\thashed"
        global_signature = self.secret.sign(signature + trusted.encode())
        return (
            "untrusted comment: signature from a throwaway CI test key\n"
            f"{base64.b64encode(b'ED' + self.key_id + signature).decode()}\n"
            f"trusted comment: {trusted}\n"
            f"{base64.b64encode(global_signature).decode()}\n"
        )


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


# ------------------------------------------------------------- test release


def write_release(work: Path, archives: Path, key: TestKey) -> dict[str, str]:
    """The signed test release and the managed install base. Returns {component: version}."""
    served = work / "served"
    base = work / "base"
    components: dict[str, dict] = {}
    # build.sh writes BUILD-INFO-component-<name>-<platform>.json next to each archive (the archive name
    # alone is ambiguous: names and versions both contain hyphens).
    for info_path in sorted(archives.rglob(f"BUILD-INFO-component-*-{PLATFORM}.json")):
        info = json.loads(info_path.read_text(encoding="utf-8"))
        m = {"name": info["name"], "version": info["version"]}
        archive = info_path.parent / f"component-{m['name']}-{m['version']}-{PLATFORM}.tar.gz"
        if info.get("platform") != PLATFORM or not archive.is_file():
            raise SystemExit(f"{info_path}: no matching {archive.name}")
        tag = f"component-{m['name']}-{m['version']}"
        (served / tag).mkdir(parents=True, exist_ok=True)
        shutil.copyfile(archive, served / tag / archive.name)
        components[m["name"]] = {
            "version": m["version"],
            "artifacts": {PLATFORM: {"name": archive.name, "sha256": sha256(archive), "size": archive.stat().st_size}},
        }
    if not components:
        raise SystemExit(f"no component-*-{PLATFORM}.tar.gz under {archives}")
    manifest = {
        "schema": "cutex/release-manifest/v1",
        "channel": "release",
        "sequence": 1,
        "publishedAt": "2025-01-01T00:00:00Z",
        "notes": {"en": "component e2e test release (throwaway key)"},
        "cutex": {"release": RELEASE, "commit": "0" * 40, "artifacts": {}},
        "components": components,
        "mode": "reload",
    }
    release_dir = base / "releases" / RELEASE
    release_dir.mkdir(parents=True)
    data = json.dumps(manifest, indent=2).encode()
    (release_dir / "manifest.json").write_bytes(data)
    (release_dir / "manifest.json.minisig").write_text(key.sign(data, "manifest.json"), encoding="utf-8")
    install = {
        "schema": "cutex/install/v1",
        "os": "windows" if WINDOWS else "linux",
        "serviceManager": "cutex-supervisor" if WINDOWS else "systemd",
        "channel": "release",
        "createdAt": "2025-01-01T00:00:00Z",
        "activeRelease": RELEASE,
        "previousRelease": None,
    }
    (base / "install.json").write_text(json.dumps(install, indent=2), encoding="utf-8")
    return {name: c["version"] for name, c in components.items()}


def serve(directory: Path) -> tuple[http.server.ThreadingHTTPServer, str]:
    class Quiet(http.server.SimpleHTTPRequestHandler):
        def log_message(self, fmt: str, *args) -> None:  # noqa: A002
            log(f"  http  {fmt % args}")

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Quiet, directory=str(directory)))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f"http://127.0.0.1:{server.server_address[1]}"


# ---------------------------------------------------------------- the CLI


class Cutex:
    def __init__(self, exe: Path, work: Path, key: TestKey) -> None:
        self.exe = exe
        self.home = work / "home"
        self.home.mkdir(parents=True)
        (self.home / MARKER).touch()
        self.base = work / "base"
        cutex_dir = self.home / ".cutex"
        cutex_dir.mkdir()
        # Tokens for the agent bus / management (never used for a request here); test ports far from defaults.
        config = {"agent_bus_enabled": True, "agent_bus_token": os.urandom(32).hex(), "management_api_token": os.urandom(32).hex(),
                  "management_port": 24570, "agent_bus_port": 24560}
        (cutex_dir / "config.json").write_text(json.dumps(config), encoding="utf-8")
        if not WINDOWS:
            (cutex_dir / "config.json").chmod(0o600)
        env = {k: v for k, v in os.environ.items() if not DROP_ENV.match(k)}
        env.update({
            "HOME": str(self.home),
            "USERPROFILE": str(self.home),
            "APPDATA": str(self.home / "AppData" / "Roaming"),
            "LOCALAPPDATA": str(self.home / "AppData" / "Local"),
            "CUTEX_TEST_PRIVATE_HOME": str(self.home),
            "CUTEX_INSTALL_BASE": str(self.base),
            "CUTEX_UPGRADE_TEST_PUBKEY": key.public_line,
            "NO_COLOR": "1",
        })
        self.env = env

    def run(self, *args: str, extra: dict[str, str] | None = None, drop: tuple[str, ...] = (), path: str | None = None,
            ok: bool = True, want_json: bool = True) -> tuple[int, dict | None, str]:
        env = dict(self.env)
        env.update(extra or {})
        for k in drop:
            env.pop(k, None)
        if path is not None:
            env["PATH"] = path
        log(f"$ cutex {' '.join(args)}")
        p = subprocess.run([str(self.exe), *args], env=env, capture_output=True, text=True, encoding="utf-8",
                           errors="replace", timeout=900)
        out = p.stdout
        start = out.find("{")
        value = None
        if start >= 0:
            try:
                value = json.loads(out[start:])
            except ValueError:
                value = None
        if p.returncode != 0 or (want_json and value is None):
            log(f"  exit {p.returncode}\n  stdout: {out[-4000:]}\n  stderr: {p.stderr[-4000:]}")
        if ok:
            check(p.returncode == 0, f"cutex {' '.join(args)} exits 0 (got {p.returncode})")
        return p.returncode, value, p.stderr

    @property
    def deployment(self) -> Path:
        return self.home / ".cutex" / "runtime" / "claude" / "deployment.json"


def component_of(value: dict | None) -> dict:
    if not value:
        return {}
    return value.get("component", value)


def needs_code(component: dict) -> str | None:
    return ((component.get("detail") or {}).get("needsUser") or {}).get("code")


def expect_needs_user(component: dict, code: str, label: str) -> None:
    check(component.get("state") == "needsUser", f"{label}: state needsUser (got {component.get('state')})")
    check(needs_code(component) == code, f"{label}: needsUser code {code} (got {needs_code(component)})")
    steps = component.get("steps")
    check(isinstance(steps, list) and len(steps) > 0 and all(isinstance(s, str) and s.strip() for s in steps),
          f"{label}: top-level steps are non-empty strings ({steps!r})")


def status(cutex: Cutex, name: str, **kw) -> dict:
    _, value, _ = cutex.run("component", "status", name, **kw)
    return component_of(value)


# -------------------------------------------------------------- claude-runtime


STUB_RS = r"""
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--version") {
        println!("0.0.0 (cutex e2e stub)");
        return;
    }
    eprintln!("cutex e2e stub: not a real Claude Code CLI");
    std::process::exit(1);
}
"""

STUB_SH = """#!/bin/sh
if [ "$1" = --version ]; then echo "0.0.0 (cutex e2e stub)"; exit 0; fi
echo "cutex e2e stub: not a real Claude Code CLI" >&2
exit 1
"""


def make_stub_claude(home: Path, work: Path) -> Path:
    bin_dir = home / ".local" / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    if WINDOWS:
        src = work / "stub-claude.rs"
        src.write_text(STUB_RS, encoding="utf-8")
        target = bin_dir / "claude.exe"
        subprocess.run(["rustc", "-O", "-o", str(target), str(src)], check=True, cwd=str(work))
    else:
        target = bin_dir / "claude"
        target.write_text(STUB_SH, encoding="utf-8")
        target.chmod(0o755)
    return target


def base_path() -> str:
    """PATH without any directory that has a `claude` (the runner image must not leak one in)."""
    names = ("claude.exe", "claude.cmd", "claude") if WINDOWS else ("claude",)
    keep = [d for d in os.environ.get("PATH", "").split(os.pathsep) if d and not any((Path(d) / n).is_file() for n in names)]
    return os.pathsep.join(keep)


def py_launcher_python() -> str | None:
    if not WINDOWS or not shutil.which("py"):
        return None
    try:
        out = subprocess.run(["py", "-3", "-c", "import sys,json; print(json.dumps([sys.executable, sys.version_info[:2]]))"],
                             capture_output=True, text=True, timeout=60)
        exe, version = json.loads(out.stdout.strip())
    except (OSError, ValueError, subprocess.SubprocessError):
        return None
    return exe if tuple(version) >= (3, 11) else None


def bundled_payload(archives: Path, version: str) -> tuple[bool, bool]:
    """Detect capabilities from the actual archive, including the supported CLI-free build."""
    matches = list(archives.rglob(f"component-claude-runtime-{version}-{PLATFORM}.tar.gz"))
    if len(matches) != 1:
        raise SystemExit(f"expected exactly one Claude archive, found {len(matches)}")
    with tarfile.open(matches[0], "r:gz") as archive:
        names = {m.name.removeprefix("./") for m in archive.getmembers() if m.isfile()}
    return "python-runtime.json" in names, "claude-cli-runtime.json" in names


def expect_deployment(cutex: Cutex, version_dir: Path, cli: Path) -> None:
    check(cutex.deployment.is_file(), "selected for new agents")
    if cutex.deployment.is_file():
        d = json.loads(cutex.deployment.read_text(encoding="utf-8"))
        check(Path(d.get("claude_cli", "")) == cli, f"deployment uses CLI {cli}")
        check(Path(d.get("adapter_source", "")) == version_dir, "deployment uses installed adapter")
        check(Path(d.get("python", "")).is_file(), "deployment Python exists")


def claude_runtime(args: argparse.Namespace, cutex: Cutex, version: str, work: Path) -> None:
    name = "claude-runtime"
    path = base_path()
    tui = {"CUTEX_CLAUDE_TUI_BINARY": str(args.cutex)}
    bundled_python, bundled_cli = bundled_payload(args.archives, version)
    version_dir = cutex.base / "components" / name / version

    _, listed, _ = cutex.run("component", "list", "--json")
    names = [c.get("name") for c in (listed or {}).get("components", [])]
    check(name in names, f"component list names {name} ({names})")
    available = next((c for c in (listed or {}).get("components", []) if c.get("name") == name), {}).get("available")
    check(available == version or (isinstance(available, dict) and available.get("version") == version),
          f"{name}: available {version} from the signed manifest (got {available!r})")

    if args.old_python:
        log("\n== phase: Python too old")
        _, out, _ = cutex.run("component", "install", name, extra={"CUTEX_CLAUDE_PYTHON": args.old_python}, path=path)
        comp = component_of(out)
        expect_needs_user(comp, "python_too_old", "old Python")
        check(comp.get("selected") == version, f"old Python: version {version} installed (got {comp.get('selected')})")
        version_dir = cutex.base / "components" / name / version
        check((version_dir / "receipt.json").is_file(), f"signed archive unpacked with a receipt in {version_dir}")
        check(not cutex.deployment.exists(), "old Python: nothing selected for new agents")

    log("\n== phase: default prepare without a system Claude CLI")
    # An empty PATH proves that r32's prepare needs neither system Python nor system CLI.
    # Absolute cutex/TUI paths keep the harness itself independent of PATH.
    default_path = "" if bundled_python else path
    _, out, _ = cutex.run("component", "install", name,
                          extra={} if bundled_cli else tui, path=default_path)
    comp = component_of(out)
    expect_needs_user(comp, "codex_runtime_missing" if bundled_cli else "claude_cli_missing", "default install")
    prepare = ((out or {}).get("result") or {}).get("prepare") or {}
    receipt = prepare.get("receipt") or {}
    base_python = receipt.get("basePython")
    log(f"  venv from {base_python} {receipt.get('basePythonVersion')}")
    check(bool(base_python), "venv prepared from vendored wheels")
    launcher = py_launcher_python()
    if bundled_python:
        check(receipt.get("basePythonSource") == "bundled", "prepare uses standalone bundled Python")
        check(Path(base_python or "").is_relative_to(version_dir / "prepared" / "python"),
              "base Python is inside the component's prepared/python")
        metadata = json.loads((version_dir / "python-runtime.json").read_text(encoding="utf-8"))
        check(receipt.get("basePythonVersion") == metadata.get("pythonVersion"),
              "prepared Python version matches the archive's pin")
    elif WINDOWS:
        if launcher:
            check(Path(base_python or "").resolve() == Path(launcher).resolve(),
                  f"py -3 discovery: venv base {base_python} is `py -3`'s {launcher}")
        else:
            log("  note  no `py` launcher with Python 3.11+ on this runner: `py -3` discovery not exercised")
    venv_dir = cutex.base / "components" / name / version / "prepared" / "venv"
    check(venv_dir.is_dir(), f"venv inside the version dir ({venv_dir})")
    check(not cutex.deployment.exists(), "no claude: nothing selected for new agents")

    if bundled_cli:
        log("\n== phase: bundled CLI + bundled Python, no system tools -> login required")
        _, out, _ = cutex.run("component", "install", name, extra=tui, path=default_path)
        expect_needs_user(component_of(out), "claude_login_required", "bundled CLI")
        cli = Path(receipt.get("bundledClaudeCli") or "")
        check(cli.is_file() and cli.is_relative_to(venv_dir), "bundled CLI is inside the prepared venv")
        cli_metadata = json.loads((version_dir / "claude-cli-runtime.json").read_text(encoding="utf-8"))
        check(bool(receipt.get("sdk")) and receipt.get("sdk") == cli_metadata.get("sdk"),
              "prepared SDK carrying the bundled CLI matches the archive's pin")
        expect_deployment(cutex, version_dir, cli)
        check(status(cutex, name, extra=tui, path=default_path).get("state") == "needsUser",
              "bundled status agrees: login required")
        cutex.run("component", "remove", name)

    log("\n== phase: explicit missing system CLI (must not silently fall back)")
    missing_cli = {**tui, "CUTEX_CLAUDE_CLI": str(work / "missing-cli")}
    _, out, _ = cutex.run("component", "install", name, extra=missing_cli, path=path)
    expect_needs_user(component_of(out), "claude_cli_missing", "explicit missing CLI")

    log("\n== phase: stub claude, no Codex runtime")
    stub = make_stub_claude(cutex.home, work)
    override = {"CUTEX_CLAUDE_CLI": str(stub)}
    _, out, _ = cutex.run("component", "install", name, extra=override, path=path)
    expect_needs_user(component_of(out), "codex_runtime_missing", "no Codex runtime")

    log("\n== phase: stub claude + TUI binary -> login required")
    _, out, _ = cutex.run("component", "install", name, extra={**tui, **override}, path=path)
    comp = component_of(out)
    expect_needs_user(comp, "claude_login_required", "stub claude")
    check(cutex.deployment.is_file(), f"selected for new agents ({cutex.deployment})")
    if cutex.deployment.is_file():
        d = json.loads(cutex.deployment.read_text(encoding="utf-8"))
        check(Path(d.get("claude_cli", "")) == stub, f"deployment claude_cli is the stub {stub.name} (got {d.get('claude_cli')})")
        check(Path(d.get("adapter_source", "")) == cutex.base / "components" / name / version,
              f"deployment adapter_source is the version dir (got {d.get('adapter_source')})")
        check(Path(d.get("python", "")).is_file(), f"deployment python exists ({d.get('python')})")
    check(status(cutex, name, extra={**tui, **override}, path=path).get("state") == "needsUser", "status agrees: needsUser")

    if args.npm_claude_prefix:
        log("\n== phase: the real Claude Code CLI from npm")
        stub.unlink()
        prefix = Path(args.npm_claude_prefix)
        npm_bin = prefix if WINDOWS else prefix / "bin"
        # r32 defaults to bundled; explicitly select the alternative CLI under test.
        npm_cli = npm_bin / ("claude.cmd" if WINDOWS else "claude")
        _, out, _ = cutex.run("component", "install", name,
                              extra={**tui, "CUTEX_CLAUDE_CLI": str(npm_cli)},
                              path=str(npm_bin) + os.pathsep + path)
        comp = component_of(out)
        expect_needs_user(comp, "claude_login_required", "npm claude")
        select = ((out or {}).get("result") or {}).get("select") or {}
        log(f"  claude --version: {select.get('claudeCliVersion')}")
        check("Claude Code" in str(select.get("claudeCliVersion")), "the real CLI answered --version")
        if cutex.deployment.is_file():
            cli = json.loads(cutex.deployment.read_text(encoding="utf-8")).get("claude_cli", "")
            log(f"  claude_cli = {cli}")
            check(Path(cli).parent == npm_bin, f"resolved from PATH ({npm_bin})")
            if WINDOWS:
                check(cli.lower().endswith((".cmd", ".exe")), f"Windows CLI is a .cmd or .exe ({cli})")

    log("\n== phase: remove = deselect only")
    _, out, _ = cutex.run("component", "remove", name)
    comp = status(cutex, name)
    check(comp.get("state") in ("missing",), f"after remove: state missing (got {comp.get('state')})")
    check(not cutex.deployment.exists(), "after remove: deployment.json gone (new agents cannot pick it)")
    versions = [v.get("version") for v in comp.get("versions") or []]
    check(version in versions, f"after remove: version {version} kept ({versions})")
    check((cutex.base / "components" / name / version).is_dir(), "after remove: the version dir is still there")

    prune_dry_run(cutex, name, version)


def prune_dry_run(cutex: Cutex, name: str, version: str) -> None:
    """prune keeps the version the active release names, so a dry run lists nothing here."""
    log("\n== phase: prune --dry-run")
    _, out, _ = cutex.run("component", "prune", name, "--dry-run")
    check((out or {}).get("dryRun") is True, "prune --dry-run reports dryRun")
    check((out or {}).get("removed") == [], f"prune keeps {version}: the active release names it ({(out or {}).get('removed')!r})")
    check((cutex.base / "components" / name / version).is_dir(), "prune --dry-run deleted nothing")


# ----------------------------------------------------------------- job-service


def fake_codex_runtime(cutex: Cutex, work: Path) -> Path:
    """A selected Codex runtime (runtime/light/deployment.json + a v4 bundle manifest) whose files are
    stubs: the Job Service only hashes its launcher (`cli`), no agent is ever launched."""
    stubs = work / "fake-codex"
    stubs.mkdir()
    files = {}
    for key, file_name in (("executable", "codex-app-server"), ("cli", "codex"), ("code_mode_host", "codex-code-mode-host"),
                           ("facade", "cutex-mcp")):
        path = stubs / file_name
        path.write_text(f"#!/bin/sh\n# cutex e2e stub {file_name}\nexit 0\n", encoding="utf-8")
        path.chmod(0o755)
        files[key] = path
    files["schema"] = stubs / "codex_app_server_protocol.schemas.json"
    files["schema"].write_text("{}\n", encoding="utf-8")
    native_home = cutex.home / ".cutex" / "codex-home"
    native_home.mkdir(parents=True, exist_ok=True)
    files["shared_config"] = native_home / "config.toml"
    files["shared_config"].write_text("", encoding="utf-8")
    bundle = {"version": 4, "upstream_commit": "0" * 40}
    for key, path in files.items():
        bundle[key] = {"path": str(path.resolve()), "sha256": sha256(path)}
    manifest = stubs / "bundle-manifest.json"
    manifest.write_text(json.dumps(bundle, indent=2), encoding="utf-8")
    light = cutex.home / ".cutex" / "runtime" / "light"
    light.mkdir(parents=True, exist_ok=True)
    deployment = {"native_home": str(native_home.resolve()), "bundle_manifest": str(manifest.resolve()), "job_mcp": None}
    (light / "deployment.json").write_text(json.dumps(deployment, indent=2), encoding="utf-8")
    return files["cli"]


def job_service(args: argparse.Namespace, cutex: Cutex, version: str, work: Path) -> None:
    name = "job-service"
    test_name = os.environ.get("CUTEX_SERVICE_TEST_NAME", "")
    if not test_name or not os.environ.get("CUTEX_COMPONENT_TEST_UNIT_DIR"):
        raise SystemExit("job-service: set CUTEX_SERVICE_TEST_NAME and CUTEX_COMPONENT_TEST_UNIT_DIR (a drill unit, never the real one)")
    extra = {k: os.environ[k] for k in ("CUTEX_SERVICE_TEST_NAME", "CUTEX_COMPONENT_TEST_UNIT_DIR", "XDG_RUNTIME_DIR",
                                         "DBUS_SESSION_BUS_ADDRESS") if os.environ.get(k)}
    unit = f"cutex-{test_name}-job-service.service"

    log("\n== phase: no Codex runtime")
    _, out, _ = cutex.run("component", "install", name, extra=extra, ok=False)
    comp = component_of(out) if out and "component" in out else status(cutex, name, extra=extra)
    log(f"  state {comp.get('state')} steps {comp.get('steps')}")
    check(comp.get("state") in ("needsUser", "missing"), f"without a Codex runtime: not ready (got {comp.get('state')})")
    check(not shutil.which("systemctl") or subprocess.run(["systemctl", "--user", "is-active", "--quiet", unit]).returncode != 0,
          f"without a Codex runtime: {unit} is not running")

    log("\n== phase: install with a (stub) Codex runtime")
    fake_codex_runtime(cutex, work)
    _, out, _ = cutex.run("component", "install", name, extra=extra)
    comp = status(cutex, name, extra=extra)
    detail = comp.get("detail") or {}
    running = detail.get("running") or {}
    log(json.dumps(comp, indent=2)[:6000])
    check(comp.get("state") == "ready", f"{name} state ready (got {comp.get('state')}, steps {comp.get('steps')})")
    check(comp.get("selected") == version, f"{name} {version} selected (got {comp.get('selected')})")
    check((cutex.base / "components" / name / version / "receipt.json").is_file(), "signed archive unpacked with a receipt")
    check(isinstance(running.get("pid"), int) and running["pid"] > 0, f"the Job Service socket answered (running = {running!r})")
    check(running.get("version") == version, f"the answering Job Service is {version} (got {running.get('version')})")
    check(detail.get("descriptorSelected") is True, "the Job MCP descriptor is selected for new agents")
    pinned = next((v.get("pinnedBy") for v in comp.get("versions") or [] if v.get("version") == version), None)
    check(isinstance(pinned, int) and pinned >= 1, f"{version} is pinned by the selected descriptor (pinnedBy {pinned})")
    check(subprocess.run(["systemctl", "--user", "is-active", "--quiet", unit]).returncode == 0, f"{unit} is active")

    log("\n== phase: remove")
    cutex.run("component", "remove", name, extra=extra)
    comp = status(cutex, name, extra=extra)
    check(comp.get("state") == "missing", f"after remove: state missing (got {comp.get('state')})")
    check(not (comp.get("detail") or {}).get("running"), "after remove: no service answers")
    check(subprocess.run(["systemctl", "--user", "is-active", "--quiet", unit]).returncode != 0, f"after remove: {unit} stopped")
    check((cutex.base / "components" / name / version).is_dir(), "after remove: the version dir is kept")
    prune_dry_run(cutex, name, version)


# --------------------------------------------------------------------- main


def main(argv: list[str] | None = None) -> int:
    # Windows runners print with cp1252; cutex output contains non-ASCII (e.g. "→").
    for stream in (sys.stdout, sys.stderr):
        stream.reconfigure(encoding="utf-8", errors="replace")
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("component", choices=["claude-runtime", "job-service"])
    ap.add_argument("--cutex", required=True, type=Path, help="debug cutex binary")
    ap.add_argument("--archives", required=True, type=Path, help="dir with component-*-<platform>.tar.gz")
    ap.add_argument("--work", required=True, type=Path, help="empty or missing work dir (may contain spaces)")
    ap.add_argument("--old-python", help="claude-runtime: a Python < 3.11 for the python_too_old phase")
    ap.add_argument("--npm-claude-prefix", help="claude-runtime: npm -g prefix with @anthropic-ai/claude-code")
    args = ap.parse_args(argv)
    args.cutex = args.cutex.resolve()
    work = args.work.resolve()
    if work.exists() and any(work.iterdir()):
        raise SystemExit(f"{work} is not empty")
    work.mkdir(parents=True, exist_ok=True)
    key = TestKey()
    versions = write_release(work, args.archives.resolve(), key)
    if args.component not in versions:
        raise SystemExit(f"no {args.component} archive for {PLATFORM} in {args.archives} (found {sorted(versions)})")
    server, url = serve(work / "served")
    log(f"test release {RELEASE} ({', '.join(f'{k} {v}' for k, v in versions.items())}) on {url}; work {work}")
    try:
        cutex = Cutex(args.cutex, work, key)
        _, _, _ = cutex.run("--version", want_json=False)
        cutex.run("upgrade", "config", "--source", url, want_json=False)
        if args.component == "claude-runtime":
            claude_runtime(args, cutex, versions[args.component], work)
        else:
            job_service(args, cutex, versions[args.component], work)
    finally:
        server.shutdown()
    log("")
    if failures:
        log(f"{len(failures)} check(s) failed:")
        for f in failures:
            log(f"  - {f}")
        return 1
    log("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
