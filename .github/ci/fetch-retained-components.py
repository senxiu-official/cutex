"""Stage the original signed component packages for the 0.2.6 release manifest."""

import hashlib
import json
from pathlib import Path
import subprocess
import sys


PACKAGES = [
    ("job-service", "0.1.3", ("linux-x64",), "92777d49ef530717663595c6ffb7a4cdb104b8fb",
     "e763caa91b471f67fd033afa798483e333311fec052300f62eff41a6377afb35", 732138,
     ("4efc59f5795f71832ea7d9ddd89fd260a13aa5fbc75ab0aadf9f921dd3162b13",)),
    ("dsh-runtime", "dsh-r3", ("linux-x64", "windows-x64"), "09022d2d33bc8e02c09e00210c77a5765b6b624b",
     "8744fcad5b964c71bd8a96b43397b06c93974464874c173f311a08118182a950", 68820,
     ("24a9b0948cce9323bf6e931cc83b147de6ccd205afb99ecec54659048f50430b",
      "55ac739df5d4b1f5e4914cc1be568c482c1d585415da548ed1c6afd4cf98513e")),
    ("kimi-runtime", "kimi-r3", ("linux-x64", "windows-x64"), "09022d2d33bc8e02c09e00210c77a5765b6b624b",
     "0729fc1410b039a182c751229c40d54b29f2a62bfe13c632bd490a61388652a5", 45835,
     ("7473ed634a425eac8585f70e4710d0a8ec92929a73b66cb62a05b333b839f9d2",
      "3b974db48d7858136cf7f735e43f1d78d7dc8c21ef4e018ca5684db4871e1aa6")),
]


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def fetch(output, key):
    output.mkdir()
    for name, version, platforms, release_commit, digest, size, signatures in PACKAGES:
        tag = f"component-{name}-{version}"
        for platform, signature_digest in zip(platforms, signatures, strict=True):
            dest = output / f"{name}-{platform}"
            dest.mkdir()
            asset = dest / f"{tag}-{platform}.tar.gz"
            signature = asset.with_suffix(".gz.minisig")
            for path in (asset, signature):
                subprocess.run(
                    ["gh", "release", "download", tag, "-R", "senxiu-official/cutex",
                     "-p", path.name, "-D", str(dest)],
                    check=True,
                )
            if asset.stat().st_size != size or sha256(asset) != digest or sha256(signature) != signature_digest:
                raise ValueError(f"{asset.name} or its signature differs from the original publication")
            subprocess.run(
                ["minisign", "-V", "-q", "-p", str(key), "-m", str(asset), "-x", str(signature)],
                check=True,
            )
            stem = f"component-{name}-{platform}"
            (dest / f"BUILD-INFO-{stem}.json").write_text(json.dumps({
                "name": name, "version": version, "platform": platform, "commit": release_commit,
                "provenance": "original published archive and signature",
            }) + "\n", encoding="utf-8")
            (dest / f"SHA256SUMS-{stem}").write_text(f"{digest}  {asset.name}\n", encoding="utf-8")


if __name__ == "__main__":
    fetch(Path(sys.argv[1]), Path(sys.argv[2]))
