"""Read only hook presence from the CLI settings sources selected by the host."""
import json
from pathlib import Path


def hooks_configured(workspace: Path, config_dir: Path, sources: tuple[str, ...], flags: dict) -> bool:
    if flags.get('disableAllHooks') is True:
        return False
    # The pinned CLI resolves projectSettings relative to cwd, not an ancestor
    # Git root. User settings use CLAUDE_CONFIG_DIR, never the OS user's home.
    paths = []
    if 'user' in sources:
        paths.append(config_dir / 'settings.json')
    if 'project' in sources:
        paths.append(workspace / '.claude' / 'settings.json')
    configured = False
    disabled = False
    for path in paths:
        try:
            with path.open('rb') as stream:
                raw = stream.read(1024 * 1024 + 1)
        except FileNotFoundError:
            continue
        except OSError:
            return True  # Cannot establish that loaded hooks are absent.
        if len(raw) > 1024 * 1024:
            return True
        try:
            data = json.loads(raw)
        except (ValueError, UnicodeError):
            return True
        if not isinstance(data, dict):
            return True
        if isinstance(data.get('disableAllHooks'), bool):
            disabled = data['disableAllHooks']
        hooks = data.get('hooks', {})
        if not isinstance(hooks, dict):
            return True
        configured = configured or any(bool(value) for value in hooks.values())
    if isinstance(flags.get('disableAllHooks'), bool):
        disabled = flags['disableAllHooks']
    configured = configured or bool(flags.get('hooks'))
    return configured and not disabled
