"""Live model / effort / fast mode / auto-compaction changes on a running Claude Code child.

Checked on 2026-10-22 against Claude Code 2.1.295 and 2.1.301 (stream-json, private home, a fake Messages API
that records every request; desktop design 037):

- `set_model` and `apply_flag_settings` take effect from the next model request, also in the middle of a turn
  (the step after a running tool already uses the new model / effort).
- `apply_flag_settings` merges into the `--settings` layer (fast mode set at launch survives an effort-only
  change). `effortLevel: null` returns to the model's default; an unknown level is acknowledged and ignored, so
  levels are validated before (protocol.py).
- `fastMode` true / false applies live (`speed: fast` on the next request).
- `env` with `CLAUDE_CODE_AUTO_COMPACT_WINDOW` / `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` applies live in both
  directions and outranks the launch environment; `""` unsets an override. The `autoCompactWindow` flag
  setting is read only at start, so it is not used.
- `set_max_thinking_tokens` changes nothing on adaptive-thinking models (not used).

An older or unknown CLI keeps the restart path (idle only).
"""
from __future__ import annotations

import json
import os

from .model_catalog import parse_cli_version

LIVE_MIN_CLI = (2, 1, 295)
WINDOW_ENV = 'CLAUDE_CODE_AUTO_COMPACT_WINDOW'
PERCENT_ENV = 'CLAUDE_AUTOCOMPACT_PCT_OVERRIDE'
# Set by cutex (profiles/<id>/claude-compaction-models.json): {"<model id>": {"window": n, "percent": p}}.
# Older adapters ignore it; the profile-wide values stay in WINDOW_ENV / PERCENT_ENV.
MODELS_ENV = 'CUTEX_CLAUDE_COMPACTION_MODELS'
WINDOW_RANGE = range(100_000, 1_000_001)
PERCENT_RANGE = range(1, 101)


def live_supported(cli_version: str | None, environ=os.environ) -> bool:
    """Whether model / effort / fast mode changes can go to the running child."""
    version = parse_cli_version(cli_version) if cli_version else None
    if version is None:
        return False
    # Claude Code ignores a live effort change while this variable pins the effort.
    if environ.get('CLAUDE_CODE_EFFORT_LEVEL'):
        return False
    return tuple(int(part) for part in version.split('.')) >= LIVE_MIN_CLI


def _value(window, percent) -> tuple[int, int] | None:
    try:
        window, percent = int(window), int(percent)
    except (TypeError, ValueError):
        return None
    if isinstance(window, bool) or window not in WINDOW_RANGE or percent not in PERCENT_RANGE:
        return None
    return window, percent


class Compaction:
    """Auto-compaction values per model: the profile's table entry, else the profile-wide values the
    adapter was launched with, else none (Claude Code decides)."""

    def __init__(self, environ=os.environ):
        self.default = _value(environ.get(WINDOW_ENV), environ.get(PERCENT_ENV)) \
            if environ.get(WINDOW_ENV) and environ.get(PERCENT_ENV) else None
        self.models: dict[str, tuple[int, int]] = {}
        raw = environ.get(MODELS_ENV)
        try:
            table = json.loads(raw) if raw else {}
        except ValueError:
            table = {}
        if isinstance(table, dict):
            for model, entry in table.items():
                value = _value(entry.get('window'), entry.get('percent')) if isinstance(entry, dict) else None
                if isinstance(model, str) and model and value is not None:
                    self.models[model] = value

    def for_model(self, model: str | None) -> tuple[int, int] | None:
        return self.models.get(model or '', self.default)

    def child_env(self, model: str | None) -> dict[str, str]:
        """Environment for a new child running `model` (on top of the adapter's own)."""
        value = self.for_model(model)
        if value is None or value == self.default:
            return {}
        return {WINDOW_ENV: str(value[0]), PERCENT_ENV: str(value[1])}

    def differs(self, old: str | None, new: str | None) -> bool:
        return self.for_model(old) != self.for_model(new)

    def switch_env(self, old: str | None, new: str | None) -> dict[str, str] | None:
        """`apply_flag_settings` env for a live switch from `old` to `new`; None when nothing changes."""
        if not self.differs(old, new):
            return None
        value = self.for_model(new)
        if value is None:
            return {WINDOW_ENV: '', PERCENT_ENV: ''}
        return {WINDOW_ENV: str(value[0]), PERCENT_ENV: str(value[1])}
