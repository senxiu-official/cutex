#!/usr/bin/env python3
"""Private component-kit signing transport. Never publishes or edits a release.

Only prepare/store run here; the workflow performs the existing minisign signing
step between them. Payloads and sidecars stay in one unpublished staging draft.
"""
from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile

REPO = "senxiu-official/cutex"
PUBLIC_KEY = "RWRH9INhAb2gACJAg/h5EBjbudos2LXj6K+5hRBEGeVLDKaKzE+FICmS"
KEY_ID = "00A0BD016183F447"
MANIFEST_NAME = "SIGNING-INPUTS.json"
RECEIPT_NAME = "signing-receipt.json"
LABEL = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,63}")
HASH = re.compile(r"[0-9a-f]{64}")
COMMIT = re.compile(r"[0-9a-f]{40}")
MAX_JSON = 1 << 20
MAX_ARCHIVE = 16 << 20
MAX_EXPANDED = 256 << 20
MAX_ENTRIES = 20_000


class KitError(ValueError):
    pass


def need(ok, message):
    if not ok:
        raise KitError(message)


def sha(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def obj(value, keys):
    need(isinstance(value, dict) and set(value) == set(keys), "unexpected document fields")
    return value


def label(value):
    need(isinstance(value, str) and LABEL.fullmatch(value) and value not in (".", ".."), "unsafe label")
    return value


def integer(value, maximum=(1 << 53) - 1):
    need(type(value) is int and 1 <= value <= maximum, "invalid positive integer")
    return value


def digest(value):
    need(isinstance(value, str) and HASH.fullmatch(value), "invalid SHA256")
    return value


def pairs(items):
    result = {}
    for key, value in items:
        need(key not in result, "duplicate JSON key")
        result[key] = value
    return result


def decode(data: bytes, *, lf=False):
    need(len(data) <= MAX_JSON, "JSON exceeds signing budget")
    need(not lf or b"\r" not in data, "signing metadata must contain exact LF bytes")
    return json.loads(data.decode("utf-8"), object_pairs_hook=pairs,
                      parse_constant=lambda _: (_ for _ in ()).throw(KitError("non-finite JSON value")))


def regular(path: Path):
    need(path.is_file() and not path.is_symlink(), "expected regular input file")


def public_key(path: Path):
    lines = path.read_text(encoding="utf-8").splitlines()
    need(len(lines) == 2 and lines[1] == PUBLIC_KEY, "release public key mismatch")


def run(args, *, output=None):
    env = dict(os.environ)
    env.pop("GH_DEBUG", None)
    result = subprocess.run(args, stdout=output if output is not None else subprocess.PIPE,
                            stderr=subprocess.PIPE, env=env)
    need(result.returncode == 0, "authenticated transport or signature operation failed")
    return result.stdout


class GitHub:
    def api(self, endpoint):
        route = f"repos/{REPO}" + (f"/{endpoint}" if endpoint else "")
        return decode(run(["gh", "api", route]))

    def release(self, release_id):
        return self.api(f"releases/{release_id}")

    def download(self, asset, path):
        with path.open("xb") as output:
            run(["gh", "api", f"repos/{REPO}/releases/assets/{asset['id']}",
                 "-H", "Accept: application/octet-stream"], output=output)
        need(path.stat().st_size == asset["size"], "downloaded asset size differs")
        need(asset.get("digest") == "sha256:" + sha(path), "downloaded asset digest differs")

    def upload(self, tag, paths):
        # No clobber and no create/edit/publish operation.
        run(["gh", "release", "upload", tag, "--repo", REPO, *map(str, paths)])


def context(github):
    need(os.environ.get("GITHUB_REPOSITORY") == REPO, "wrong repository")
    need(os.environ.get("GITHUB_EVENT_NAME") == "workflow_dispatch", "manual dispatch required")
    commit = os.environ.get("GITHUB_SHA", "")
    need(COMMIT.fullmatch(commit), "invalid workflow commit")
    branch = github.api("")["default_branch"]
    need(branch == "release" and os.environ.get("GITHUB_REF") == "refs/heads/" + branch,
         "only the default release branch may sign")
    need(github.api("branches/release")["commit"]["sha"] == commit, "default branch advanced; re-review required")
    need(run(["git", "rev-parse", "HEAD"]).decode().strip() == commit, "checkout/workflow identity differs")
    return {"workflowCommit": commit, "runId": integer(int(os.environ["GITHUB_RUN_ID"])),
            "repository": REPO}


def staged(release, release_id, ctx):
    need(release.get("id") == release_id and release.get("draft") is True, "staging release must remain a draft")
    tag = release.get("tag_name", "")
    need(isinstance(tag, str) and re.fullmatch(r"sign-input-[A-Za-z0-9][A-Za-z0-9._-]{0,63}", tag),
         "not an isolated signing staging tag")
    need(release.get("target_commitish") == ctx["workflowCommit"], "draft must target reviewed CI commit")
    return tag


def assets(release):
    result = {}
    for item in release.get("assets", []):
        name = item.get("name")
        need(isinstance(name, str) and "/" not in name and "\\" not in name and name not in result,
             "duplicate or unsafe asset name")
        integer(item.get("id"))
        integer(item.get("size"), MAX_ARCHIVE)
        digest(str(item.get("digest", "")).removeprefix("sha256:"))
        need(item.get("state") == "uploaded", "asset upload is incomplete")
        result[name] = item
    return result


def identity(items):
    return sorted((a["id"], a["name"], a["size"], a["digest"], a.get("updated_at")) for a in items.values())


def timestamp(value):
    need(isinstance(value, str) and re.fullmatch(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z", value),
         "invalid UTC timestamp")
    dt.datetime.fromisoformat(value.removesuffix("Z") + "+00:00")


def relative(value):
    need(isinstance(value, str) and value and not any(c in value for c in "\\:\0\r\n"),
         "invalid relative contract path")
    need(all(p not in ("", ".", "..") for p in value.split("/")), "unsafe relative contract path")


def external(contract, name, version):
    obj(contract, ["schema", "name", "version", "payload", "externalRoot", "node", "checks", "environment"])
    need((contract["schema"], contract["name"], contract["version"], contract["payload"]) ==
         ("cutex/external-runtime/v1", name, version, "adapter-v1"), "external contract identity differs")
    root = obj(contract["externalRoot"], ["kind", "packages", "cliEntry"] +
               (["catalogEntry"] if "catalogEntry" in contract["externalRoot"] else []))
    need(root["kind"] == "npm-prefix" and isinstance(root["packages"], dict) and root["packages"],
         "invalid external package contract")
    for package, version_pin in root["packages"].items():
        need(isinstance(package, str) and package and isinstance(version_pin, str) and
             re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", version_pin), "package version must be exact")
    relative(root["cliEntry"])
    if "catalogEntry" in root:
        relative(root["catalogEntry"])
    node = obj(contract["node"], ["range", "recommended", "features"])
    need(all(isinstance(node[k], str) and node[k] for k in ("range", "recommended")), "invalid Node contract")
    need(isinstance(node["features"], list) and len(node["features"]) == len(set(node["features"])) and
         all(x == "node:sqlite" for x in node["features"]), "invalid Node feature")
    checks = obj(contract["checks"], ["rg", "nodePty"])
    need(all(type(x) is bool for x in checks.values()), "invalid dependency check flags")
    environment = obj(contract["environment"], ["installRoot"])
    need(environment["installRoot"] in ("DSH_INSTALL_ROOT", "KIMI_INSTALL_ROOT"), "invalid runtime root variable")


def archive_contract(path, binding):
    required = {}
    total = 0
    seen = set()
    with tarfile.open(path, "r:gz") as archive:
        for member in archive:
            name = member.name
            need(member.isfile() and not member.pax_headers and
                 all(p not in ("", ".", "..") for p in name.split("/")) and
                 not any(c in name for c in "\\:\0\r\n") and name not in seen,
                 "archive contains unsafe, duplicate or non-regular entries")
            need("node_modules" not in name.split("/"), "adapter must not bundle an external npm prefix")
            seen.add(name)
            total += member.size
            need(len(seen) <= MAX_ENTRIES and total <= MAX_EXPANDED, "archive exceeds adapter budget")
            if name in ("component.json", "external-runtime.json"):
                need(member.size <= MAX_JSON, "archive metadata is too large")
                required[name] = archive.extractfile(member).read()
    need(set(required) == {"component.json", "external-runtime.json"}, "archive contract is missing")
    component = obj(decode(required["component.json"]), ["schema", "name", "version", "platforms"])
    need(component["schema"] == "cutex/component/v1" and component["name"] == binding["component"] and
         component["version"] == binding["version"] and isinstance(component["platforms"], list) and
         binding["platform"] in component["platforms"], "archive component identity differs")
    raw = required["external-runtime.json"]
    need(hashlib.sha256(raw).hexdigest() == binding["externalRuntimeSha256"] and
         decode(raw) == binding["externalRuntime"], "archive/index external contract differs")


def validate_bundle(manifest, directory):
    obj(manifest, ["schema", "source", "index", "archives"])
    need(manifest["schema"] == "cutex/component-signing-inputs/v1", "wrong input manifest schema")
    source = obj(manifest["source"], ["productCommit", "indexCommit"])
    need(all(isinstance(x, str) and COMMIT.fullmatch(x) for x in source.values()), "invalid private provenance")
    spec = obj(manifest["index"], ["name", "size", "sha256", "channel", "sequence", "publishedAt"])
    need(spec["name"] == "index.json" and label(spec["channel"]).startswith("private-"),
         "only a private component channel may use this workflow")
    integer(spec["sequence"])
    timestamp(spec["publishedAt"])
    digest(spec["sha256"])
    integer(spec["size"], MAX_JSON)
    index_path = directory / "index.json"
    regular(index_path)
    need(index_path.stat().st_size == spec["size"] and sha(index_path) == spec["sha256"], "index bytes differ")
    index = obj(decode(index_path.read_bytes(), lf=True),
                ["schema", "channel", "sequence", "publishedAt", "components"])
    integer(index["sequence"])
    need(index["schema"] == "cutex/components-index/v1" and
         all(index[k] == spec[k] for k in ("channel", "sequence", "publishedAt")), "index header differs")
    need(isinstance(index["components"], list) and index["components"], "missing components")
    bindings, component_names = {}, set()
    for component in index["components"]:
        obj(component, ["name", "recommended", "releases"])
        name = label(component["name"])
        need(name not in component_names, "duplicate component")
        component_names.add(name)
        need(isinstance(component["releases"], list) and component["releases"], "missing component releases")
        versions = {}
        for release in component["releases"]:
            obj(release, ["version", "payload", "compatible", "assetTag", "platforms",
                          "yanked", "yankReason", "externalRuntime", "externalRuntimeSha256"])
            version = label(release["version"])
            need(version not in versions, "duplicate version")
            versions[version] = release
            need(release["payload"] == "adapter-v1" and type(release["yanked"]) is bool,
                 "only adapter payloads may use this workflow")
            need(release["yankReason"] is None or isinstance(release["yankReason"], str), "invalid withdrawal reason")
            caps = obj(release["compatible"], ["allCapabilities"])["allCapabilities"]
            need(isinstance(caps, list) and all(isinstance(x, str) for x in caps) and
                 len(caps) == len(set(caps)), "invalid capability set")
            for cap in caps:
                label(cap)
            tag = label(release["assetTag"])
            need(tag == f"component-{name}-{version}", "component tag differs")
            external(release["externalRuntime"], name, version)
            digest(release["externalRuntimeSha256"])
            platforms = release["platforms"]
            need(isinstance(platforms, dict) and platforms, "missing platforms")
            for platform, artifact in platforms.items():
                need(platform in ("linux-x64", "windows-x64"), "unsupported adapter platform")
                obj(artifact, ["name", "sha256", "size", "signatureName"])
                filename = artifact["name"]
                need(filename == f"component-{name}-{version}-{platform}.tar.gz" and
                     len(filename) <= 56 and artifact["signatureName"] == filename + ".minisig" and
                     filename not in bindings, "unsafe or duplicate archive identity")
                digest(artifact["sha256"])
                integer(artifact["size"], MAX_ARCHIVE)
                bindings[filename] = {"component": name, "version": version, "platform": platform,
                                      "assetTag": tag, "sha256": artifact["sha256"], "size": artifact["size"],
                                      "externalRuntime": release["externalRuntime"],
                                      "externalRuntimeSha256": release["externalRuntimeSha256"]}
        recommended = component["recommended"]
        need(recommended is None or (isinstance(recommended, str) and recommended in versions and
                                     not versions[recommended]["yanked"]), "invalid recommendation")
    records = manifest["archives"]
    need(isinstance(records, list) and 1 <= len(records) <= 16, "invalid archive count")
    supplied = {}
    for record in records:
        obj(record, ["name", "size", "sha256", "component", "version", "platform", "assetTag"])
        filename = record["name"]
        need(isinstance(filename, str) and filename in bindings and filename not in supplied, "unexpected archive")
        need(all(record[k] == bindings[filename][k] for k in record if k != "name"), "archive binding differs")
        path = directory / filename
        regular(path)
        need(path.stat().st_size == record["size"] and sha(path) == record["sha256"], "archive bytes differ")
        archive_contract(path, bindings[filename])
        supplied[filename] = record
    need(supplied.keys() == bindings.keys(), "index references an unstaged archive")
    return ["index.json", *sorted(supplied)]


def prepare(args, github, ctx):
    public_key(args.public_key)
    release_id = integer(args.release_id)
    digest(args.manifest_sha256)
    release = github.release(release_id)
    tag = staged(release, release_id, ctx)
    items = assets(release)
    need(MANIFEST_NAME in items and items[MANIFEST_NAME]["size"] <= MAX_JSON, "input manifest missing")
    need(not args.work.exists(), "work directory already exists; inspect previous outcome")
    inputs = args.work / "inputs"
    inputs.mkdir(parents=True)
    github.download(items[MANIFEST_NAME], inputs / MANIFEST_NAME)
    need(sha(inputs / MANIFEST_NAME) == args.manifest_sha256, "approved input manifest digest differs")
    manifest = decode((inputs / MANIFEST_NAME).read_bytes(), lf=True)
    # Validate names before they become paths or download requests.
    obj(manifest, ["schema", "source", "index", "archives"])
    need(isinstance(manifest["archives"], list) and 1 <= len(manifest["archives"]) <= 16, "invalid archive count")
    wanted = [record.get("name") for record in manifest["archives"] if isinstance(record, dict)]
    need(len(wanted) == len(manifest["archives"]) and len(set(wanted)) == len(wanted) and
         all(isinstance(n, str) and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,55}\.tar\.gz", n) for n in wanted),
         "invalid archive filenames")
    need(set(items) == {MANIFEST_NAME, "index.json", *wanted}, "draft must contain only the frozen inputs")
    for name in ["index.json", *wanted]:
        github.download(items[name], inputs / name)
    payloads = validate_bundle(manifest, inputs)
    final = github.release(release_id)
    need(staged(final, release_id, ctx) == tag and identity(assets(final)) == identity(items),
         "input draft changed during verification")
    state = {"schema": "cutex/component-signing-stage/v1", "context": ctx, "releaseId": release_id,
             "tag": tag, "manifestSha256": args.manifest_sha256, "inputAssets": items, "payloads": payloads}
    (args.work / "stage.json").write_text(json.dumps(state, indent=2) + "\n", encoding="utf-8")
    (args.work / "payloads.txt").write_text("\n".join(payloads) + "\n", encoding="utf-8")
    print("Private component signing inputs verified; no GitHub write.")


def store(args, github, ctx):
    public_key(args.public_key)
    regular(args.work / "stage.json")
    state = decode((args.work / "stage.json").read_bytes())
    need(state["context"] == ctx, "signing context changed")
    release_id = integer(state["releaseId"])
    current = github.release(release_id)
    need(staged(current, release_id, ctx) == state["tag"] and
         identity(assets(current)) == identity(state["inputAssets"]), "draft inputs or outputs changed; inspect before retry")
    inputs = args.work / "inputs"
    need(sha(inputs / MANIFEST_NAME) == state["manifestSha256"], "input manifest changed")
    manifest = decode((inputs / MANIFEST_NAME).read_bytes(), lf=True)
    payloads = validate_bundle(manifest, inputs)
    need(payloads == state["payloads"], "payload set changed")
    rows, paths = [], []
    for name in payloads:
        payload = inputs / name
        signature = inputs / (name + ".minisig")
        regular(signature)
        need(signature.stat().st_size <= 4096, "signature exceeds budget")
        run(["minisign", "-V", "-q", "-p", str(args.public_key), "-m", str(payload), "-x", str(signature)])
        rows.append({"name": name, "size": payload.stat().st_size, "sha256": sha(payload),
                     "signatureName": signature.name, "signatureSha256": sha(signature)})
        paths.append(signature)
    receipt = {"schema": "cutex/component-signing-receipt/v1", **ctx, "stagingReleaseId": release_id,
               "inputManifestSha256": state["manifestSha256"], "source": manifest["source"],
               "index": manifest["index"], "publicKeyId": KEY_ID, "payloads": rows,
               "publication": "unpublished draft only"}
    receipt_path = args.work / RECEIPT_NAME
    receipt_path.write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
    paths.append(receipt_path)
    # Only the existing isolated draft is mutated. No publishing API is present.
    github.upload(state["tag"], paths)
    after = github.release(release_id)
    need(staged(after, release_id, ctx) == state["tag"], "staging draft state changed")
    actual = assets(after)
    need(set(actual) == set(state["inputAssets"]) | {p.name for p in paths}, "unexpected output asset set")
    need(identity({n: actual[n] for n in state["inputAssets"]}) == identity(state["inputAssets"]),
         "frozen input asset identities changed")
    verified = args.work / "downloaded-outputs"
    verified.mkdir()
    for path in paths:
        github.download(actual[path.name], verified / path.name)
        need((verified / path.name).read_bytes() == path.read_bytes(), "stored output differs")
    for name in payloads:
        run(["minisign", "-V", "-q", "-p", str(args.public_key), "-m", str(inputs / name),
             "-x", str(verified / (name + ".minisig"))])
    print(f"Stored and re-verified {len(payloads)} signatures in the unpublished staging draft.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=["prepare", "store"])
    parser.add_argument("--release-id", type=int)
    parser.add_argument("--manifest-sha256")
    parser.add_argument("--work", required=True, type=Path)
    parser.add_argument("--public-key", required=True, type=Path)
    args = parser.parse_args()
    try:
        github = GitHub()
        ctx = context(github)
        if args.operation == "prepare":
            need(args.release_id is not None and args.manifest_sha256 is not None, "prepare requires frozen inputs")
            prepare(args, github, ctx)
        else:
            store(args, github, ctx)
        return 0
    except (KitError, OSError, ValueError, KeyError, TypeError, tarfile.TarError):
        # Never dump private index/event/payload content or authenticated API bodies.
        print("Component signing stopped: verification or transport failed; inspect the private inputs/run.", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
