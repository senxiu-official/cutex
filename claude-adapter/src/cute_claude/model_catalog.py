"""Reference choices plus SDK metadata, never an account entitlement check.

Reading the catalog may run the already selected CLI's --version. It does not
connect the SDK, inspect credentials, resolve aliases, or send a model request.
"""
from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time


REFERENCE_PATH = Path(__file__).with_name("model_catalog.json")
EFFORT_LEVELS = {"low", "medium", "high", "xhigh", "max"}
VERSION = re.compile(r"^v?([0-9]{1,20})\.([0-9]{1,20})\.([0-9]{1,20})(?: \(Claude Code\))?$")


def parse_cli_version(output: str) -> str | None:
    """Accept only CLI version output, not an incidental version in a warning."""
    match = VERSION.fullmatch(output.strip())
    if not match or any(int(value) > 2**64 - 1 for value in match.groups()):
        return None
    return ".".join(match.groups())


class CliVersionProbe:
    """One bounded probe per executable identity, including failed probes."""
    def __init__(self, *, timeout: float = 3.0, output_limit: int = 4096, ttl: float = 300.0):
        self.timeout, self.output_limit = timeout, output_limit
        self.ttl, self._checked = ttl, 0.0
        self._key = None
        self._version = None
        self._lock = asyncio.Lock()

    async def read(self, cli: Path | None) -> str | None:
        if cli is None:
            return None
        cli = Path(cli)
        # driver.cli has already passed official_cli validation. Never search
        # PATH here or replace a missing executable with another installation.
        if not cli.is_absolute():
            return None
        try:
            info = cli.stat()
            key = (str(cli), info.st_mtime_ns, info.st_size)
            exists = cli.is_file()
        except OSError:
            key, exists = (str(cli), None), False
        async with self._lock:
            if key != self._key or time.monotonic() - self._checked >= self.ttl:
                version = await self._run(cli) if exists else None
                self._key, self._version = key, version
                self._checked = time.monotonic()
            return self._version

    async def _run(self, cli: Path) -> str | None:
        try:
            with tempfile.TemporaryDirectory(prefix="cutex-claude-version-") as scratch:
                return await self._execute(cli, Path(scratch))
        except OSError:
            return None

    async def _execute(self, cli: Path, scratch: Path) -> str | None:
        process = None
        env = {key: os.environ[key] for key in ("SYSTEMROOT", "WINDIR", "PATH", "TEMP", "TMP")
               if key in os.environ}
        env.update({key: str(scratch) for key in ("HOME", "USERPROFILE", "CLAUDE_CONFIG_DIR",
                    "APPDATA", "LOCALAPPDATA", "XDG_CONFIG_HOME", "XDG_CACHE_HOME")})
        env.update({key: "1" for key in ("DISABLE_AUTOUPDATER", "DISABLE_TELEMETRY",
                    "DISABLE_ERROR_REPORTING", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC")})
        try:
            async with asyncio.timeout(self.timeout):
                process = await asyncio.create_subprocess_exec(
                    str(cli), "--version", stdin=asyncio.subprocess.DEVNULL,
                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
                    limit=self.output_limit, env=env, cwd=str(scratch),
                    creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0,
                )
                chunks, size = [], 0
                while chunk := await process.stdout.read(min(1024, self.output_limit + 1 - size)):
                    chunks.append(chunk)
                    size += len(chunk)
                    if size > self.output_limit:
                        return None
                code = await process.wait()
                if code == 0:
                    return parse_cli_version(b"".join(chunks).decode("utf-8", errors="replace"))
        except (OSError, TimeoutError, ValueError):
            return None
        finally:
            if process is not None and process.returncode is None:
                try:
                    process.kill()
                except ProcessLookupError:
                    pass
                # Kill before draining: output and cleanup remain bounded even
                # if a broken executable emits indefinitely or keeps pipes open.
                try:
                    await asyncio.wait_for(process.communicate(), timeout=1.0)
                except (OSError, TimeoutError):
                    pass
        return None


def reference_catalog() -> dict:
    raw = REFERENCE_PATH.read_bytes()
    if len(raw) > 1024 * 1024:
        raise ValueError("Model reference catalog exceeds size limit")
    catalog = json.loads(raw)
    if not isinstance(catalog, dict) or type(catalog.get("version")) is not int or catalog["version"] != 1:
        raise ValueError("Unsupported model reference catalog version")
    def label(value, limit):
        return (isinstance(value, str) and 0 < len(value.encode("utf-8")) <= limit
                and not any(ord(ch) < 32 or 127 <= ord(ch) < 160 for ch in value))
    models = catalog["models"]
    if (not isinstance(models, list) or not 0 < len(models) <= 512
            or not label(catalog["updatedAt"], 64) or len(catalog["sourceUrls"]) > 32
            or any(not label(url, 2048) or not url.startswith("https://") for url in catalog["sourceUrls"])):
        raise ValueError("Invalid model reference metadata")
    for row in models:
        minimum, levels = row["minCliVersion"], row["supportedEffortLevels"]
        if (not label(row["id"], 200) or any(ch.isspace() for ch in row["id"])
                or not label(row["displayName"], 200)
                or not isinstance(row["description"], str) or len(row["description"].encode("utf-8")) > 4000
                or any(ord(ch) < 32 or 127 <= ord(ch) < 160 for ch in row["description"])
                or (row["family"] is not None and not label(row["family"], 100))
                or row["kind"] not in {"pinned", "alias", "default"}
                or (minimum is not None and (not isinstance(minimum, str) or parse_cli_version(minimum) != minimum))
                or (levels is not None and (not isinstance(levels, list)
                    or any(not isinstance(level, str) or level not in EFFORT_LEVELS for level in levels)))):
            raise ValueError("Invalid model reference entry")
    ids = [row["id"] for row in models]
    if len(ids) != len(set(ids)):
        raise ValueError("Duplicate model reference ID")
    return catalog


def compatibility(cli_version: str | None, minimum: str | None) -> dict:
    version = parse_cli_version(cli_version) if cli_version else None
    if version is None:
        return {"state": "unknown", "reason": "cliVersionUnknown"}
    if not minimum:
        return {"state": "unknown", "reason": "minimumCliVersionUnknown"}
    if tuple(map(int, version.split("."))) < tuple(map(int, minimum.split("."))):
        return {"state": "incompatible", "reason": "cliVersionTooOld"}
    return {"state": "compatible", "reason": None}


def _metadata_text(value) -> bool:
    return (isinstance(value, str) and bool(value.strip())
            and len(value.encode("utf-8")) <= 256
            and not any(ord(ch) < 32 or 127 <= ord(ch) < 160 for ch in value))


def merge_catalog(reference: dict, *, cli_version: str | None, selected_model: str,
                  retained_models=(), sdk_names: dict | None = None,
                  sdk_efforts: dict | None = None) -> dict:
    """Keep reference IDs even if the SDK only advertises dynamic aliases."""
    sdk_names = {key: label for key, label in (sdk_names or {}).items()
                 if _metadata_text(key) and _metadata_text(label)}
    sdk_efforts = sdk_efforts or {}
    retained = set(retained_models) | {selected_model}
    rows = {row["id"]: dict(row) for row in reference["models"]}
    order = list(dict.fromkeys([selected_model, *rows, *sdk_names, *sorted(retained)]))
    entries = []
    for model in order:
        entry = rows.get(model, {
            "id": model, "displayName": sdk_names.get(model, model), "family": None,
            "kind": "custom", "minCliVersion": None, "supportedEffortLevels": None,
            "description": "Custom or SDK-reported model ID; account availability unknown.",
        }).copy()
        # A handshake label is not evidence that an alias resolved to that
        # concrete model. Keep the reference label and selection semantics.
        if entry["kind"] == "alias" and not entry["displayName"].endswith(" (alias)"):
            entry["displayName"] += " (alias)"
        sources = (["reference"] if model in rows else [])
        if model in sdk_names:
            sources.append("sdkHandshake")
        if model in retained:
            sources.append("savedSelection")
        levels = sdk_efforts.get(model)
        if isinstance(levels, list):
            entry["supportedEffortLevels"] = list(dict.fromkeys(
                level for level in levels if isinstance(level, str) and level in EFFORT_LEVELS))
            effort_source = "sdkHandshake"
        else:
            effort_source = "reference" if entry["supportedEffortLevels"] is not None else "unknown"
        cli = compatibility(cli_version, entry["minCliVersion"])
        if cli["state"] == "incompatible":
            entry["description"] += f" Requires Claude Code >= {entry['minCliVersion']}; installed {cli_version}."
            if model in retained:
                entry["description"] += " Kept because it is a saved selection."
        elif cli["state"] == "unknown":
            entry["description"] += " CLI compatibility is unverified."
        entry.update({"sources": sources, "effortSource": effort_source,
                      "cliCompatibility": cli, "accountAvailability": "unknown",
                      "selected": model == selected_model, "retained": model in retained,
                      "hidden": cli["state"] == "incompatible" and model not in retained})
        entries.append(entry)
    return {"version": reference["version"], "source": {
                "kind": "reference+sdk-handshake", "updatedAt": reference["updatedAt"],
                "sourceUrls": list(reference["sourceUrls"])},
            "cliVersion": cli_version, "accountAvailability": "unknown",
            "selectedModel": selected_model, "entries": entries}


class ModelCatalog:
    def __init__(self, server):
        self.server = server
        self.reference = reference_catalog()
        self.probe = CliVersionProbe()

    async def read(self, tid: str | None = None) -> dict:
        server = self.server
        selected = server.store.thread(tid)["model"] if tid else server.model_config.model()
        retained = [row[0] for row in server.store.db.execute("SELECT DISTINCT model FROM threads")]
        retained.append(server.model_config.model())
        driver = getattr(server.engine, "driver", None)
        names, efforts = {}, {}
        # Read cached handshakes only; catalog reads must not start an owner.
        for owner in getattr(driver, "owners", {}).values():
            names.update(getattr(owner, "model_catalog", {}))
            efforts.update(getattr(owner, "model_efforts", {}))
        version = await self.probe.read(getattr(driver, "cli", None))
        return merge_catalog(self.reference, cli_version=version, selected_model=selected,
                             retained_models=retained, sdk_names=names, sdk_efforts=efforts)


def native_models(entries: list[dict], *, default_model: str, configured_effort: str | None,
                  subscription: bool, mock: bool = False) -> dict:
    data = []
    for entry in entries:
        model = entry["id"]
        levels = entry.get("supportedEffortLevels")
        efforts = (([{"reasoningEffort": "none", "description": "Claude default effort"}] if subscription else []) + [
            {"reasoningEffort": level, "description": "Reconnects the same Claude session to apply this effort"}
            for level in levels] if levels else [])
        data.append({"id": model, "model": model, "upgrade": None, "upgradeInfo": None,
            "availabilityNux": None, "displayName": "Mock " + model if mock else entry["displayName"],
            "description": entry["description"], "modelSpecialty": None, "hidden": entry.get("hidden", False),
            "supportedReasoningEfforts": efforts,
            "defaultReasoningEffort": configured_effort if configured_effort in (levels or []) else "none",
            "inputModalities": ["text", "image"] if subscription else ["text"], "supportsPersonality": False,
            "multiAgentVersion": None, "additionalSpeedTiers": [], "serviceTiers": [],
            "defaultServiceTier": None, "isDefault": model == default_model})
    return {"data": data, "nextCursor": None}
