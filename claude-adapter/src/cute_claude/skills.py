"""Bounded skills explicitly selected by the host for one workspace/account.

Only SKILL.md documents in a configured root or its immediate child directories
are indexed. We never execute YAML, discover ~/.claude, or load Claude settings.
"""
from __future__ import annotations

import json
import os
import re
from pathlib import Path

from .errors import RpcError, require, text

MAX_SKILL_BYTES = 48 * 1024
MAX_SKILLS = 256


def _ordinary_windows_path(value: str) -> str:
    """Normalize only filesystem drive/UNC encodings, never device namespaces."""
    if value[:8].lower() == '\\\\?\\unc\\':
        return '\\\\' + value[8:]
    if value.startswith('\\\\?\\') and re.match(r'^[A-Za-z]:\\', value[4:]):
        return value[4:]
    return value


def _resolved(path: Path, *, strict=False) -> Path:
    # Resolve links before normalizing spelling, so every containment check
    # still compares actual filesystem targets. Windows resolve() can preserve
    # a supplied verbatim prefix even for an ordinary local directory.
    resolved = path.resolve(strict=strict)
    if os.name != 'nt':
        return resolved
    ordinary = Path(_ordinary_windows_path(str(resolved)))
    if ordinary != resolved and resolved.exists():
        # Extended paths can name objects with otherwise significant trailing
        # dots/spaces. Removing their prefix must never select another object.
        require(ordinary.samefile(resolved), 'Windows skill path normalization changes its target')
    return ordinary


def _frontmatter(body: str) -> dict[str, str]:
    if not body.startswith('---\n'):
        return {}
    end = body.find('\n---', 4)
    require(end >= 0, 'skill frontmatter is not terminated')
    lines = body[4:end].splitlines()
    fields: dict[str, str] = {}
    for index, line in enumerate(lines):
        match = re.match(r'^(name|description|short_description):\s*(.*)$', line)
        if not match:
            continue
        key, value = match.groups()
        if value in {'>', '|', '>-', '|-'}:
            continuation = []
            for following in lines[index + 1:]:
                if following and not following[0].isspace():
                    break
                continuation.append(following.strip())
            value = ('\n' if value.startswith('|') else ' ').join(continuation).strip()
        elif value.startswith('"'):
            try:
                value = json.loads(value)
            except ValueError:
                raise RpcError(-32602, 'invalid quoted skill frontmatter') from None
        elif len(value) >= 2 and value.startswith("'") and value.endswith("'"):
            value = value[1:-1].replace("''", "'")
        require(isinstance(value, str), 'skill metadata must be text')
        fields[key] = value
    return fields


class ManagedSkills:
    def __init__(self, settings):
        self.workspace = _resolved(settings.workspace)
        self.roots = tuple(dict.fromkeys(_resolved(root) for root in settings.managed_skills_roots))

    def _cwd(self, value: str | Path | None) -> Path:
        candidate = Path(value) if value is not None else self.workspace
        require(candidate.is_absolute(), 'skill cwd must be absolute')
        candidate = _resolved(candidate)
        require(candidate == self.workspace or candidate.is_relative_to(self.workspace),
                'skill cwd is outside the configured workspace')
        return candidate

    def _read(self, candidate: Path, root: Path) -> tuple[dict, str]:
        path = _resolved(candidate, strict=True)
        require(path.is_relative_to(root) and path.name == 'SKILL.md', 'skill path escapes its managed root')
        require(path.is_file() and path.stat().st_size <= MAX_SKILL_BYTES, 'skill exceeds 48 KiB')
        with path.open('rb') as stream:
            raw = stream.read(MAX_SKILL_BYTES + 1)
        require(len(raw) <= MAX_SKILL_BYTES, 'skill grew beyond 48 KiB')
        body = raw.decode('utf-8-sig').replace('\r\n', '\n')
        fields = _frontmatter(body)
        name = fields.get('name') or path.parent.name
        description = fields.get('description') or next(
            (line.strip('# ').strip() for line in body.splitlines() if line.strip() and line.strip() != '---'), name)
        text(name, 'skill.name', limit=256)
        text(description, 'skill.description', limit=8192)
        metadata = {'name': name, 'description': description, 'path': str(path),
                    'scope': 'repo' if root.is_relative_to(self.workspace) else 'user',
                    'enabled': True, 'pluginId': None}
        if fields.get('short_description'):
            metadata['shortDescription'] = fields['short_description'][:1024]
        return metadata, body

    def _catalog(self) -> tuple[list[dict], list[dict]]:
        result, errors, seen = [], [], set()
        for root in self.roots:
            if not root.exists():
                continue
            try:
                require(root.is_dir(), 'managed skill root is not a directory')
                candidates = [root / 'SKILL.md'] if (root / 'SKILL.md').is_file() else []
                # Bound entries before sorting to avoid an unbounded directory allocation.
                for index, child in enumerate(root.iterdir()):
                    require(index < MAX_SKILLS * 4, 'managed skill root contains too many entries')
                    if child.is_dir() and (child / 'SKILL.md').is_file():
                        candidates.append(child / 'SKILL.md')
                for candidate in sorted(candidates):
                    try:
                        metadata, _ = self._read(candidate, root)
                        if metadata['path'] in seen:
                            continue
                        require(len(result) < MAX_SKILLS, 'at most 256 managed skills are supported')
                        seen.add(metadata['path'])
                        result.append(metadata)
                    except (RpcError, OSError, UnicodeError) as exc:
                        errors.append({'path': str(candidate), 'message': str(exc)[:256]})
            except (RpcError, OSError) as exc:
                errors.append({'path': str(root), 'message': str(exc)[:256]})
        return result, errors

    def list(self, params: dict) -> dict:
        require(not set(params) - {'cwds', 'forceReload'}, 'unsupported skills/list fields')
        require(type(params.get('forceReload', False)) is bool, 'forceReload must be a boolean')
        cwds = params.get('cwds') or [str(self.workspace)]
        require(isinstance(cwds, list) and len(cwds) <= 32 and all(isinstance(cwd, str) for cwd in cwds),
                'cwds must contain at most 32 paths')
        paths = [self._cwd(cwd) for cwd in cwds]
        skills, errors = self._catalog()
        return {'data': [{'cwd': str(cwd), 'skills': skills, 'errors': errors} for cwd in paths]}

    def resolve(self, item: dict, cwd: str | Path | None = None) -> tuple[str, dict]:
        self._cwd(cwd)
        require(not set(item) - {'type', 'name', 'path'}, 'unsupported skill input fields')
        name = text(item.get('name'), 'skill.name', limit=256)
        candidate = Path(text(item.get('path'), 'skill.path', limit=4096))
        require(candidate.is_absolute(), 'skill path must be absolute')
        try:
            path = _resolved(candidate, strict=True)
            skills, _ = self._catalog()
            listed = next((skill for skill in skills if Path(skill['path']) == path and skill['name'] == name), None)
            require(listed is not None, 'skill is not in the configured managed catalog')
            root = next(root for root in self.roots if path.is_relative_to(root))
            metadata, body = self._read(path, root)
            require(metadata['name'] == name, 'skill changed while reading')
        except (OSError, UnicodeError):
            raise RpcError(-32602, 'managed skill is unreadable') from None
        label = json.dumps({'name': name, 'path': str(path)}, ensure_ascii=False)
        return f'MANAGED_SKILL {label}\n{body}\nEND_MANAGED_SKILL', {'type': 'skill', 'name': name, 'path': str(path)}
