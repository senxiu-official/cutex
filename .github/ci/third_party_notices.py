#!/usr/bin/env python3
"""THIRD_PARTY_NOTICES.md for a Rust release archive (cutex, the job-service component, the cute-codex
natives): every third-party crate the shipped binaries are built from, with its license expression and
the text of the license / notice files its published source carries.

    python3 third_party_notices.py --manifest-path DIR/Cargo.toml --target TRIPLE --package NAME [...]
        [--title TEXT] [--intro TEXT] [--cargo CMD] [--toolchain NAME] [--metadata FILE] --out FILE

Packages: the dependency closure of the --package roots (workspace packages, at least one) for
--target, from `cargo metadata --locked --offline --filter-platform TRIPLE` (no network: run it after
the build has fetched the crates; --metadata reads a saved `cargo metadata --format-version 1` output
instead). Normal and build dependencies are followed, dev dependencies are not. Workspace members are
walked through but not listed (they are the project's own code, covered by its LICENSE / NOTICE).

License texts: regular files at the top of each package's source directory named LICENSE*, LICENCE*,
COPYING*, NOTICE*, COPYRIGHT*, UNLICENSE*, PATENTS* (not source files such as license.rs), files of
those names one directory down (bundled sources, e.g. quickjs/LICENSE of a -sys crate), every file in
a top-level LICENSES/ directory, and the manifest's `license-file`. Texts are compared after
normalising line endings and trailing whitespace; an identical text is printed once with every package
that ships it. Packages whose published source has no such file are listed in their own section with
their license expression and repository (their texts must be added by hand if a license requires it).

The output is deterministic (sorted by package name, version and source; no dates, no local paths), so
a rebuild of the same Cargo.lock gives the same bytes. Python 3.9+, standard library only.
"""

from __future__ import annotations

import argparse
import json
import re
import shlex
import subprocess
import sys
from pathlib import Path

LICENSE_NAME = re.compile(r"^(licen[cs]e|copying|notice|copyright|unlicen[cs]e|patents)([._-].*)?$", re.I)
# Extensions of source / build files that can match the names above (license.rs, copyright.py, ...).
NOT_A_TEXT = re.compile(r"\.(rs|toml|py|sh|ps1|js|mjs|ts|json|ya?ml|c|h|cc|cpp|hpp|go|rb|pl|in|lock|bzl|bazel|gn|gni)$", re.I)
LICENSE_DIRS = ("LICENSES", "licenses")
MAX_TEXT = 512 * 1024  # a license text larger than this is not a license text


def cargo_metadata(args: argparse.Namespace) -> dict:
    if args.metadata:
        return json.loads(Path(args.metadata).read_text(encoding="utf-8"))
    cmd = shlex.split(args.cargo) if args.cargo else ["cargo"]
    if args.toolchain:
        cmd.append(f"+{args.toolchain}")
    cmd += ["metadata", "--locked", "--offline", "--format-version", "1", "--filter-platform", args.target,
            "--manifest-path", args.manifest_path]
    out = subprocess.run(cmd, check=True, stdout=subprocess.PIPE)
    return json.loads(out.stdout)


def closure(meta: dict, roots: list[str]) -> tuple[list[dict], set[str]]:
    """Packages reachable from the root packages (by name, workspace members only)."""
    packages = {p["id"]: p for p in meta["packages"]}
    members = set(meta["workspace_members"])
    nodes = {n["id"]: n for n in (meta.get("resolve") or {}).get("nodes") or []}
    if not nodes:
        raise SystemExit("cargo metadata has no resolve graph")
    start = []
    for name in roots:
        ids = [i for i in members if packages[i]["name"] == name]
        if len(ids) != 1:
            raise SystemExit(f"--package {name}: {len(ids)} workspace members have that name")
        start.append(ids[0])
    seen: set[str] = set()
    todo = list(start)
    while todo:
        pid = todo.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for dep in nodes[pid].get("deps") or []:
            kinds = dep.get("dep_kinds") or [{"kind": None}]
            if any(k.get("kind") in (None, "normal", "build") for k in kinds):
                todo.append(dep["pkg"])
    third = [packages[i] for i in seen if i not in members]
    return sorted(third, key=lambda p: (p["name"], version_key(p["version"]), p.get("source") or "")), members


def version_key(v: str) -> tuple:
    core = re.split(r"[-+]", v, maxsplit=1)[0]
    return tuple(int(x) if x.isdigit() else 0 for x in core.split(".")) + (v,)


def normalise(text: str) -> str:
    lines = text.replace("\r\n", "\n").replace("\r", "\n").split("\n")
    return "\n".join(line.rstrip() for line in lines).strip("\n")


_FILES: dict[str, list[tuple[str, str]]] = {}


def license_files(pkg: dict) -> list[tuple[str, str]]:
    """(relative name, normalised text) of a package's license / notice files, sorted by name."""
    if pkg["id"] not in _FILES:
        _FILES[pkg["id"]] = _license_files(pkg)
    return _FILES[pkg["id"]]


def _license_files(pkg: dict) -> list[tuple[str, str]]:
    root = Path(pkg["manifest_path"]).parent
    if not root.is_dir():
        raise SystemExit(f"{pkg['name']} {pkg['version']}: source not downloaded (run after the build or `cargo fetch`)")
    found: dict[str, Path] = {}
    for entry in sorted(root.iterdir(), key=lambda p: p.name):
        if entry.is_file() and not entry.is_symlink() and LICENSE_NAME.match(entry.name) and not NOT_A_TEXT.search(entry.name):
            found[entry.name] = entry
        elif entry.is_dir() and not entry.is_symlink() and not entry.name.startswith("."):
            # LICENSES/: every file; any other directory one level down: the license files of bundled
            # sources (e.g. a -sys crate's vendored C library: quickjs/LICENSE).
            every = entry.name in LICENSE_DIRS
            for sub in sorted(entry.iterdir(), key=lambda p: p.name):
                if sub.is_file() and not sub.is_symlink() and (every or (LICENSE_NAME.match(sub.name) and not NOT_A_TEXT.search(sub.name))):
                    found[f"{entry.name}/{sub.name}"] = sub
    declared = pkg.get("license_file")
    if declared:
        path = (root / declared)
        try:
            rel = path.resolve().relative_to(root.resolve()).as_posix()
        except ValueError:
            rel = None
        if rel and path.is_file() and rel not in found:
            found[rel] = path
    out = []
    for rel in sorted(found):
        data = found[rel].read_bytes()
        if len(data) > MAX_TEXT or b"\0" in data:
            continue
        text = normalise(data.decode("utf-8", errors="replace"))
        if text:
            out.append((rel, text))
    return out


def fence(text: str) -> str:
    longest = max((len(m) for m in re.findall(r"`+", text)), default=0)
    return "`" * max(3, longest + 1)


def cell(text: str) -> str:
    return text.replace("|", "\\|").replace("\n", " ")


def label(pkg: dict) -> str:
    source = pkg.get("source") or ""
    if source.startswith("registry+") or source.startswith("sparse+"):
        return f"{pkg['name']} {pkg['version']}"
    if source.startswith("git+"):
        return f"{pkg['name']} {pkg['version']} (git)"
    return f"{pkg['name']} {pkg['version']} (path)"


def render(pkgs: list[dict], title: str, intro: str, target: str, roots: list[str]) -> str:
    texts: dict[str, list[str]] = {}
    files_of: dict[str, list[str]] = {}
    order: list[str] = []
    missing: list[dict] = []
    for pkg in pkgs:
        files = license_files(pkg)
        if not files:
            missing.append(pkg)
        for rel, text in files:
            if text not in texts:
                texts[text] = []
                files_of[text] = []
                order.append(text)
            texts[text].append(label(pkg))
            if rel not in files_of[text]:
                files_of[text].append(rel)
    out = [f"# {title}", ""]
    if intro:
        out += [intro.strip(), ""]
    out += [
        f"Generated from `Cargo.lock` for `{target}` ({', '.join(f'`{r}`' for r in roots)} and every crate they",
        f"depend on, dev dependencies excluded): {len(pkgs)} third-party crates. Each crate's license is the SPDX",
        "expression of its manifest; the texts below are the license and notice files each crate's published",
        "source ships, each distinct text once.",
        "",
        "| Crate | Version | License |",
        "|---|---|---|",
    ]
    for pkg in pkgs:
        version = label(pkg)[len(pkg["name"]) + 1:]
        out.append(f"| {cell(pkg['name'])} | {cell(version)} | {cell(pkg.get('license') or 'see license file')} |")
    out.append("")
    if missing:
        out += [
            "## Crates without a license file in their published source",
            "",
            "Their license is the SPDX expression above; the full text is in the crate's repository.",
            "",
        ]
        for pkg in missing:
            repo = pkg.get("repository") or pkg.get("homepage") or "no repository given"
            out.append(f"- {label(pkg)}: {pkg.get('license') or 'no license expression'} ({repo})")
        out.append("")
    out += ["## License and notice texts", ""]
    for n, text in enumerate(order, 1):
        users = texts[text]
        out += [f"### {n}. {', '.join(files_of[text])}", "", f"Used by: {', '.join(users)}.", ""]
        f = fence(text)
        out += [f"{f}text", text, f, ""]
    return "\n".join(out).rstrip("\n") + "\n"


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--manifest-path", required=True)
    ap.add_argument("--target", required=True)
    ap.add_argument("--package", action="append", required=True, help="a root workspace package (repeatable)")
    ap.add_argument("--title", default="Third-party notices")
    ap.add_argument("--intro", default="")
    ap.add_argument("--cargo", default="", help="the cargo command (default: cargo)")
    ap.add_argument("--toolchain", default="", help="rustup toolchain (cargo +NAME)")
    ap.add_argument("--metadata", default="", help="a saved `cargo metadata --format-version 1` output")
    ap.add_argument("--out", required=True)
    args = ap.parse_args(argv)
    meta = cargo_metadata(args)
    pkgs, _ = closure(meta, args.package)
    text = render(pkgs, args.title, args.intro, args.target, args.package)
    Path(args.out).write_bytes(text.encode("utf-8"))
    missing = sum(1 for p in pkgs if not license_files(p))
    print(f"third-party notices: {len(pkgs)} crates, {missing} without a license file -> {args.out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
