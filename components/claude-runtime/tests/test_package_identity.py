"""The browser bridge must not be mistaken for the pre-browser runtime package."""
from pathlib import Path
import json
import tomllib

from cute_claude import __version__


def test_browser_adapter_package_and_runtime_identity_match():
    manifest = Path(__file__).resolve().parents[1] / "pyproject.toml"
    project = tomllib.loads(manifest.read_text(encoding="utf-8"))["project"]
    assert project["version"] == __version__ == "0.8.12"
    assert project["name"] == "cute-claude-app-server"


def test_component_manifest_matches_adapter_release():
    root = Path(__file__).resolve().parents[1]
    component = json.loads((root / 'component.json').read_text(encoding='utf-8'))
    features = json.loads((root / 'src/cute_claude/features.json').read_text(encoding='utf-8'))
    assert component['version'] == features['release']
    assert component['version'].startswith('claude-r')
