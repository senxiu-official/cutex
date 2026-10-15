"""Windows spelling parity must preserve resolved filesystem boundaries."""
import itertools
import os
import subprocess
from pathlib import Path

import pytest

from cute_claude.config import Settings
from cute_claude.errors import RpcError
from cute_claude.skills import ManagedSkills, _ordinary_windows_path


@pytest.mark.parametrize(('value', 'expected'), [
    (r'\\?\C:\project\skill\SKILL.md', r'C:\project\skill\SKILL.md'),
    (r'\\?\d:\project', r'd:\project'),
    (r'\\?\UNC\server\share\project\SKILL.md', r'\\server\share\project\SKILL.md'),
    (r'\\?\unc\server\share\project', r'\\server\share\project'),
    (r'C:\project', r'C:\project'),
    (r'\\server\share\project', r'\\server\share\project'),
    (r'\\?\GLOBALROOT\Device\HarddiskVolume1\project', r'\\?\GLOBALROOT\Device\HarddiskVolume1\project'),
    (r'\\.\pipe\catalog', r'\\.\pipe\catalog'),
])
def test_drive_unc_and_device_namespace_normalization(value, expected):
    assert _ordinary_windows_path(value) == expected


def verbatim(path: Path) -> Path:
    raw = str(path.absolute())
    return Path('\\\\?\\UNC\\' + raw[2:] if raw.startswith('\\\\') else '\\\\?\\' + raw)


@pytest.mark.skipif(os.name != 'nt', reason='requires real Windows path resolution')
@pytest.mark.parametrize(('workspace_extended', 'root_extended', 'input_extended', 'cwd_extended'),
                         list(itertools.product((False, True), repeat=4)))
def test_real_windows_mixed_spellings_have_repo_scope_and_accept_skill(tmp_path, workspace_extended, root_extended, input_extended, cwd_extended):
    workspace = tmp_path / 'project'
    root = workspace / '.claude' / 'skills'
    skill = root / 'review' / 'SKILL.md'
    skill.parent.mkdir(parents=True)
    skill.write_text('---\nname: review\ndescription: Review changes\n---\nRead the diff.\n', encoding='utf-8')
    cwd = workspace / 'src'
    cwd.mkdir()
    manager = ManagedSkills(Settings(verbatim(workspace) if workspace_extended else workspace, tmp_path / 'state',
        managed_skills_roots=(verbatim(root) if root_extended else root,)))
    request_cwd = verbatim(cwd) if cwd_extended else cwd
    catalog = manager.list({'cwds': [str(request_cwd)]})['data'][0]
    assert catalog['cwd'] == str(cwd)
    assert catalog['errors'] == []
    assert len(catalog['skills']) == 1
    assert catalog['skills'][0]['scope'] == 'repo'
    assert catalog['skills'][0]['path'] == str(skill)
    prompt, display = manager.resolve({'type': 'skill', 'name': 'review',
        'path': str(verbatim(skill) if input_extended else skill)}, request_cwd)
    assert 'Read the diff.' in prompt
    assert display['path'] == str(skill)
    assert '\\\\?\\' not in prompt
    # The ordinary input must also resolve when both host settings were verbatim.
    assert manager.resolve({'type': 'skill', 'name': 'review', 'path': str(skill)}, cwd)[1] == display


@pytest.mark.skipif(os.name != 'nt', reason='requires real Windows path resolution')
def test_mixed_spellings_dedupe_roots_and_still_reject_outside_cwd_and_skill(tmp_path):
    workspace = tmp_path / 'workspace'
    root = workspace / 'skills'
    inside = root / 'inside' / 'SKILL.md'
    inside.parent.mkdir(parents=True)
    inside.write_text('inside skill', encoding='utf-8')
    outside = tmp_path / 'outside'
    outside.mkdir()
    secret = outside / 'SKILL.md'
    secret.write_text('private content', encoding='utf-8')
    manager = ManagedSkills(Settings(verbatim(workspace), tmp_path / 'state', managed_skills_roots=(root, verbatim(root))))
    assert len(manager.roots) == 1 and len(manager.list({})['data'][0]['skills']) == 1
    for path in (outside, verbatim(outside)):
        with pytest.raises(RpcError, match='outside'):
            manager.list({'cwds': [str(path)]})
    for path in (secret, verbatim(secret)):
        with pytest.raises(RpcError, match='managed catalog'):
            manager.resolve({'type': 'skill', 'name': 'outside', 'path': str(path)})


@pytest.mark.skipif(os.name != 'nt', reason='requires real Windows link resolution')
def test_verbatim_link_escape_is_rejected_after_realpath(tmp_path):
    workspace = tmp_path / 'workspace'
    root = workspace / 'skills'
    root.mkdir(parents=True)
    outside = tmp_path / 'outside'
    outside.mkdir()
    (outside / 'SKILL.md').write_text('private content', encoding='utf-8')
    link = root / 'escaped'
    try:
        link.symlink_to(outside, target_is_directory=True)
    except OSError:
        # Junctions need no symlink privilege and exercise the same realpath
        # escape check on ordinary Windows validation hosts.
        subprocess.run(['cmd', '/c', 'mklink', '/J', str(link), str(outside)],
                       check=True, capture_output=True)
    manager = ManagedSkills(Settings(verbatim(workspace), tmp_path / 'state', managed_skills_roots=(root,)))
    catalog = manager.list({})['data'][0]
    assert catalog['skills'] == [] and 'escapes' in catalog['errors'][0]['message']
    with pytest.raises(RpcError, match='managed catalog'):
        manager.resolve({'type': 'skill', 'name': 'outside', 'path': str(verbatim(link / 'SKILL.md'))})
