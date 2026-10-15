"""Checkpoint capture is an explicit profile/CLI choice, never a side effect of edits."""
from dataclasses import replace

import pytest

from cute_claude.__main__ import parser, run
from cute_claude.config import Settings
from test_persistent import ph


@pytest.mark.parametrize('enabled', [False, True])
async def test_sdk_checkpoint_capture_follows_settings(ph, enabled):
    ph.engine.settings = replace(ph.settings, file_checkpointing=enabled)
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    options = ph.sdk.clients[0].options
    assert options.enable_file_checkpointing is enabled
    assert options.extra_args['replay-user-messages'] is None
    assert ph.sdk.clients[0].query_count == 0


def test_defaults_do_not_create_native_file_snapshots(tmp_path):
    settings = Settings(tmp_path, tmp_path / 'state')
    assert settings.file_checkpointing is False
    settings.validate()
    args = parser().parse_args(['--workspace', str(tmp_path), '--mock'])
    assert args.file_checkpointing == 'disabled'
    args = parser().parse_args(['--workspace', str(tmp_path), '--backend', 'subscription',
                               '--file-checkpointing', 'enabled'])
    assert args.file_checkpointing == 'enabled'


async def test_enabled_flag_requires_subscription_before_opening_store(tmp_path):
    state = tmp_path / 'state'
    args = parser().parse_args(['--workspace', str(tmp_path), '--state-dir', str(state),
                               '--mock', '--file-checkpointing', 'enabled'])
    with pytest.raises(ValueError, match='subscription'):
        await run(args)
    assert not state.exists()


@pytest.mark.parametrize('value', [None, 'enabled', 1])
def test_checkpoint_setting_requires_a_boolean(tmp_path, value):
    with pytest.raises(ValueError, match='boolean'):
        Settings(tmp_path, tmp_path / 'state', file_checkpointing=value).validate()
