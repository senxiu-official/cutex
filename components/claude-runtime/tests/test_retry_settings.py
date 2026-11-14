import json

import pytest

from cute_claude.retry_settings import hooks_configured


@pytest.mark.parametrize('settings,expected', [
    ({}, False), ({'hooks': {'Stop': []}}, False),
    ({'permissions': {'allow': ['Read']}}, False),
    ({'hooks': {'Stop': [{'hooks': [{'type': 'command', 'command': 'echo hook'}]}]}}, True),
    ({'disableAllHooks': True, 'hooks': {'Stop': [{'hooks': [{'type': 'command', 'command': 'echo hook'}]}]}}, False),
])
def test_project_hook_presence_only_from_selected_sources(tmp_path, settings, expected):
    project = tmp_path / 'work'
    account = tmp_path / 'account'
    (project / '.claude').mkdir(parents=True)
    account.mkdir()
    (project / '.claude/settings.json').write_text(json.dumps(settings))
    assert hooks_configured(project, account, ('project',), {}) == expected
    assert not hooks_configured(project, account, (), {})
    assert not hooks_configured(project, account, ('project',), {'disableAllHooks': True})


def test_user_hooks_use_the_selected_account_and_unselected_local_settings_are_ignored(tmp_path):
    (tmp_path / '.claude').mkdir()
    account = tmp_path / 'account'
    account.mkdir()
    hook = {'hooks': {'UserPromptSubmit': [{'hooks': [{'type': 'command', 'command': 'echo hook'}]}]}}
    (account / 'settings.json').write_text(json.dumps(hook))
    (tmp_path / '.claude/settings.local.json').write_text(json.dumps(hook))
    assert not hooks_configured(tmp_path, account, ('project',), {})
    assert hooks_configured(tmp_path, account, ('user',), {})


def test_unreadable_loaded_settings_do_not_prove_hooks_absent(tmp_path):
    (tmp_path / '.claude').mkdir()
    (tmp_path / '.claude/settings.json').write_text('{broken')
    assert hooks_configured(tmp_path, tmp_path, ('project',), {})
