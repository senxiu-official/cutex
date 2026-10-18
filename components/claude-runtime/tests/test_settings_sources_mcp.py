"""Claude Code settings sources, host-selected external MCP servers and claude.ai connectors.

All three are explicit launch choices cutex makes per profile (fake SDK, no login)."""
import asyncio
import json
from dataclasses import replace
from types import SimpleNamespace

import pytest

from cute_claude.__main__ import parser, run
from cute_claude.config import Settings, load_mcp_servers_config
from cute_claude.native_tools import exclusions, external_mcp_tool
from test_persistent import ph
from test_questions_plan import begin
from test_native_tools import ask_mode

SERVERS = {
    'docs': {'type': 'stdio', 'command': 'docs-mcp', 'args': ['--stdio'], 'env': {'TOKEN': '${CUTEX_CLAUDE_MCP_1}'}},
    'issues': {'type': 'http', 'url': 'https://mcp.example/issues', 'headers': {'Authorization': 'Bearer ${CUTEX_CLAUDE_MCP_2}'}},
    'issues__v2': {'type': 'http', 'url': 'https://mcp.example/v2'},
}
FILTERS = {'docs': {'enabled': None, 'disabled': ['drop_db']}, 'issues': {'enabled': ['search'], 'disabled': []}}


async def started(ph, **changes):
    ph.engine.settings = replace(ph.settings, **changes)
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    return ph.sdk.clients[-1].options


async def test_defaults_keep_claude_code_settings_and_other_mcp_off(ph):
    options = await started(ph)
    assert options.setting_sources == [] and options.plugins == []
    assert list(options.mcp_servers) == ['cutex'] and options.strict_mcp_config is True
    # Claude Code's own auto memory is off for every Claude agent, settings sources or not.
    assert options.env['CLAUDE_CODE_DISABLE_AUTO_MEMORY'] == '1'


@pytest.mark.parametrize('sources', [(), ('project',), ('project', 'user')])
async def test_exec_off_disables_settings_hooks_and_keeps_the_sdk_callbacks(ph, sources):
    options = await started(ph, setting_sources=sources, allow_exec=False)
    assert json.loads(options.settings) == {'disableAllHooks': True}
    # The adapter's own hooks are SDK callbacks (initialize), not settings hooks: still passed.
    assert {'PreToolUse', 'PostToolUse', 'PostToolUseFailure', 'SubagentStart', 'SubagentStop'} <= set(options.hooks)
    assert options.env['CLAUDE_CODE_DISABLE_AUTO_MEMORY'] == '1'


@pytest.mark.parametrize('sources', [(), ('project', 'user')])
async def test_exec_on_keeps_settings_hooks_as_before(ph, sources):
    options = await started(ph, setting_sources=sources, allow_exec=True)
    assert options.settings is None or 'disableAllHooks' not in json.loads(options.settings)
    assert options.env['CLAUDE_CODE_DISABLE_AUTO_MEMORY'] == '1'


async def test_live_rule_reload_keeps_hooks_off_while_exec_is_off(ph):
    ph.engine.settings = replace(ph.settings, allow_exec=False)
    tid = await ph.thread()
    assert ph.engine.flag_settings_dict(tid, explicit_rules=True) == {'disableAllHooks': True, 'permissions': {'allow': []}}
    ph.engine.settings = replace(ph.settings, allow_exec=True)
    assert ph.engine.flag_settings_dict(tid, explicit_rules=True) == {'permissions': {'allow': []}}


async def test_profile_setting_sources_reach_the_sdk(ph):
    options = await started(ph, setting_sources=('project', 'user'))
    assert options.setting_sources == ['project', 'user'] and options.plugins == []
    # Memory stays cutex's: Claude Code's own auto-memory is off with settings on.
    assert options.env['CLAUDE_CODE_DISABLE_AUTO_MEMORY'] == '1'
    # `user` is the account home (CLAUDE_CONFIG_DIR), never the OS user's ~/.claude.
    assert options.env['CLAUDE_CONFIG_DIR'] == str(ph.engine.driver.config_dir)


async def test_external_servers_join_the_cutex_server_under_strict_config(ph):
    options = await started(ph, mcp_servers=SERVERS, mcp_tool_filters=FILTERS)
    assert set(options.mcp_servers) == {'cutex', 'docs', 'issues', 'issues__v2'}
    assert options.mcp_servers['docs'] == SERVERS['docs'] and options.strict_mcp_config is True
    assert 'mcp__docs__drop_db' in options.disallowed_tools
    # Secrets are references only; the adapter never resolves them.
    assert '${CUTEX_CLAUDE_MCP_2}' in json.dumps(options.mcp_servers['issues'])


@pytest.mark.parametrize('api_account', [False, True])
async def test_connectors_drop_strict_config_for_subscription_logins_only(ph, api_account):
    if api_account:
        ph.engine.driver.api_account = SimpleNamespace(cli_settings=lambda: {}, environment=lambda: {})
    options = await started(ph, claude_ai_connectors=True)
    assert options.strict_mcp_config is api_account


def test_external_tool_names():
    settings = SimpleNamespace(mcp_servers=SERVERS, claude_ai_connectors=False)
    assert external_mcp_tool(settings, 'mcp__docs__search') == ('docs', 'search')
    assert external_mcp_tool(settings, 'mcp__issues__v2__list') == ('issues__v2', 'list')
    assert external_mcp_tool(settings, 'mcp__issues__list') == ('issues', 'list')
    assert external_mcp_tool(settings, 'mcp__cutex__agent_tell') is None
    assert external_mcp_tool(settings, 'mcp__claude_ai_Gmail__search') is None
    assert external_mcp_tool(settings, 'Bash') is None
    on = SimpleNamespace(mcp_servers={}, claude_ai_connectors=True)
    assert external_mcp_tool(on, 'mcp__claude_ai_Gmail__search') == ('claude_ai_Gmail', 'search')


async def test_external_mcp_tool_is_observed_and_approved_like_a_native_tool(ph):
    ph.engine.settings = replace(ph.settings, mcp_servers=SERVERS, mcp_tool_filters=FILTERS)
    tid, turn, b = await begin(ph)
    ask_mode(ph, tid)
    args = {'query': 'release notes'}
    assert await b.pre_tool({'session_id': b.sid, 'tool_name': 'mcp__docs__search', 'tool_input': args}, 'ext-1', {}) == {}
    assert not b.grants  # never executed through the cutex bridge
    item = ph.store.items(turn)[-1]
    assert item['type'] == 'mcpToolCall' and item['server'] == 'docs' and item['tool'] == 'search'
    assert item['status'] == 'inProgress'
    task = asyncio.create_task(b.native.approve('mcp__docs__search', args, SimpleNamespace(tool_use_id='ext-1')))
    try:
        async with asyncio.timeout(2):
            while not ph.server.approvals.pending:
                await asyncio.sleep(.001)
        req = next(x for x in ph.out if x.get('method') == 'item/commandExecution/requestApproval')
        assert req['params']['command'].startswith('mcp__docs__search ')
        await ph.server.handle(ph.peer, {'id': req['id'], 'result': {'decision': 'accept'}})
        assert (await task)['allow']
    finally:
        if not task.done():
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)
    await b.native.post({'session_id': b.sid, 'hook_event_name': 'PostToolUse',
                         'tool_response': [{'type': 'text', 'text': 'found 3'}]}, 'ext-1', {})
    item = ph.store.items(turn)[-1]
    assert item['status'] == 'completed' and item['result']['content'] == [{'type': 'text', 'text': 'found 3'}]


async def test_external_tool_filters_and_unknown_servers_are_denied(ph):
    ph.engine.settings = replace(ph.settings, mcp_servers=SERVERS, mcp_tool_filters=FILTERS)
    tid, turn, b = await begin(ph)
    for name in ['mcp__issues__create', 'mcp__docs__drop_db', 'mcp__elsewhere__x', 'mcp__claude_ai_Gmail__search']:
        denied = await b.pre_tool({'session_id': b.sid, 'tool_name': name, 'tool_input': {}}, name, {})
        assert denied['hookSpecificOutput']['permissionDecision'] == 'deny', name
    assert await b.pre_tool({'session_id': b.sid, 'tool_name': 'mcp__issues__search', 'tool_input': {}}, 'ok', {}) == {}


async def test_failed_external_tool_closes_its_item(ph):
    ph.engine.settings = replace(ph.settings, mcp_servers=SERVERS)
    tid, turn, b = await begin(ph)
    await b.pre_tool({'session_id': b.sid, 'tool_name': 'mcp__issues__search', 'tool_input': {}}, 'ext-f', {})
    await b.native.post({'session_id': b.sid, 'hook_event_name': 'PostToolUseFailure', 'error': 'server down'}, 'ext-f', {})
    item = ph.store.items(turn)[-1]
    assert item['status'] == 'failed' and item['error'] == {'message': 'server down'}


def test_cli_flags_and_config_file(tmp_path):
    config = tmp_path / 'mcp.json'
    config.write_text(json.dumps({'version': 1, 'servers': SERVERS, 'toolFilters': FILTERS}))
    assert load_mcp_servers_config(config) == (SERVERS, FILTERS)
    args = parser().parse_args(['--workspace', str(tmp_path), '--backend', 'subscription',
                               '--setting-sources', 'project,user', '--mcp-servers-config', str(config),
                               '--claude-ai-connectors', 'enabled'])
    assert args.setting_sources == 'project,user' and args.claude_ai_connectors == 'enabled'
    defaults = parser().parse_args(['--workspace', str(tmp_path), '--mock'])
    assert defaults.setting_sources == '' and defaults.claude_ai_connectors == 'disabled' and defaults.mcp_servers_config is None


@pytest.mark.parametrize('servers,filters', [
    ({'cutex': {'command': 'x'}}, {}),
    ({'claude_ai_x': {'command': 'x'}}, {}),
    ({'bad name': {'command': 'x'}}, {}),
    ({'s': {'type': 'sse', 'url': 'https://x'}}, {}),
    ({'s': {'command': 'x', 'cwd': '/tmp'}}, {}),
    ({'s': {'type': 'http', 'url': 'file:///x'}}, {}),
    ({'s': {'command': 'x', 'env': {'A': 1}}}, {}),
    ({'s': {'command': 'x'}}, {'t': {'disabled': []}}),
])
def test_invalid_server_configs_are_refused(tmp_path, servers, filters):
    with pytest.raises(ValueError):
        Settings(tmp_path, tmp_path / 'state', backend='subscription', mock=False,
                 mcp_servers=servers, mcp_tool_filters=filters).validate()


@pytest.mark.parametrize('changes', [
    {'setting_sources': ('project', 'project')}, {'setting_sources': ('local',)},
])
def test_invalid_setting_sources(tmp_path, changes):
    with pytest.raises(ValueError, match='setting_sources'):
        Settings(tmp_path, tmp_path / 'state', backend='subscription', mock=False, **changes).validate()


async def test_options_require_subscription_before_opening_store(tmp_path):
    state = tmp_path / 'state'
    args = parser().parse_args(['--workspace', str(tmp_path), '--state-dir', str(state), '--mock',
                               '--setting-sources', 'project'])
    with pytest.raises(ValueError, match='subscription'):
        await run(args)
    assert not state.exists()


def test_features_declare_the_new_launch_options():
    from pathlib import Path
    features = json.loads((Path(__file__).parents[1] / 'src/cute_claude/features.json').read_text())['features']
    assert features['settingSources'] == ['project', 'user']
    assert features['mcpServers'] is True and features['claudeAiConnectors'] is True
    assert exclusions(SimpleNamespace(allow_write=True, allow_exec=True, mcp_tool_filters={})) == exclusions(
        SimpleNamespace(allow_write=True, allow_exec=True))
