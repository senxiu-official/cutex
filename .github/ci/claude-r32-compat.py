#!/usr/bin/env python3
"""Restore the immutable Linux claude-r32 archive's exact historical bytes.

r32's baseline was produced with Windows-generated CRLF metadata, an MSYS
execute bit on connect_tui.py and GNU tar 1.35's NUL device fields. Linux CI
instead generated LF/0644 and tar 1.34's ASCII-zero device fields. This tool
exists only for that fixed r32 baseline and the public 0.2 source commit.
r33 should use a clean, explicitly LF recipe under its own runtime identity.

Usage: python claude-r32-compat.py OUT_DIR --source-commit SHA
The target SHA is immutable: no argument or environment override is supported.
Requires Python 3.11+ and GNU gzip on PATH.
"""
from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile

SOURCE_COMMIT = "9f73365dd88e3ef4d9e145972f72ea9a28c59d47"
EXPECTED_SHA256 = "c210c9fc5a9f87b48b5dd2ec87cb2ac7777863b34288d1febb90ca8246c5dde2"
ARCHIVE = "component-claude-runtime-claude-r32-linux-x64.tar.gz"
BUILD_INFO = "BUILD-INFO-component-claude-runtime-linux-x64.json"
SUMS = "SHA256SUMS-component-claude-runtime-linux-x64"
CRLF_FILES = frozenset({"PYTHON-THIRD-PARTY-NOTICES.md", "claude-cli-runtime.json",
                        "python-runtime.json", "requirements.lock"})
EXECUTABLE = "scripts/connect_tui.py"
MAX_ENTRIES = 20_000
MAX_TOTAL_BYTES = 4 << 30
MAX_METADATA_BYTES = 1 << 20
MAX_TRAILER_BYTES = 64 << 10
BLOCK = 512


class CompatError(ValueError):
    pass


def sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def regular_file(path: Path) -> None:
    if path.is_symlink() or not stat.S_ISREG(path.stat().st_mode):
        raise CompatError(f"expected a regular file: {path.name}")


def unique_object(pairs: list[tuple[str, object]]) -> dict:
    value = {}
    for key, item in pairs:
        if key in value:
            raise CompatError("duplicate BUILD-INFO key")
        value[key] = item
    return value


def validate_info(path: Path, source_commit: str) -> None:
    if not re.fullmatch(r"[0-9a-f]{40}", source_commit) or source_commit != SOURCE_COMMIT:
        raise CompatError("this r32 compatibility recipe requires the fixed public 0.2 source commit")
    regular_file(path)
    if path.stat().st_size > MAX_METADATA_BYTES:
        raise CompatError("BUILD-INFO is too large")
    info = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=unique_object)
    expected = {"name": "claude-runtime", "version": "claude-r32", "platform": "linux-x64",
                "commit": source_commit, "bundledClaudeCli": True}
    if (not isinstance(info, dict) or any(info.get(key) != value for key, value in expected.items()) or
            info.get("bundledClaudeCli") is not True):
        raise CompatError("BUILD-INFO must identify bundled-CLI claude-runtime/claude-r32/linux-x64 at the supplied source commit")


def octal(field: bytes) -> int:
    text = field.strip(b"\0 ")
    if text and any(byte not in b"01234567" for byte in text):
        raise CompatError("invalid tar numeric field")
    return int(text or b"0", 8)


def cstring(field: bytes) -> str:
    return field.split(b"\0", 1)[0].decode("utf-8", errors="strict")


def read_exact(stream, size: int) -> bytes:
    data = stream.read(size)
    if len(data) != size:
        raise CompatError("truncated tar archive")
    return data


def canonical_tar(archive: Path, target: Path) -> dict:
    """Preserve ordering/trailer/data except the four explicitly named text files."""
    seen = set()
    total = 0
    with gzip.open(archive, "rb") as source, target.open("xb") as output:
        while True:
            header = bytearray(read_exact(source, BLOCK))
            if not any(header):
                remainder = source.read(MAX_TRAILER_BYTES + 1)
                trailer = bytes(header) + remainder
                if (len(remainder) > MAX_TRAILER_BYTES or len(trailer) < 2 * BLOCK or
                        len(trailer) % BLOCK or any(trailer)):
                    raise CompatError("invalid or excessive tar trailer")
                output.write(trailer)
                break
            stored_checksum = octal(header[148:156])
            header[148:156] = b" " * 8
            if sum(header) != stored_checksum:
                raise CompatError("tar header checksum mismatch")
            if header[257:265] != b"ustar\x0000" or header[156:157] not in (b"0", b"\0"):
                raise CompatError("r32 requires USTAR regular files; links, PAX, GNU extensions and special entries are forbidden")
            if any(header[157:257]):
                raise CompatError("regular-file tar entry has a link target")
            name = cstring(header[:100])
            prefix = cstring(header[345:500])
            name = f"{prefix}/{name}" if prefix else name
            if (not name or name.startswith("/") or any(c in name for c in ("\\", ":", "\0", "\r", "\n")) or
                    any(part in ("", ".", "..") for part in name.split("/")) or name in seen):
                raise CompatError("unsafe or duplicate tar path")
            seen.add(name)
            octal(header[100:108])
            size = octal(header[124:136])
            total += size
            if len(seen) > MAX_ENTRIES or total > MAX_TOTAL_BYTES:
                raise CompatError("archive exceeds r267 entry/byte budgets")
            if octal(header[108:116]) or octal(header[116:124]) or octal(header[136:148]) != 1735689600:
                raise CompatError("r32 requires owner 0:0 and its fixed file mtime")
            if octal(header[329:337]) or octal(header[337:345]):
                raise CompatError("regular-file tar entry has nonzero device numbers")
            data = None
            if name in CRLF_FILES:
                if size > MAX_METADATA_BYTES:
                    raise CompatError("r32 metadata file is too large")
                data = read_exact(source, size)
                data.decode("utf-8", errors="strict")
                data = data.replace(b"\r\n", b"\n")
                if b"\r" in data or b"\0" in data:
                    raise CompatError("unexpected metadata text encoding")
                data = data.replace(b"\n", b"\r\n")
                total += len(data) - size
                if total > MAX_TOTAL_BYTES:
                    raise CompatError("canonical archive exceeds r267 byte budget")
                header[124:136] = f"{len(data):011o}\0".encode("ascii")
            header[100:108] = b"0000755\0" if name == EXECUTABLE else b"0000644\0"
            header[329:345] = bytes(16)
            header[148:156] = f"{sum(header):06o}\0 ".encode("ascii")
            output.write(header)
            if data is None:
                remaining = size
                while remaining:
                    block = read_exact(source, min(remaining, 1 << 20))
                    output.write(block)
                    remaining -= len(block)
            else:
                output.write(data)
            padding = read_exact(source, (-size) % BLOCK)
            if any(padding):
                raise CompatError("nonzero tar data padding")
            output.write(bytes((-(len(data) if data is not None else size)) % BLOCK))
    if not CRLF_FILES.issubset(seen) or EXECUTABLE not in seen:
        raise CompatError("archive lacks required r32 metadata or connect_tui.py")
    return {"entries": len(seen), "fileBytes": total}


def compress_gnu(source: Path, target: Path) -> None:
    binary = shutil.which("gzip")
    if binary is None:
        raise CompatError("GNU gzip is required")
    version = subprocess.run([binary, "--version"], capture_output=True, text=True, check=True)
    if "Free Software Foundation" not in version.stdout:
        raise CompatError("GNU gzip is required")
    with source.open("rb") as raw, target.open("xb") as output:
        subprocess.run([binary, "-n", "-9"], stdin=raw, stdout=output, check=True)


def apply_compat(out: Path, source_commit: str) -> dict:
    out = out.resolve(strict=True)
    info, archive, sums = out / BUILD_INFO, out / ARCHIVE, out / SUMS
    validate_info(info, source_commit)
    regular_file(archive)
    if archive.stat().st_size > MAX_TOTAL_BYTES:
        raise CompatError("compressed archive exceeds the byte budget")
    if sums.exists() or sums.is_symlink():
        regular_file(sums)
    old_hash = sha256(archive)
    sum_bytes = f"{EXPECTED_SHA256}  {ARCHIVE}\n".encode("ascii")
    if old_hash == EXPECTED_SHA256 and sums.is_file() and sums.read_bytes() == sum_bytes:
        return {"status": "already-compatible", "sha256": old_hash, "sourceCommit": source_commit}
    with tempfile.TemporaryDirectory(prefix=".claude-r32-compat-", dir=out) as directory:
        work = Path(directory)
        prepared_sums = work / "sums"
        prepared_sums.write_bytes(sum_bytes)
        prepared_sums.chmod(stat.S_IMODE(sums.stat().st_mode) if sums.exists() else 0o644)
        changed_archive = old_hash != EXPECTED_SHA256
        details = {}
        if changed_archive:
            details = canonical_tar(archive, work / "canonical.tar")
            compress_gnu(work / "canonical.tar", work / "verified.tar.gz")
            if sha256(work / "verified.tar.gz") != EXPECTED_SHA256:
                raise CompatError("canonical archive does not match the immutable claude-r32 baseline; original is untouched")
            # Retain a private copy for rollback if the subsequent sums replacement fails.
            shutil.copy2(archive, work / "original.tar.gz")
            (work / "verified.tar.gz").chmod(stat.S_IMODE(archive.stat().st_mode))
            if sha256(archive) != old_hash:
                raise CompatError("input archive changed during compatibility preparation")
            os.replace(work / "verified.tar.gz", archive)
        try:
            os.replace(prepared_sums, sums)
        except BaseException:
            if changed_archive:
                os.replace(work / "original.tar.gz", archive)
            raise
    return {"status": "restored" if changed_archive else "sums-repaired", "sha256": EXPECTED_SHA256,
            "sourceCommit": source_commit, **details}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("out", type=Path)
    parser.add_argument("--source-commit", required=True)
    args = parser.parse_args(argv)
    try:
        print(json.dumps(apply_compat(args.out, args.source_commit), sort_keys=True))
        return 0
    except (OSError, ValueError, EOFError, subprocess.SubprocessError) as error:
        parser.exit(1, f"claude-r32-compat: {error}\n")


if __name__ == "__main__":
    raise SystemExit(main())
