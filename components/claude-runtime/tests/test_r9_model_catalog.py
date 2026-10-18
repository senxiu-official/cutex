"""Offline directory/protocol contracts; no real account or model is contacted."""
from __future__ import annotations

import asyncio
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest

from cute_claude.model_catalog import (
    CliVersionProbe, compatibility, merge_catalog, parse_cli_version, reference_catalog,
)
from test_persistent import ph


def indexed(catalog):
    return {row["id"]: row for row in catalog["entries"]}


def merged(**kwargs):
    return merge_catalog(reference_catalog(), cli_version=kwargs.pop("cli_version", None),
                         selected_model=kwargs.pop("selected_model", "sonnet"), **kwargs)


def test_reference_fixed_ids_efforts_and_aliases_are_separate():
    reference = reference_catalog()
    rows = {row["id"]: row for row in reference["models"]}
    expected = {"claude-opus-5-5": "2.1.280", "claude-sonnet-5-5": "2.1.284",
                "claude-fable-5-1": "2.1.257", "claude-haiku-5-5": "2.1.293"}
    for model, minimum in expected.items():
        assert rows[model]["kind"] == "pinned"
        assert rows[model]["minCliVersion"] == minimum
        assert rows[model]["supportedEffortLevels"] == ["low", "medium", "high", "xhigh", "max"]
    for model in ("claude-opus-4-6", "claude-sonnet-4-6"):
        assert rows[model]["supportedEffortLevels"] == ["low", "medium", "high", "max"]
    for model in ("claude-haiku-4-5-20251001", "claude-opus-4-5-20251101", "claude-sonnet-4-5-20250929"):
        assert rows[model]["kind"] == "pinned"
        assert rows[model]["supportedEffortLevels"] == []
    assert rows["default"]["kind"] == "default"
    for row in rows.values():
        if row["kind"] == "alias":
            assert row["displayName"].endswith(" (alias)")
            assert row["supportedEffortLevels"] is None
    assert "https://code.claude.com/docs/en/model-config" in reference["sourceUrls"]


def test_nonempty_sdk_catalog_keeps_pinned_versions_without_resolving_aliases():
    rows = indexed(merged(sdk_names={"opus": "Opus 5.5", "future-model": "Provider custom"}))
    assert rows["claude-opus-5-5"]["displayName"] == "Opus 5.5"
    assert rows["opus"]["displayName"] == "Opus (alias)"
    assert rows["future-model"]["displayName"] == "Provider custom"
    assert rows["future-model"]["kind"] == "custom"
    assert rows["opus"]["sources"] == ["reference", "sdkHandshake"]
    assert all(row["accountAvailability"] == "unknown" for row in rows.values())


@pytest.mark.parametrize("cli,minimum,state,reason", [
    ("2.1.279", "2.1.280", "incompatible", "cliVersionTooOld"),
    ("2.1.280", "2.1.280", "compatible", None),
    ("2.1.1000", "2.1.293", "compatible", None),
    ("2.2.0", "2.1.293", "compatible", None),
    (None, "2.1.293", "unknown", "cliVersionUnknown"),
    ("2.1.293-beta", "2.1.293", "unknown", "cliVersionUnknown"),
    ("2.1.293", None, "unknown", "minimumCliVersionUnknown"),
])
def test_cli_compatibility_is_not_account_availability(cli, minimum, state, reason):
    assert compatibility(cli, minimum) == {"state": state, "reason": reason}


def test_old_cli_hides_only_unselected_incompatible_choices():
    rows = indexed(merged(cli_version="2.1.280", selected_model="claude-sonnet-5-5",
                          retained_models=["claude-haiku-5-5", "my-deployment"]))
    assert not rows["claude-sonnet-5-5"]["hidden"]
    assert rows["claude-sonnet-5-5"]["cliCompatibility"]["state"] == "incompatible"
    assert "saved selection" in rows["claude-sonnet-5-5"]["description"]
    assert not rows["claude-haiku-5-5"]["hidden"]
    assert rows["my-deployment"]["retained"] and rows["my-deployment"]["kind"] == "custom"
    rows = indexed(merged(cli_version="2.1.279"))
    assert rows["claude-opus-5-5"]["hidden"]
    assert not rows["claude-fable-5-1"]["hidden"]


def test_unknown_cli_and_alias_efforts_do_not_fabricate_capability():
    rows = indexed(merged(sdk_names={"opus": "Opus 5.5"}, sdk_efforts={"opus": ["max"]}))
    assert all(row["cliCompatibility"]["state"] == "unknown" for row in rows.values())
    assert not any(row["hidden"] for row in rows.values())
    assert rows["opus"]["supportedEffortLevels"] == ["max"]
    assert rows["sonnet"]["supportedEffortLevels"] is None
    assert rows["claude-opus-5-5"]["supportedEffortLevels"] == ["low", "medium", "high", "xhigh", "max"]
    assert rows["claude-opus-5-5"]["effortSource"] == "reference"


def test_exact_sdk_effort_metadata_overrides_reference_including_empty():
    rows = indexed(merged(sdk_efforts={"claude-opus-5-5": [], "claude-sonnet-5-5": ["low", "low", "invalid", {}]}))
    assert rows["claude-opus-5-5"]["supportedEffortLevels"] == []
    assert rows["claude-opus-5-5"]["effortSource"] == "sdkHandshake"
    assert rows["claude-sonnet-5-5"]["supportedEffortLevels"] == ["low"]


@pytest.mark.parametrize("output,expected", [
    ("2.1.293 (Claude Code)\r\n", "2.1.293"), ("v2.1.293", "2.1.293"),
    ("Python 3.13.1", None), ("warning 2.1.293 (Claude Code)", None),
    ("warning\n2.1.293 (Claude Code)", None), ("2.1.293-beta", None),
    ("2.1.293\n(Claude Code)", None), ("2.1.293 extra", None),
    ("２.１.２９３", None),
])
def test_cli_version_accepts_only_standalone_version_output(output, expected):
    assert parse_cli_version(output) == expected


async def test_probe_caches_concurrent_calls_failures_ttl_and_binary_changes(tmp_path, monkeypatch):
    cli = tmp_path / "private-fixture.exe"
    cli.write_bytes(b"fixture")
    now = [10.0]
    monkeypatch.setattr("cute_claude.model_catalog.time.monotonic", lambda: now[0])
    probe = CliVersionProbe(ttl=300)
    run = AsyncMock(side_effect=[None, "2.1.280", "2.1.293"])
    monkeypatch.setattr(probe, "_run", run)
    assert await asyncio.gather(*(probe.read(cli) for _ in range(8))) == [None] * 8
    assert run.await_count == 1
    now[0] += 299
    assert await probe.read(cli) is None and run.await_count == 1
    now[0] += 2
    assert await probe.read(cli) == "2.1.280" and run.await_count == 2
    cli.write_bytes(b"updated fixture binary")
    assert await probe.read(cli) == "2.1.293" and run.await_count == 3


async def test_probe_missing_or_relative_path_never_searches_path(tmp_path, monkeypatch):
    probe = CliVersionProbe()
    run = AsyncMock()
    monkeypatch.setattr(probe, "_run", run)
    for cli in (None, Path("claude"), tmp_path / "missing"):
        assert await probe.read(cli) is None
    run.assert_not_awaited()


@pytest.mark.parametrize("mode", ["success", "timeout", "oversized", "wrongExecutable"])
async def test_probe_is_bounded_isolated_and_cleans_child(tmp_path, monkeypatch, mode):
    import os
    import subprocess
    cli = tmp_path / "private-fixture.exe"
    cli.touch()
    process = SimpleNamespace(returncode=None)
    process.killed = False
    def kill():
        process.killed, process.returncode = True, -9
    async def wait():
        process.returncode = 0
        return 0
    async def hung_read(size):
        await asyncio.Event().wait()
    output = b"Python 3.13.1" if mode == "wrongExecutable" else b"2.1.293 (Claude Code)"
    process.stdout = SimpleNamespace(read=AsyncMock(
        side_effect=hung_read if mode == "timeout" else [b"x" * 65] if mode == "oversized" else [output, b""]))
    process.kill, process.wait = kill, wait
    process.communicate = AsyncMock(return_value=(b"", b""))
    calls = []
    async def spawn(*args, **kwargs):
        calls.append((args, kwargs))
        assert Path(kwargs["cwd"]).is_dir()
        return process
    monkeypatch.setenv("CLAUDE_CODE_OAUTH_TOKEN", "secret-fixture-not-a-token")
    monkeypatch.setenv("ANTHROPIC_API_KEY", "secret-fixture-not-a-key")
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path / "real-account-must-not-read"))
    monkeypatch.setattr("cute_claude.model_catalog.asyncio.create_subprocess_exec", spawn)
    probe = CliVersionProbe(timeout=.03, output_limit=64)
    assert await probe.read(cli) == ("2.1.293" if mode == "success" else None)
    args, options = calls[0]
    assert args == (str(cli), "--version")
    env = options["env"]
    assert "ANTHROPIC_API_KEY" not in env and "CLAUDE_CODE_OAUTH_TOKEN" not in env
    assert env["HOME"] == env["USERPROFILE"] == env["CLAUDE_CONFIG_DIR"] == options["cwd"]
    assert env["DISABLE_AUTOUPDATER"] == env["DISABLE_TELEMETRY"] == "1"
    assert not Path(options["cwd"]).exists()
    assert options["creationflags"] == (subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0)
    assert process.killed is (mode in {"timeout", "oversized"})


async def test_catalog_rpc_offline_retains_custom_model_without_login(ph, monkeypatch):
    tid = (await ph.call("thread/start", {"model": "my-deployment"}))["thread"]["id"]
    probe = AsyncMock(return_value=None)
    monkeypatch.setattr(ph.server.model_catalog.probe, "read", probe)
    result = await ph.call("claude/models/catalog", {"threadId": tid})
    rows = indexed(result)
    assert result["selectedModel"] == "my-deployment" and rows["my-deployment"]["selected"]
    assert result["cliVersion"] is None and result["accountAvailability"] == "unknown"
    assert rows["claude-sonnet-5-5"]["cliCompatibility"]["state"] == "unknown"
    native = await ph.call("model/list")
    assert native["nextCursor"] is None
    assert "my-deployment" in {row["id"] for row in native["data"]}
    assert not ph.sdk.clients and not ph.logins and not ph.engine.driver.owners
    assert (await ph.call("claude/model/status", {"threadId": tid}))["requestedModel"] == "my-deployment"
    assert not ph.sdk.clients and not ph.logins


async def test_native_picker_preserves_sdk_and_pinned_effort_metadata(ph, monkeypatch):
    tid = await ph.thread()
    await ph.call("claude/runtime/start", {"threadId": tid})
    monkeypatch.setattr(ph.server.model_catalog.probe, "read", AsyncMock(return_value="2.1.280"))
    result = await ph.call("model/list")
    rows = {row["id"]: row for row in result["data"]}
    assert rows["sonnet"]["displayName"] == "Sonnet (alias)"
    assert rows["claude-opus-5-5"]["displayName"] == "Opus 5.5"
    assert rows["claude-sonnet-5-5"]["hidden"]
    assert not rows["claude-opus-5-5"]["hidden"]
    assert [row["reasoningEffort"] for row in rows["claude-opus-4-6"]["supportedReasoningEfforts"]] == ["none", "low", "medium", "high", "max"]
    assert rows["haiku"]["supportedReasoningEfforts"] == []
    assert rows["claude-haiku-5-5"]["supportedReasoningEfforts"]
    assert rows["future-model"]["displayName"] == "Future-model"
    assert ph.sdk.clients[0].query_count == 0


@pytest.mark.parametrize("method,params", [
    ("claude/models/catalog", {"model": "sonnet"}),
    ("claude/models/catalog", {"threadId": None}),
    ("claude/models/catalog", {"threadId": 3}),
    ("claude/model/status", {}),
    ("claude/model/status", {"threadId": "missing", "fresh": True}),
])
async def test_model_extensions_reject_unknown_or_invalid_params(ph, method, params):
    error = await ph.call(method, params, error=True)
    assert error["code"] == -32602
    assert not ph.sdk.clients and not ph.logins


@pytest.mark.parametrize("method", ["claude/models/catalog", "claude/model/status"])
async def test_model_extensions_unknown_thread_is_not_found(ph, method):
    assert (await ph.call(method, {"threadId": "missing"}, error=True))["code"] == -32004
    assert not ph.sdk.clients and not ph.logins


async def test_non_sdk_extensions_fail_typed_and_native_picker_stays_small(h):
    capabilities = await h.call("claude/capabilities")
    assert not capabilities["features"]["versionedModelCatalog"]
    assert not capabilities["features"]["modelObservation"]
    for method, feature in (("claude/models/catalog", "versionedModelCatalog"), ("claude/model/status", "modelObservation")):
        error = await h.call(method, error=True)
        assert error["code"] == -32601 and error["data"]["feature"] == feature
    models = (await h.call("model/list"))["data"]
    assert len(models) == 1 and models[0]["id"] == h.settings.model


def test_reference_context_windows_are_nominal_registry_values():
    rows = {row["id"]: row for row in reference_catalog()["models"]}
    for model, row in rows.items():
        if row["kind"] == "pinned":
            assert row["contextWindow"] in (200_000, 1_000_000), model
    assert rows["claude-opus-5-5"]["contextWindow"] == 1_000_000
    assert rows["claude-opus-4-6"]["contextWindow"] == 200_000
    # Plain aliases resolve per account; only the explicit 1M aliases are known.
    assert rows["sonnet"]["contextWindow"] is None and rows["default"]["contextWindow"] is None
    assert rows["sonnet[1m]"]["contextWindow"] == rows["opus[1m]"]["contextWindow"] == 1_000_000


@pytest.mark.parametrize("window", [0, -1, "1000000", 1.5, True, 100_000_001])
def test_reference_rejects_invalid_context_window(tmp_path, monkeypatch, window):
    import json
    import cute_claude.model_catalog as catalog
    data = json.loads(catalog.REFERENCE_PATH.read_text())
    data["models"][0]["contextWindow"] = window
    path = tmp_path / "model_catalog.json"
    path.write_text(json.dumps(data))
    monkeypatch.setattr(catalog, "REFERENCE_PATH", path)
    with pytest.raises(ValueError):
        catalog.reference_catalog()


async def test_effort_change_accepts_reference_levels_the_picker_advertises(ph, monkeypatch):
    # Pinned IDs are not in the SDK handshake; model/list offers their reference
    # levels, so thread/settings/update must accept the same levels (owner report
    # 2026-10-18: "This model does not advertise the selected effort").
    tid = await ph.thread()
    await ph.call("claude/runtime/start", {"threadId": tid})
    monkeypatch.setattr(ph.server.model_catalog.probe, "read", AsyncMock(return_value="2.1.299"))
    listed = {row["id"]: [x["reasoningEffort"] for x in row["supportedReasoningEfforts"]]
              for row in (await ph.call("model/list"))["data"]}
    assert "xhigh" in listed["claude-opus-4-8"]
    await ph.call("thread/settings/update", {"threadId": tid, "model": "claude-opus-4-8", "effort": "xhigh"})
    assert ph.engine.runtime_status(tid)["effort"] == "xhigh"
    assert "xhigh" not in listed["claude-opus-4-6"]
    error = await ph.call("thread/settings/update", {"threadId": tid, "model": "claude-opus-4-6", "effort": "xhigh"}, error=True)
    assert error["code"] == -32602 and "offered: low, medium, high, max" in error["message"]
    assert listed["claude-haiku-4-5-20251001"] == []
    error = await ph.call("thread/settings/update", {"threadId": tid, "model": "claude-haiku-4-5-20251001", "effort": "high"}, error=True)
    assert "offers no effort levels" in error["message"]
    assert ph.engine.runtime_status(tid)["effort"] == "xhigh"
