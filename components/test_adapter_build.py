"""Source-only adapter acceptance checks; no vendor packages, services or network."""
from pathlib import Path
import gzip
import hashlib
import importlib.util
import io
import json
import subprocess
import tarfile
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("adapter_build", Path(__file__).with_name("adapter_build.py"))
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)


class AdapterBuildTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="source-adapter-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q")
        self.git("config", "core.autocrlf", "false")
        self.git("config", "core.symlinks", "false")
        self.source = self.root / "components/dsh-runtime"
        self.source.mkdir(parents=True)
        self.write("component.json", dict(schema="cutex/component/v1", name="dsh-runtime", version="dsh-r3", platforms=list(builder.PLATFORMS)))
        self.write("external-runtime.json", dict(schema="cutex/external-runtime/v1", name="dsh-runtime", version="dsh-r3", payload="adapter-v1"))
        self.write("package.json", dict(name="adapter-fixture", private=True, type="module", scripts={"test": "unused"}, devDependencies={"ws": "8.22.0"}))
        for path, data in [("README.md", b"Install upstream separately.\n"), ("LICENSE", b"Apache-2.0\n"), ("NOTICE", b"Test fixture\n"), ("src/server.mjs", b"export const adapter = true;\n")]:
            self.write(path, data)
        # Even tracked or executable vendor/tool payloads must never enter the allowlist.
        for path in ["runtime/node", "runtime/node_modules/vendor/cli.js", "node_modules/ws/index.js", "test/fixture.mjs", "build.sh"]:
            self.write(path, b"must not ship\n")
        self.commit()

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.root), *args], stderr=subprocess.PIPE)

    def write(self, relative, data):
        path = self.source / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes((json.dumps(data, indent=2) + "\n").encode() if isinstance(data, dict) else data)

    def commit(self):
        self.git("add", ".")
        self.git("-c", "user.name=Adapter test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-qm", "fixture")

    def build(self, output="out", platform="linux-x64", version="dsh-r3"):
        return builder.build(self.root, "dsh-runtime", platform, version, self.root / output)

    def archive(self, output="out", platform="linux-x64"):
        return self.root / output / f"component-dsh-runtime-dsh-r3-{platform}.tar.gz"

    def test_repeated_builds_and_platforms_have_identical_payload_bytes(self):
        first = self.build()
        self.build("again")
        windows = self.build("windows", "windows-x64")
        self.assertEqual(self.archive().read_bytes(), self.archive("again").read_bytes())
        self.assertEqual(first["sha256"], windows["sha256"])
        self.assertEqual(first["sha256"], hashlib.sha256(self.archive().read_bytes()).hexdigest())
        self.assertEqual(first["commit"], self.git("rev-parse", "HEAD").decode().strip())

    def test_flat_allowlist_and_normalized_ustar_gzip_metadata(self):
        self.build()
        data = self.archive().read_bytes()
        self.assertEqual(data[3], 0)
        self.assertEqual(data[4:8], bytes(4))
        self.assertEqual(gzip.decompress(data)[257:265], b"ustar\x0000")
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as tar:
            self.assertEqual(tar.getnames(), sorted([*builder.ROOT_FILES, "src/server.mjs"]))
            for entry in tar:
                self.assertTrue(entry.isfile())
                self.assertEqual((entry.uid, entry.gid, entry.mtime, entry.mode), (0, 0, builder.EPOCH, 0o644))
                self.assertFalse(entry.pax_headers)
            package = json.load(tar.extractfile("package.json"))
            self.assertEqual(package, dict(name="adapter-fixture", private=True, type="module"))

    def test_cannot_reuse_bundled_r2_identity_or_mismatch_metadata(self):
        with self.assertRaisesRegex(builder.BuildError, "r3-or-later"):
            self.build(version="dsh-r2")
        with self.assertRaisesRegex(builder.BuildError, "metadata"):
            self.build(version="dsh-r4")

    def test_dirty_input_rejected_and_untracked_files_never_included(self):
        self.write("src/server.mjs", b"changed\n")
        with self.assertRaisesRegex(builder.BuildError, "local changes"):
            self.build()
        self.git("checkout", "--", "components/dsh-runtime/src/server.mjs")
        self.write("src/not-committed.mjs", b"not allowed\n")
        self.build()
        with tarfile.open(self.archive()) as tar:
            self.assertNotIn("src/not-committed.mjs", tar.getnames())

    def test_committed_crlf_is_rejected(self):
        self.write("src/server.mjs", b"bad\r\n")
        self.commit()
        with self.assertRaisesRegex(builder.BuildError, "LF"):
            self.build()

    def test_documentation_commit_does_not_change_archive_identity(self):
        first = self.build()
        self.write("README.md", b"Updated configure/API guidance.\n")
        self.commit()
        second = self.build("documentation-update")
        self.assertNotEqual(first["commit"], second["commit"])
        self.assertEqual(first["sha256"], second["sha256"])
        with tarfile.open(self.archive()) as tar:
            self.assertNotIn("README.md", tar.getnames())

    def test_symlink_git_mode_is_rejected_even_on_windows(self):
        self.write("src/link.mjs", b"server.mjs")
        oid = self.git("hash-object", "-w", "components/dsh-runtime/src/link.mjs").decode().strip()
        self.git("update-index", "--add", "--cacheinfo", f"120000,{oid},components/dsh-runtime/src/link.mjs")
        self.git("-c", "user.name=Adapter test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-qm", "link fixture")
        with self.assertRaisesRegex(builder.BuildError, "regular file"):
            self.build()

    def test_output_collisions_preserve_existing_and_unrelated_files(self):
        out = self.root / "out"
        out.mkdir()
        sentinel = out / "unrelated-component.tar.gz"
        sentinel.write_bytes(b"keep")
        sums = out / "SHA256SUMS-component-dsh-runtime-linux-x64"
        sums.write_bytes(b"existing")
        with self.assertRaises(FileExistsError):
            self.build()
        self.assertEqual(sums.read_bytes(), b"existing")
        self.assertEqual(sentinel.read_bytes(), b"keep")
        self.assertFalse(self.archive().exists())


if __name__ == "__main__":
    unittest.main()
