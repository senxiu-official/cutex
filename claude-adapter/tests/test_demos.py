from pathlib import Path
import subprocess
import sys
import pytest


@pytest.mark.parametrize("script", ["demo.py", "demo_job.py"])
def test_shipped_offline_demos(script):
    root = Path(__file__).parents[1]
    result = subprocess.run([sys.executable, str(root / "scripts" / script)], cwd=root, text=True, capture_output=True, timeout=30)
    assert result.returncode == 0, result.stdout + result.stderr
    assert "passed" in result.stdout
