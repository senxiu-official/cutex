"""Persistent "always allow" rules (cutex `claude-permission-rules`).

cutex keeps the rules per workspace (never in the repository's
`.claude/settings.json`) and writes this session's list to
``<state dir>/permission-rules.json`` (``{"allow": [...]}``) before it starts
the adapter and whenever the list changes. The adapter passes them to the
official runtime as flag settings (``settings.permissions.allow``), which
Claude Code reads even with ``setting_sources=[]``.

A rule uses Claude Code's permission rule syntax: ``Tool`` or
``Tool(specifier)``, e.g. ``Bash(git status:*)``, ``Edit(src/**)``,
``WebFetch(domain:example.com)``.
"""
from __future__ import annotations

import json
import re
from pathlib import Path

RULES_FILE = 'permission-rules.json'
MAX_RULES = 256
MAX_RULE_CHARS = 1024
TOOL_NAME = re.compile(r'[A-Za-z][A-Za-z0-9_]*(?:__[A-Za-z0-9_.-]+)*')
EDIT_TOOLS = {'Edit', 'MultiEdit', 'Write', 'NotebookEdit'}


def valid_rule(rule: object) -> bool:
    if not isinstance(rule, str) or not rule or len(rule) > MAX_RULE_CHARS:
        return False
    if any(ch in rule for ch in '\r\n\0'):
        return False
    name, paren, rest = rule.partition('(')
    if not TOOL_NAME.fullmatch(name):
        return False
    return not paren or (rest.endswith(')') and len(rest) > 1)


def rule_string(tool_name: object, content: object = None) -> str | None:
    if not isinstance(tool_name, str):
        return None
    rule = f'{tool_name}({content})' if isinstance(content, str) and content else tool_name
    return rule if valid_rule(rule) else None


def _field(obj, key, default=None):
    return obj.get(key, default) if isinstance(obj, dict) else getattr(obj, key, default)


def from_suggestions(suggestions) -> list[str]:
    """Allow rules the official runtime suggested for this request (its own
    "don't ask again" proposals), whatever settings file they named."""
    rules: list[str] = []
    for update in suggestions or []:
        if _field(update, 'type') not in {'addRules', 'replaceRules'} or _field(update, 'behavior') != 'allow':
            continue
        for value in _field(update, 'rules') or []:
            rule = rule_string(_field(value, 'tool_name', _field(value, 'toolName')),
                               _field(value, 'rule_content', _field(value, 'ruleContent')))
            if rule and rule not in rules:
                rules.append(rule)
    return rules


def fallback(name: str, arguments: dict, workspace: Path) -> list[str]:
    """A narrow rule when the runtime suggested none: the exact Bash command,
    edits below the edited file's folder (workspace-relative), else the tool."""
    if name == 'Bash':
        command = arguments.get('command')
        rule = rule_string('Bash', command.strip()) if isinstance(command, str) and command.strip() else None
        return [rule] if rule else []
    if name in EDIT_TOOLS:
        raw = arguments.get('notebook_path' if name == 'NotebookEdit' else 'file_path')
        try:
            relative = Path(str(raw)).resolve().parent.relative_to(workspace.resolve())
        except (OSError, ValueError, TypeError):
            return []
        # Edit rules cover every built-in file-editing tool (gitignore-style path).
        rule = rule_string('Edit', '**' if not relative.parts else f'{relative.as_posix()}/**')
        return [rule] if rule else []
    rule = rule_string(name)
    return [rule] if rule else []


def offered(name: str, arguments: dict, context, workspace: Path) -> list[str]:
    rules = from_suggestions(_field(context, 'suggestions'))
    return (rules or fallback(name, arguments, workspace))[:8]


def load(state_dir: Path) -> list[str]:
    """This session's persisted allow rules; invalid entries are skipped and a
    missing / unreadable file means none (the runtime keeps asking)."""
    try:
        value = json.loads((state_dir / RULES_FILE).read_text(encoding='utf-8'))
    except (OSError, ValueError):
        return []
    allow = value.get('allow') if isinstance(value, dict) else None
    if not isinstance(allow, list):
        return []
    return list(dict.fromkeys(rule for rule in allow if valid_rule(rule)))[:MAX_RULES]


def with_rules(settings: dict, rules: list[str], *, explicit: bool = False) -> dict:
    """`settings` (flag settings) plus the persisted rules in
    ``permissions.allow`` (after any allow rules already there). `explicit`
    writes an empty list too, so a live update can clear removed rules."""
    if not rules and not explicit:
        return settings
    permissions = dict(settings.get('permissions') or {})
    existing = [r for r in permissions.get('allow') or [] if isinstance(r, str)]
    permissions['allow'] = list(dict.fromkeys([*existing, *rules]))
    return {**settings, 'permissions': permissions}


def flag_settings(base: dict | None, rules: list[str]) -> str | None:
    """The `settings` option (flag settings JSON): `base` plus
    ``permissions.allow``; None when there is nothing to pass."""
    settings = with_rules(dict(base or {}), rules)
    return json.dumps(settings) if settings else None
