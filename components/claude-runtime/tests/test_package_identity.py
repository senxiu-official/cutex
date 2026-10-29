"""The browser bridge must not be mistaken for the pre-browser runtime package."""
from pathlib import Path
import tomllib

from cute_claude import __version__


def test_browser_adapter_package_and_runtime_identity_match():
    manifest = Path(__file__).resolve().parents[1] / "pyproject.toml"
    project = tomllib.loads(manifest.read_text(encoding="utf-8"))["project"]
    assert project["version"] == __version__ == "0.8.5"
    assert project["name"] == "cute-claude-app-server"
