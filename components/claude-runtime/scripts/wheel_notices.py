"""THIRD_PARTY_NOTICES.md for the claude-runtime archive: the vendored wheels with their license and
the text of the license / notice files each wheel carries in its .dist-info.

    python scripts/wheel_notices.py --wheels DIR/wheels --out DIR/THIRD_PARTY_NOTICES.md

Used by build.sh. One entry per distribution (name + version; the per-Python wheels of a package
carry the same files), identical texts printed once. Deterministic (sorted, no dates, no paths), so
the same wheels give the same bytes. Standard library only.
"""

from __future__ import annotations

import argparse
import email.parser
import re
import sys
import zipfile
from pathlib import Path

LICENSE_NAME = re.compile(r"^(licen[cs]e|copying|notice|copyright|authors|patents)([._-].*)?$", re.I)


def normalise(text: str) -> str:
    lines = text.replace("\r\n", "\n").replace("\r", "\n").split("\n")
    return "\n".join(line.rstrip() for line in lines).strip("\n")


def license_of(meta) -> str:
    expr = (meta.get("License-Expression") or "").strip()
    if expr:
        return expr
    lic = (meta.get("License") or "").strip()
    if lic and "\n" not in lic and len(lic) <= 60:
        return lic
    classifiers = [c.split("::")[-1].strip() for c in meta.get_all("Classifier") or [] if c.startswith("License ::")]
    return " / ".join(classifiers) or "see license file"


def read_wheel(path: Path):
    with zipfile.ZipFile(path) as z:
        names = sorted(z.namelist())
        info = [n for n in names if re.fullmatch(r"[^/]+\.dist-info/METADATA", n)]
        if len(info) != 1:
            raise SystemExit(f"{path.name}: no single .dist-info/METADATA")
        dist_info = info[0].split("/")[0]
        meta = email.parser.Parser().parsestr(z.read(info[0]).decode("utf-8", errors="replace"))
        files = []
        for n in names:
            parts = n.split("/")
            if parts[0] != dist_info or n.endswith("/"):
                continue
            if (len(parts) >= 3 and parts[1] == "licenses") or (len(parts) == 2 and LICENSE_NAME.match(parts[1])):
                text = normalise(z.read(n).decode("utf-8", errors="replace"))
                if text:
                    files.append(("/".join(parts[1:]), text))
    return meta["Name"], meta["Version"], license_of(meta), files


def fence(text: str) -> str:
    longest = max((len(m) for m in re.findall(r"`+", text)), default=0)
    return "`" * max(3, longest + 1)


def render(wheels: Path) -> str:
    dists = {}
    for path in sorted(wheels.glob("*.whl")):
        name, version, lic, files = read_wheel(path)
        dists.setdefault((name.lower(), version), (name, version, lic, files))
    if not dists:
        raise SystemExit(f"{wheels}: no wheels")
    texts: dict[str, list[str]] = {}
    origin: dict[str, list[str]] = {}
    out = [
        "# claude-runtime: third-party notices",
        "",
        "The claude-runtime component is licensed under the MIT License (LICENSE). It installs the Python",
        f"packages below from the wheels in `wheels/` ({len(dists)} packages), each redistributed unmodified",
        "under its own license; each wheel also carries these files in its `.dist-info`. claude-agent-sdk",
        "is built from its source distribution without the bundled Claude Code CLI (not redistributed).",
        "",
        "| Package | Version | License |",
        "|---|---|---|",
    ]
    for key in sorted(dists):
        name, version, lic, files = dists[key]
        out.append(f"| {name} | {version} | {lic.replace('|', '/')} |")
        for rel, text in files:
            texts.setdefault(text, []).append(f"{name} {version}")
            origin.setdefault(text, [])
            if rel not in origin[text]:
                origin[text].append(rel)
    missing = [f"{d[0]} {d[1]}" for _, d in sorted(dists.items()) if not d[3]]
    out.append("")
    if missing:
        out += ["Without a license file in the wheel: " + ", ".join(missing) + ".", ""]
    out += ["## License and notice texts", ""]
    for n, text in enumerate(texts, 1):
        f = fence(text)
        out += [f"### {n}. {', '.join(origin[text])}", "", f"Used by: {', '.join(texts[text])}.", "", f"{f}text", text, f, ""]
    return "\n".join(out).rstrip("\n") + "\n"


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--wheels", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args(argv)
    Path(args.out).write_bytes(render(Path(args.wheels)).encode("utf-8"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
