#!/usr/bin/env python3
"""Offline route regressions: use the real transport class, intercept only process execution."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("signer", Path(__file__).with_name("sign-component-kit.py"))
signer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(signer)


class RouteTests(unittest.TestCase):
    def test_repository_root_exact_route(self):
        with patch.object(signer, "run", return_value=b'{"default_branch":"release"}') as request:
            self.assertEqual(signer.GitHub().api("")["default_branch"], "release")
        request.assert_called_once_with(["gh", "api", "repos/senxiu-official/cutex"])

    def test_release_exact_route(self):
        with patch.object(signer, "run", return_value=b'{"id":123}') as request:
            self.assertEqual(signer.GitHub().release(123)["id"], 123)
        request.assert_called_once_with(["gh", "api", "repos/senxiu-official/cutex/releases/123"])

    def test_branch_context_exact_routes(self):
        commit = "a" * 40
        requests = []
        replies = [
            (["gh", "api", "repos/senxiu-official/cutex"], {"default_branch": "release"}),
            (["gh", "api", "repos/senxiu-official/cutex/branches/release"], {"commit": {"sha": commit}}),
            (["git", "rev-parse", "HEAD"], commit),
        ]

        def execute(args):
            expected, result = replies[len(requests)]
            self.assertEqual(args, expected)
            requests.append(args)
            return (json.dumps(result) if isinstance(result, dict) else result).encode()

        env = {"GITHUB_REPOSITORY": "senxiu-official/cutex", "GITHUB_EVENT_NAME": "workflow_dispatch",
               "GITHUB_SHA": commit, "GITHUB_REF": "refs/heads/release", "GITHUB_RUN_ID": "123"}
        with patch.dict(os.environ, env), patch.object(signer, "run", side_effect=execute):
            self.assertEqual(signer.context(signer.GitHub())["workflowCommit"], commit)
        self.assertEqual(len(requests), 3)

    def test_asset_download_exact_route_and_headers(self):
        body = b"synthetic input"
        asset = {"id": 456, "size": len(body), "digest": "sha256:" + hashlib.sha256(body).hexdigest()}

        def execute(args, *, output):
            self.assertEqual(args, ["gh", "api", "repos/senxiu-official/cutex/releases/assets/456",
                                    "-H", "Accept: application/octet-stream"])
            output.write(body)

        with tempfile.TemporaryDirectory() as directory, patch.object(signer, "run", side_effect=execute) as request:
            path = Path(directory) / "input.json"
            signer.GitHub().download(asset, path)
            self.assertEqual(path.read_bytes(), body)
            request.assert_called_once()

    def test_upload_exact_argv_without_publish_or_clobber(self):
        paths = [Path("payload.minisig"), Path("signing-receipt.json")]
        with patch.object(signer, "run") as request:
            signer.GitHub().upload("sign-input-example", paths)
        request.assert_called_once_with(["gh", "release", "upload", "sign-input-example",
                                        "--repo", "senxiu-official/cutex", *map(str, paths)])


if __name__ == "__main__":
    unittest.main(verbosity=2)
