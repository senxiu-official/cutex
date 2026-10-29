from __future__ import annotations

import importlib.util
import io
import tarfile
from pathlib import Path

import pytest


SCRIPT = Path(__file__).parents[1] / "scripts" / "vendor_python.py"
SPEC = importlib.util.spec_from_file_location("vendor_python", SCRIPT)
assert SPEC and SPEC.loader
vendor_python = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(vendor_python)


def test_stage_keeps_payload_and_exposes_cpython_license(tmp_path: Path) -> None:
    source = tmp_path / "source"
    (source / "python/lib/python3.13").mkdir(parents=True)
    (source / "python/lib/python3.13/LICENSE.txt").write_text("PSF fixture", encoding="utf-8")
    archive = tmp_path / "python.tar.gz"
    with tarfile.open(archive, "w:gz") as bundle:
        bundle.add(source / "python", arcname="python")
    out = tmp_path / "out"
    out.mkdir()
    vendor_python.stage_payload(archive, out, {"license": "python/lib/python3.13/LICENSE.txt"})
    assert (out / vendor_python.PAYLOAD).read_bytes() == archive.read_bytes()
    assert (out / vendor_python.LICENSE).read_text(encoding="utf-8") == "PSF fixture"


def test_verify_checks_size_and_hash(tmp_path: Path) -> None:
    archive = tmp_path / "asset"
    archive.write_bytes(b"known")
    asset = {"size": 5, "sha256": vendor_python.sha256(archive)}
    vendor_python.verify(archive, asset)
    with pytest.raises(ValueError, match="size"):
        vendor_python.verify(archive, {**asset, "size": 6})
    with pytest.raises(ValueError, match="sha256"):
        vendor_python.verify(archive, {**asset, "sha256": "0" * 64})


def test_download_copy_is_capped_at_pinned_size() -> None:
    out = io.BytesIO()
    vendor_python.copy_limited(io.BytesIO(b"known"), out, 5)
    assert out.getvalue() == b"known"
    with pytest.raises(ValueError, match="exceeds"):
        vendor_python.copy_limited(io.BytesIO(b"too large"), io.BytesIO(), 3)
