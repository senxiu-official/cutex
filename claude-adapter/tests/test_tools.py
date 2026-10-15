import asyncio
import hashlib
import json
import os
import sys
from dataclasses import replace

import pytest

from cute_claude.tools import ToolContext, ToolRegistry


def context(h):
    return ToolContext('thread', 'turn', 'call', h.settings.workspace)


async def test_read_and_default_write_exec_disabled(h):
    result = await h.tools.execute('read_file', {'path': 'hello.txt'}, context(h))
    assert result.success and json.loads(result.text)['text'] == 'hello from fixture'
    assert not (await h.tools.execute('write_file', {'path': 'x', 'content': 'x'}, context(h))).success
    assert not (await h.tools.execute('exec', {'argv': ['echo', 'x']}, context(h))).success
    assert not (h.settings.workspace/'x').exists()


async def test_path_traversal_and_symlink_escape(h, tmp_path):
    (tmp_path/'outside').write_text('secret')
    assert not (await h.tools.execute('read_file', {'path': '../outside'}, context(h))).success
    if os.name != 'nt':
        (h.settings.workspace/'escape').symlink_to(tmp_path/'outside')
        assert not (await h.tools.execute('read_file', {'path': 'escape'}, context(h))).success


async def test_state_files_not_exposed_inside_workspace(h):
    tools = ToolRegistry(replace(h.settings, state_dir=h.settings.workspace/'private'))
    (h.settings.workspace/'private').mkdir()
    (h.settings.workspace/'private'/'secret').write_text('secret')
    assert not (await tools.execute('read_file', {'path': 'private/secret'}, context(h))).success
    await tools.close()


async def test_atomic_write_requires_expected_hash_on_existing_file(h):
    tools = ToolRegistry(replace(h.settings, allow_write=True))
    ctx = context(h)
    assert (await tools.execute('write_file', {'path': 'new.txt', 'content': 'first'}, ctx)).success
    assert not (await tools.execute('write_file', {'path': 'new.txt', 'content': 'second'}, ctx)).success
    digest = hashlib.sha256(b'first').hexdigest()
    assert (await tools.execute('write_file', {'path': 'new.txt', 'content': 'second', 'expected_sha256': digest}, ctx)).success
    assert (h.settings.workspace/'new.txt').read_text() == 'second'
    await tools.close()


async def test_invalid_tool_schema_arguments(h):
    result = await h.tools.execute('read_file', {'path': 123, 'extra': 'x'}, context(h))
    assert not result.success


async def test_exec_explicit_opt_in_and_output_bound(h):
    tools = ToolRegistry(replace(h.settings, allow_exec=True, max_tool_bytes=1024))
    result = await tools.execute('exec', {'argv': [sys.executable, '-c', 'print("x"*5000)']}, context(h))
    assert result.success
    value = json.loads(result.text) if not result.text.endswith('[tool output truncated]') else None
    if value:
        assert value['exitCode'] == 0 and value['truncated']
    assert len(result.text) < 1100
    await tools.close()


async def test_exec_timeout_returns_uncertainty(h):
    tools = ToolRegistry(replace(h.settings, allow_exec=True, tool_timeout=0.05))
    result = await tools.execute('exec', {'argv': [sys.executable, '-c', 'import time;time.sleep(10)']}, context(h))
    assert not result.success and 'uncertain' in result.text
    await tools.close()
