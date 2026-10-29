"""Offline contract tests. Fake SDK messages are NOT a live Claude login test."""
from __future__ import annotations

import asyncio
import copy
from dataclasses import dataclass, make_dataclass
import json
import os
from pathlib import Path
import subprocess
import sys
from types import SimpleNamespace

import pytest
import pytest_asyncio

from cute_claude.config import Settings
from cute_claude.protocol import Server
from cute_claude.store import Store, dump, identifier
from cute_claude.subscription import CALL_FIELD, OfficialSdkDriver, SdkToolBridge, SubscriptionEngine
from cute_claude.subscription_auth import (
    CONFLICTING_ENV, SubscriptionSetupError, check_official_login,
    check_subscription_environment, official_cli, validate_auth_status,
)
from cute_claude.tools import ToolRegistry
from conftest import Harness


class ScriptedDriver:
    """Only the official runtime is faked. Protocol/store/queue/tools are real."""
    def __init__(self, behavior=None):
        self.requests = []
        self.behavior = behavior
        self.closed = False

    async def close(self):
        self.closed = True

    async def run(self, **kw):
        engine = kw['engine']
        self.requests.append({k: v for k, v in kw.items() if k not in {'engine', 'on_message'}})
        engine.store.db.execute('UPDATE sdk_sessions SET attempted=1 WHERE thread_id=?', (kw['tid'],))
        if self.behavior:
            await self.behavior(**kw)
        else:
            await emit_reply(kw['on_message'], kw['session_id'], '[FAKE SDK] ' + kw['prompt'])


async def emit_reply(emit, sid, text, **terminal_overrides):
    await emit({'type': 'system', 'subtype': 'init', 'data': {'session_id': sid}})
    await emit({'type': 'stream_event', 'session_id': sid,
                'event': {'type': 'content_block_start', 'index': 0, 'content_block': {'type': 'text', 'text': ''}}})
    for chunk in (text[:5], text[5:]):
        await emit({'type': 'stream_event', 'session_id': sid,
                    'event': {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'text_delta', 'text': chunk}}})
    await emit({'type': 'assistant', 'content': [{'type': 'text', 'text': text}]})
    await emit({'type': 'result', 'session_id': sid, 'subtype': 'success', 'is_error': False,
                'terminal_reason': 'completed', 'num_turns': 1, 'usage': {'input_tokens': 3, 'output_tokens': 7}, **terminal_overrides})


class SubHarness(Harness):
    def __init__(self, settings, driver=None):
        self.settings = settings
        self.store = Store(settings.state_dir)
        self.provider = driver or ScriptedDriver()
        self.tools = ToolRegistry(settings)
        self.engine = SubscriptionEngine(settings, self.store, self.provider, self.tools)
        self.server = Server(settings, self.store, self.engine)
        self.out, self.next_id = [], 0
        self.peer = self.server.connect(self.send)


@pytest_asyncio.fixture
async def sh(tmp_path):
    work = tmp_path / 'work'
    work.mkdir()
    (work / 'hello.txt').write_text('subscription tool fixture')
    h = SubHarness(Settings(work, tmp_path/'state', mock=False, backend='subscription', model='sonnet'))
    await h.init()
    yield h
    await h.close()


async def test_subscription_capabilities_admit_closed_runtime(sh):
    tid = await sh.thread()
    caps = await sh.call('claude/capabilities')
    assert caps['cliDependency'] is True
    assert caps['officialRuntimeDependency'] is True
    assert caps['officialClaudeCode'] is False  # Adapter is not the official product.
    assert caps['apiKeyRequired'] is False
    assert caps['extraChargesDisabled'] is False
    assert caps['tools']['mcp'] and not caps['tools']['externalMcpServerRequired']
    assert caps['input']['policy'] == 'fifo-separate-turns'
    inspect = await sh.call('claude/thread/inspect', {'threadId': tid})
    assert inspect['localHistoryScope'] == 'audit-projection'
    assert inspect['nativeSession'] is None
    await sh.call('thread/resume', {'threadId': tid})
    assert not sh.provider.requests


async def test_native_session_reuse_current_prompt_only(sh):
    tid = await sh.thread()
    t1 = await sh.turn(tid, 'first unique prompt')
    await sh.drain()
    t2 = await sh.turn(tid, 'second unique prompt')
    await sh.drain()
    a, b = sh.provider.requests
    assert a['session_id'] == b['session_id'] != tid
    assert a['resume'] is False and b['resume'] is True
    assert b['prompt'] == 'second unique prompt'
    assert 'first unique prompt' not in b['prompt']
    assert sh.store.turn(t1)['status'] == sh.store.turn(t2)['status'] == 'completed'
    text_items = [i for i in sh.store.items(t2) if i['type'] == 'agentMessage']
    assert len(text_items) == 1 and text_items[0]['phase'] == 'final_answer'
    deltas = ''.join(m['params']['delta'] for m in sh.out if m.get('method') == 'item/agentMessage/delta' and m['params']['turnId'] == t2)
    assert deltas == text_items[0]['text']
    usage = json.loads(sh.store.db.execute('SELECT usage FROM turns WHERE id=?', (t2,)).fetchone()[0])
    assert usage['modelRequests'][0]['kind'] == 'sdk-turn' and not usage['mock']


async def test_same_workspace_distinct_agents_get_distinct_native_sessions(sh):
    a, b = await sh.thread(), await sh.thread()
    await sh.turn(a, 'A')
    await sh.turn(b, 'B')
    await sh.drain()
    assert sh.engine.session(a)['session_id'] != sh.engine.session(b)['session_id']


async def test_fifo_service_ingress_and_dedup_in_subscription(sh):
    tid = await sh.thread()
    turns = []
    for n in range(3):
        p = {'threadId': tid, 'messageId': f'job-{n}', 'source': {'kind': 'service', 'id': 'job-service'},
             'eventType': 'job.completed', 'text': f'result {n}', 'metadata': {'private': 'not-in-prompt'}}
        r = await sh.call('claude/input/submit', p)
        repeat = await sh.call('claude/input/submit', p)
        assert repeat['duplicate'] and r['turnId'] == repeat['turnId']
        assert r['receiptScope'] == 'local-inbox' and not r['sourceVerified']
        turns.append(r['turnId'])
    await sh.drain()
    assert [r['turn_id'] for r in sh.provider.requests] == turns
    for n, r in enumerate(sh.provider.requests):
        assert 'EXTERNAL_INPUT_JSON' in r['prompt'] and f'result {n}' in r['prompt']
        assert 'not-in-prompt' not in r['prompt']


@pytest.mark.parametrize('terminal', [
    {'is_error': True}, {'subtype': 'error_max_turns'}, {'terminal_reason': 'api_error'},
    {'terminal_reason': 'aborted_tools'}, {'session_id': 'wrong-session'},
])
async def test_terminal_failure_is_not_success_or_automatic_retry(sh, terminal):
    async def behavior(**kw):
        await emit_reply(kw['on_message'], kw['session_id'], 'partial answer', **terminal)
    sh.provider.behavior = behavior
    tid = await sh.thread()
    turn = await sh.turn(tid, 'one attempt only')
    await sh.drain()
    assert sh.store.turn(turn)['status'] == 'failed'
    assert sh.store.thread(tid)['halted']
    assert len(sh.provider.requests) == 1
    await sh.call('thread/resume', {'threadId': tid})
    assert len(sh.provider.requests) == 1


async def test_missing_terminal_fails_without_retry(sh):
    async def behavior(**kw):
        await kw['on_message']({'type': 'assistant', 'content': [{'type': 'text', 'text': 'partial'}]})
    sh.provider.behavior = behavior
    tid = await sh.thread()
    turn = await sh.turn(tid, 'test')
    await sh.drain()
    assert sh.store.turn(turn)['status'] == 'failed'
    assert 'without a terminal' in sh.store.turn(turn)['error']['message']
    assert len(sh.provider.requests) == 1


async def test_foreign_session_stream_rejected(sh):
    async def behavior(**kw):
        await kw['on_message']({'type': 'system', 'data': {'session_id': 'other-agent'}})
    sh.provider.behavior = behavior
    tid = await sh.thread()
    turn = await sh.turn(tid, 'hello')
    await sh.drain()
    assert 'different native session' in sh.store.turn(turn)['error']['message']


async def test_auth_error_is_sanitized(sh):
    async def behavior(**kw):
        raise SubscriptionSetupError('secret account response NOT_FOR_MODEL')
    sh.provider.behavior = behavior
    tid = await sh.thread()
    turn = await sh.turn(tid, 'hello')
    await sh.drain()
    assert sh.store.turn(turn)['status'] == 'failed'
    assert 'NOT_FOR_MODEL' not in dump(sh.out)


async def test_tool_bridge_real_read_and_completed_id_cache(sh):
    count = []
    async def tool(args, context):
        count.append(context.call_id)
        return (context.workspace / args['path']).read_text()
    sh.tools.register('demo_read', 'read', {'type':'object', 'properties':{'path':{'type':'string'}}, 'required':['path']}, tool)
    async def behavior(**kw):
        bridge = SdkToolBridge(kw['engine'], kw['tid'], kw['turn_id'], kw['session_id'])
        data = {'session_id': kw['session_id'], 'tool_name':'mcp__cutex__demo_read', 'tool_input':{'arguments':{'path':'hello.txt'}}}
        results=[]
        for _ in range(2):
            permit = await bridge.pre_tool(data, 'native-call-1', {})
            args = permit['hookSpecificOutput']['updatedInput']
            results.append(await bridge.execute('demo_read', args))
        assert results[0] == results[1] and not results[0]['is_error']
        await emit_reply(kw['on_message'], kw['session_id'], results[0]['content'][0]['text'])
    sh.provider.behavior = behavior
    tid = await sh.thread()
    turn = await sh.turn(tid, 'read')
    await sh.drain()
    assert sh.store.turn(turn)['status'] == 'completed'
    assert count == ['native-call-1']
    calls = sh.store.db.execute('SELECT * FROM tool_calls').fetchall()
    assert len(calls)==1 and calls[0]['state']=='completed'
    assert len([i for i in sh.store.items(turn) if i['type']=='dynamicToolCall'])==1


@pytest.mark.parametrize('invalid', ['native-tool', 'wrong-session', 'missing-id', 'bad-args'])
async def test_pre_tool_invalid_cases_do_not_grant_execution(sh, invalid):
    tid = await sh.thread()
    sid = sh.engine.session(tid)['session_id']
    bridge = SdkToolBridge(sh.engine, tid, 'not-a-turn', sid)
    data = {'session_id':sid,'tool_name':'mcp__cutex__read_file','tool_input':{'arguments':{'path':'hello.txt'}}}
    call_id = 'native-call'
    if invalid=='native-tool': data['tool_name']='Bash'
    if invalid=='wrong-session': data['session_id']='foreign'
    if invalid=='missing-id': call_id=None
    if invalid=='bad-args': data['tool_input']={}
    result = await bridge.pre_tool(data, call_id, {})
    assert result['hookSpecificOutput']['permissionDecision']=='deny'
    assert not bridge.grants


async def test_forged_correlation_and_uncertain_call_rejected(sh):
    tid = await sh.thread()
    sid = sh.engine.session(tid)['session_id']
    bridge = SdkToolBridge(sh.engine, tid, 'not-a-turn', sid)
    r=await bridge.execute('read_file', {'arguments':{'path':'hello.txt'}, CALL_FIELD:'invented'})
    assert r['is_error']
    call={'id':'uncertain-call','name':'read_file','input':{'path':'hello.txt'}}
    sh.store.db.execute("INSERT INTO tool_calls VALUES(?,?,?,'uncertain',?,NULL)", (tid,call['id'],'old-turn',dump(call)))
    data={'session_id':sid,'tool_name':'mcp__cutex__read_file','tool_input':{'arguments':call['input']}}
    r=await bridge.pre_tool(data,call['id'],{})
    assert r['hookSpecificOutput']['permissionDecision']=='deny'


async def test_interrupt_tool_pauses_queue_and_marks_effects_uncertain(sh):
    started=asyncio.Event()
    async def slow(args, ctx):
        (ctx.workspace/'effect.txt').write_text('effect already happened')
        started.set()
        await asyncio.Event().wait()
    sh.tools.register('slow', 'slow', {'type':'object','properties':{}}, slow)
    async def behavior(**kw):
        bridge=SdkToolBridge(kw['engine'],kw['tid'],kw['turn_id'],kw['session_id'])
        data={'session_id':kw['session_id'],'tool_name':'mcp__cutex__slow','tool_input':{'arguments':{}}}
        permit=await bridge.pre_tool(data,'side-effect-call',{})
        await bridge.execute('slow',permit['hookSpecificOutput']['updatedInput'])
    sh.provider.behavior=behavior
    tid=await sh.thread()
    turn=await sh.turn(tid,'slow')
    await asyncio.wait_for(started.wait(),2)
    await sh.call('turn/interrupt',{'threadId':tid,'turnId':turn})
    await sh.drain()
    assert sh.store.turn(turn)['status']=='interrupted'
    assert sh.store.thread(tid)['halted']
    assert sh.store.db.execute('SELECT state FROM tool_calls').fetchone()[0]=='uncertain'
    assert (sh.settings.workspace/'effect.txt').read_text()=='effect already happened'
    assert len(sh.provider.requests)==1


async def test_restart_keeps_native_id_and_does_not_query(sh):
    tid=await sh.thread()
    await sh.turn(tid,'first')
    await sh.drain()
    sid=sh.engine.session(tid)['session_id']
    await sh.engine.close()
    sh.store.close()
    # Replace in-place so fixture finalizer owns exactly one reopened store.
    sh.store=Store(sh.settings.state_dir)
    sh.provider=ScriptedDriver()
    sh.tools=ToolRegistry(sh.settings)
    sh.engine=SubscriptionEngine(sh.settings,sh.store,sh.provider,sh.tools)
    sh.server=Server(sh.settings,sh.store,sh.engine)
    sh.peer=sh.server.connect(sh.send)
    await sh.init()
    await sh.call('thread/resume',{'threadId':tid})
    assert sh.engine.session(tid)['session_id']==sid and not sh.provider.requests
    await sh.turn(tid,'after restart')
    await sh.drain()
    assert sh.provider.requests[0]['resume'] and sh.provider.requests[0]['session_id']==sid


def test_backend_store_binding_refuses_conversion(tmp_path):
    s=Store(tmp_path/'state')
    try:
        s.bind_backend('messages')
        with pytest.raises(ValueError,match='another backend'): s.bind_backend('subscription')
    finally: s.close()
    s=Store(tmp_path/'legacy')
    try:
        s.new_thread('model',str(tmp_path),'')
        with pytest.raises(ValueError,match='legacy state'): s.bind_backend('subscription')
    finally: s.close()


@pytest.mark.parametrize('key',CONFLICTING_ENV)
def test_subscription_rejects_ambiguous_credentials_without_printing_values(key):
    with pytest.raises(SubscriptionSetupError) as e:
        check_subscription_environment({key:'super-secret'})
    assert key in str(e.value) and 'super-secret' not in str(e.value)


def test_subscription_environment_has_no_key_requirement():
    check_subscription_environment({})


@pytest.mark.parametrize('payload',[{}, {'loggedIn':False}, {'loggedIn':True,'authMethod':'api_key'}, {'loggedIn':True,'authMethod':'unrecognized'}])
def test_auth_status_not_guessed(payload):
    with pytest.raises(SubscriptionSetupError): validate_auth_status(payload)


def test_auth_status_minimal_whitelist():
    result=validate_auth_status({'loggedIn':True,'authMethod':'claude.ai','subscriptionType':'team','email':'private@example.com','token':'hidden'})
    assert result['subscriptionType']=='team'
    assert result['entitlementVerified'] is result['extraChargesDisabled'] is False
    assert 'private' not in dump(result) and 'hidden' not in dump(result)


@pytest.mark.skipif(sys.platform == 'win32', reason='executes a #! script / extensionless PATH entry (POSIX only)')
async def test_auth_status_subprocess_exact_command_and_redacted_output(tmp_path,monkeypatch):
    for name in CONFLICTING_ENV: monkeypatch.delenv(name,raising=False)
    cli=tmp_path/'fake-official-cli'
    cli.write_text('#!'+sys.executable+'\nimport sys,json\nassert sys.argv[1:]==["auth","status"]\nprint(json.dumps({"loggedIn":True,"authMethod":"claude.ai","email":"hidden"}))\n')
    cli.chmod(0o700)
    result=await check_official_login(cli)
    assert result['loggedIn'] and 'email' not in result


async def test_sdk_public_driver_option_wiring_and_same_task_ownership(sh, monkeypatch, tmp_path):
    from fake_sdk import install_fake_sdk
    driver, sdk, logins = install_fake_sdk(monkeypatch, tmp_path)
    sh.engine.driver = sh.engine.provider = driver
    tid = await sh.thread()
    turn = await sh.turn(tid, 'TOOL:read_file')
    await sh.drain()
    assert sh.store.turn(turn)['status'] == 'completed'
    assert len(sdk.clients) == 1 and logins == []
    client = sdk.clients[0]
    o = client.options
    assert o.tools == {'type':'preset','preset':'claude_code'} and o.allowed_tools == ['mcp__cutex__read_file','mcp__cutex__list_directory'] and o.strict_mcp_config
    assert o.permission_mode == 'bypassPermissions'
    assert o.extra_args == {'allow-dangerously-skip-permissions':None, 'replay-user-messages':None}
    assert o.enable_file_checkpointing is False and o.forward_subagent_text is True  # subagent_transcripts
    assert o.setting_sources == [] and o.plugins == [] and not o.continue_conversation
    # Claude Code's preset prompt; the host note (no --system-append-file) is appended.
    assert o.system_prompt == {'type': 'preset', 'preset': 'claude_code', 'append': sh.engine.settings.preset_append('')}
    assert o.session_id and o.resume is None
    assert o.env['CLAUDE_CODE_MAX_RETRIES'] == '0'
    assert o.env['CLAUDE_CONFIG_DIR'] == str(tmp_path/'account')
    assert o.env['CLAUDE_CODE_HARBOR_KITE'] == '0'
    assert 'ANTHROPIC_API_KEY' not in o.env
    assert client.disconnect_count == 0
    result = await sh.call('claude/runtime/close', {'threadId': tid})
    assert result['processExitConfirmed'] is True and client.disconnect_count == 1


def test_preset_append_uses_the_launch_file_text_and_thread_instructions(tmp_path):
    from cute_claude.config import RUNTIME_LIMITS
    base = Settings(tmp_path, tmp_path / 'state', model='sonnet', mock=False, backend='subscription', allow_exec=True)
    assert base.preset_append('') == base.system.strip() + '\n\n' + RUNTIME_LIMITS
    assert base.preset_append('Be terse.') == base.system.strip() + '\n\n' + RUNTIME_LIMITS + '\n\nBe terse.'
    cutex = Settings(tmp_path, tmp_path / 'state', model='sonnet', mock=False, backend='subscription', allow_exec=True,
                     system_append='# Cutex\n\nYou are a persistent agent managed by cutex.\n')
    head = '# Cutex\n\nYou are a persistent agent managed by cutex.\n\n' + RUNTIME_LIMITS
    assert cutex.preset_append('') == head
    assert cutex.preset_append('Role.') == head + '\n\nRole.'
    assert Settings(tmp_path, tmp_path, system_append='', allow_exec=True).preset_append('x') == RUNTIME_LIMITS + '\n\nx'


def test_preset_append_tells_the_model_the_runtime_limits(tmp_path):
    # Owner notes (claude-r16): background subagents were refused without warning, and a turn
    # woken by a finished background shell command could not use tools.
    text = Settings(tmp_path, tmp_path / 'state', mock=False, backend='subscription').preset_append('')
    assert 'background subagents' in text and 'run_in_background' in text and 'foreground' in text
    # claude-r23: background subagents are allowed and preferred for long work; the finished
    # notice comes from cutex ("Claude Task"); Claude Code's own notification turn answers in
    # one line without tools.
    assert 'are allowed, and preferred for long work' in text and '"Claude Task"' in text
    assert 'cannot use tools' in text and 'reply in one line and use no tools' in text
    assert 'cannot use cutex tools, start subagents, ask the user, or get approval' in text


def test_preset_append_says_auto_mode_does_not_decide_cutex_approvals(tmp_path):
    # Q21: Claude's auto mode only covers Claude Code's own tool permissions.
    from cute_claude.config import AUTO_MODE_LIMIT
    settings = Settings(tmp_path, tmp_path / 'state', mock=False, backend='subscription', allow_exec=True)
    auto = settings.preset_append('Role.', 'auto')
    assert AUTO_MODE_LIMIT in auto and auto.endswith('\n\nRole.')
    assert "only covers Claude Code's own tool permissions" in auto and 'cutex approvals' in auto
    assert 'managed services' in auto and 'delegation reviews' in auto and 'browser grants' in auto
    assert 'go to the owner' in auto
    for mode in (None, 'default', 'acceptEdits', 'bypassPermissions', 'plan'):
        assert AUTO_MODE_LIMIT not in settings.preset_append('Role.', mode)


async def test_auto_mode_thread_gets_the_cutex_approvals_note(sh, monkeypatch, tmp_path):
    from cute_claude.config import AUTO_MODE_LIMIT
    from fake_sdk import install_fake_sdk
    driver, sdk, logins = install_fake_sdk(monkeypatch, tmp_path)
    sh.engine.driver = sh.engine.provider = driver
    tid = await sh.thread()
    await sh.server.approvals.configure(tid, 'auto')
    turn = await sh.turn(tid, 'TOOL:read_file')
    await sh.drain()
    assert sh.store.turn(turn)['status'] == 'completed'
    append = sdk.clients[0].options.system_prompt['append']
    assert append == sh.engine.settings.preset_append('', 'auto') and AUTO_MODE_LIMIT in append


def test_preset_append_says_shell_tools_are_off_without_command_execution(tmp_path):
    # r243 acceptance: Claude Code's environment block still named "Shell: PowerShell (primary); Bash tool
    # also available" with command execution off.
    off = Settings(tmp_path, tmp_path / 'state', mock=False, backend='subscription').preset_append('')
    assert 'Command execution is off' in off and 'no Bash or PowerShell tool' in off and 'hooks do not run' in off
    on = Settings(tmp_path, tmp_path / 'state', mock=False, backend='subscription', allow_exec=True).preset_append('')
    assert 'Command execution is off' not in on
    from cute_claude.native_tools import exclusions
    assert {'Bash', 'PowerShell'} <= set(exclusions(Settings(tmp_path, tmp_path / 'state')))


def test_system_append_file_requires_the_subscription_backend(tmp_path):
    import subprocess, os
    root = Path(__file__).parents[1]
    env = {**os.environ, 'PYTHONPATH': str(root / 'src')}
    append = tmp_path / 'append.md'
    append.write_text('# Cutex\n', encoding='utf-8')
    p = subprocess.run([sys.executable, '-m', 'cute_claude', '--mock', '--workspace', str(tmp_path),
                        '--state-dir', str(tmp_path / 'state'), '--system-append-file', str(append)],
                       env=env, capture_output=True, text=True, timeout=20)
    assert p.returncode == 2 and '--system-append-file requires --backend subscription' in p.stderr


def test_actual_installed_sdk_public_surface_when_available():
    # Explicitly skipped in the delivered offline validation; no fake success.
    sdk=pytest.importorskip('claude_agent_sdk',reason='official SDK is not installed in this offline validation environment')
    import dataclasses
    fields={f.name for f in dataclasses.fields(sdk.ClaudeAgentOptions)}
    assert {'cli_path','env','stderr','tools','strict_mcp_config','session_id','resume','setting_sources','hooks','settings'} <= fields
    for method in ('connect','query','receive_messages','interrupt','disconnect'):
        assert callable(getattr(sdk.ClaudeSDKClient,method,None))
    assert callable(sdk.create_sdk_mcp_server) and callable(sdk.ClaudeSDKClient)

@pytest.mark.parametrize('extra',[{'apiProvider':'bedrock'}, {'apiKeySource':'ANTHROPIC_API_KEY'}])
def test_auth_status_ambiguous_billing_rejected(extra):
    with pytest.raises(SubscriptionSetupError):
        validate_auth_status({'loggedIn':True,'authMethod':'claude.ai',**extra})


def test_no_api_key_does_not_implicitly_launch_subscription(tmp_path,monkeypatch):
    import subprocess, os
    env={k:v for k,v in os.environ.items() if k!='ANTHROPIC_API_KEY'}
    root=Path(__file__).parents[1]
    env['PYTHONPATH']=str(root/'src')
    p=subprocess.run([sys.executable,'-m','cute_claude','--model','sonnet','--workspace',str(tmp_path),
                     '--state-dir',str(tmp_path/'state')],env=env,capture_output=True,text=True,timeout=10)
    assert p.returncode==2 and '--backend subscription' in p.stderr
    assert not (tmp_path/'state').exists()

def write_api_account(tmp_path, **overrides):
    key_file = tmp_path / 'keys' / 'ca-test.json'
    key_file.parent.mkdir(parents=True, exist_ok=True)
    key_file.write_text(json.dumps({'apiKey': 'sk-test-only-secret'}), encoding='utf-8')
    config = {'version': 1, 'accountId': 'ca-test', 'keyFile': str(key_file), 'keyEnv': 'ANTHROPIC_API_KEY',
              'baseUrl': 'https://api.deepseek.com/anthropic',
              'env': {'ANTHROPIC_DEFAULT_SONNET_MODEL': 'deepseek-v4-pro'}, **overrides}
    path = tmp_path / 'claude-api-account.json'
    path.write_text(json.dumps(config), encoding='utf-8')
    return path


def test_api_account_config_is_loaded_without_exposing_the_key(tmp_path):
    from cute_claude.subscription_auth import load_api_account
    account = load_api_account(write_api_account(tmp_path))
    env = account.environment()
    # The key is in no environment (Bash / hooks / MCP stdio servers inherit the runtime's).
    assert env == {'ANTHROPIC_BASE_URL': 'https://api.deepseek.com/anthropic',
                   'ANTHROPIC_DEFAULT_SONNET_MODEL': 'deepseek-v4-pro'}
    assert 'sk-test-only' not in repr(account) and 'sk-test-only' not in dump(account.status())
    assert account.status()['authMethod'] == 'api_key'
    helper = account.cli_settings()['apiKeyHelper']
    assert 'sk-test-only' not in helper and str((tmp_path / 'keys' / 'ca-test.json').resolve().as_posix()) in helper
    bearer = load_api_account(write_api_account(tmp_path, keyEnv='ANTHROPIC_AUTH_TOKEN', baseUrl=None, env={}))
    assert bearer.environment() == {}


def test_api_key_helper_prints_only_the_key(tmp_path):
    import shlex
    from cute_claude.subscription_auth import load_api_account
    account = load_api_account(write_api_account(tmp_path))
    command = account.cli_settings()['apiKeyHelper']
    env = {k: v for k, v in os.environ.items() if k not in ('ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN')}
    p = subprocess.run(command, shell=True, capture_output=True, text=True, timeout=30, env=env)
    assert p.returncode == 0 and p.stdout == 'sk-test-only-secret' and p.stderr == ''
    # A bad key file fails without echoing its content.
    (tmp_path / 'keys' / 'ca-test.json').write_text('{"apiKey": "has space sk-test-only"}', encoding='utf-8')
    p = subprocess.run(command, shell=True, capture_output=True, text=True, timeout=30, env=env)
    assert p.returncode == 1 and p.stdout == '' and 'sk-test-only' not in p.stderr
    # Paths with spaces are quoted; paths needing shell-specific escaping are refused.
    spaced = tmp_path / 'key dir' / 'k.json'
    spaced.parent.mkdir()
    spaced.write_text('{"apiKey": "sk-test-only-spaced"}', encoding='utf-8')
    from cute_claude.subscription_auth import ApiAccount
    command = ApiAccount('ca', 'ANTHROPIC_API_KEY', None, {}, 'sk-test-only-spaced', spaced).cli_settings()['apiKeyHelper']
    assert shlex.split(command)[-1] == spaced.resolve().as_posix()
    p = subprocess.run(command, shell=True, capture_output=True, text=True, timeout=30, env=env)
    assert p.stdout == 'sk-test-only-spaced'
    with pytest.raises(SubscriptionSetupError):
        ApiAccount('ca', 'ANTHROPIC_API_KEY', None, {}, 'k', tmp_path / 'a$b.json').cli_settings()
    with pytest.raises(SubscriptionSetupError):
        ApiAccount('ca', 'ANTHROPIC_API_KEY', None, {}, 'k').cli_settings()


@pytest.mark.parametrize('overrides', [
    {'version': 2}, {'accountId': ''}, {'keyEnv': 'OPENAI_API_KEY'}, {'baseUrl': 'ftp://x'},
    {'env': {'ANTHROPIC_API_KEY': 'other'}}, {'env': {'CLAUDE_CONFIG_DIR': '/elsewhere'}},
    {'keyFile': '/does/not/exist.json'}, {'keyFile': ''},
])
def test_api_account_config_rejects_unsafe_settings_without_printing_values(tmp_path, overrides):
    from cute_claude.subscription_auth import load_api_account
    with pytest.raises(SubscriptionSetupError) as e:
        load_api_account(write_api_account(tmp_path, **overrides))
    assert 'sk-test-only' not in str(e.value)


async def test_api_account_key_reaches_only_the_official_runtime_env(sh, monkeypatch, tmp_path):
    from fake_sdk import install_fake_sdk
    from cute_claude.subscription_auth import load_api_account
    import cute_claude.subscription as sub
    _, sdk, logins = install_fake_sdk(monkeypatch, tmp_path)
    account = load_api_account(write_api_account(tmp_path))
    driver = sub.OfficialSdkDriver(Path('/fixture/not-official-claude'), tmp_path/'account', interrupt_timeout=2.0,
                                   api_account=account)
    sh.engine.driver = sh.engine.provider = driver
    tid = await sh.thread()
    turn = await sh.turn(tid, 'hello')
    await sh.drain()
    assert sh.store.turn(turn)['status'] == 'completed'
    assert logins == [], 'API accounts do not check a claude.ai login'
    o = sdk.clients[0].options
    assert 'ANTHROPIC_API_KEY' not in o.env and 'ANTHROPIC_AUTH_TOKEN' not in o.env
    assert 'sk-test-only' not in json.dumps(o.env)
    assert json.loads(o.settings) == {**account.cli_settings(), 'disableAllHooks': True}
    assert o.env['ANTHROPIC_BASE_URL'] == 'https://api.deepseek.com/anthropic'
    assert o.env['ANTHROPIC_DEFAULT_SONNET_MODEL'] == 'deepseek-v4-pro'
    assert o.env['CLAUDE_CODE_MAX_RETRIES'] == '3'
    assert o.env['CLAUDE_CONFIG_DIR'] == str(tmp_path/'account') and o.env['CLAUDE_CODE_HARBOR_KITE'] == '0'
    import os
    assert 'ANTHROPIC_API_KEY' not in os.environ, 'the adapter process itself never carries the key'
    await sh.call('claude/runtime/close', {'threadId': tid})


async def test_subscription_start_and_turn_run_no_auth_status(sh, monkeypatch, tmp_path):
    # 2026-10-11: a pre-start `claude auth status` on the same config dir raced the SDK
    # process for Claude Code's OAuth refresh lock after hours idle (expired access token),
    # failing the first turn. Subscription starts / turns spawn no official CLI besides the
    # SDK's own process, which refreshes the token itself; the adapter never refreshes it.
    from fake_sdk import install_fake_sdk
    import cute_claude.subscription as sub
    import cute_claude.subscription_auth as auth
    driver, sdk, logins = install_fake_sdk(monkeypatch, tmp_path)
    assert driver.api_account is None
    real_exec = asyncio.create_subprocess_exec
    async def guarded_exec(program, *args, **kwargs):
        # The fake SDK spawns a stand-in process; any `<cli> auth ...` is the forbidden check.
        if str(program) == str(driver.cli) or 'auth' in args:
            raise AssertionError('no official CLI auth subprocess before or during a subscription turn')
        return await real_exec(program, *args, **kwargs)
    monkeypatch.setattr(auth.asyncio, 'create_subprocess_exec', guarded_exec)
    assert not hasattr(sub, 'check_official_login')
    sh.engine.driver = sh.engine.provider = driver
    tid = await sh.thread()
    await sh.call('claude/runtime/start', {'threadId': tid})
    first = await sh.turn(tid, 'first message after hours idle')
    await sh.drain()
    second = await sh.turn(tid, 'second')
    await sh.drain()
    assert [sh.store.turn(t)['status'] for t in (first, second)] == ['completed', 'completed']
    assert logins == [] and len(sdk.clients) == 1
    assert sdk.clients[0].options.env['CLAUDE_CONFIG_DIR'] == str(tmp_path/'account')
    # A screenshot or large file in one tool result must not end the turn (SDK default 1 MiB).
    assert sdk.clients[0].options.max_buffer_size == 64 * 1024 * 1024
    await sh.call('claude/runtime/close', {'threadId': tid})


def test_api_account_config_requires_the_subscription_backend(tmp_path):
    import subprocess, os
    root = Path(__file__).parents[1]
    env = {**os.environ, 'PYTHONPATH': str(root / 'src')}
    p = subprocess.run([sys.executable, '-m', 'cute_claude', '--mock', '--workspace', str(tmp_path),
                        '--state-dir', str(tmp_path / 'state'), '--api-account-config', str(write_api_account(tmp_path))],
                       env=env, capture_output=True, text=True, timeout=20)
    assert p.returncode == 2 and 'require --backend subscription' in p.stderr
    assert 'sk-test-only' not in p.stderr


def test_features_declare_api_accounts():
    from cute_claude.protocol import FEATURES
    assert FEATURES['features']['apiAccounts'] is True


async def test_fresh_user_turn_after_failure_precedes_queued_input_without_replay(sh):
    async def fail(**kw):
        await emit_reply(kw['on_message'], kw['session_id'], 'partial', subtype='error_max_turns', is_error=True)
    sh.provider.behavior=fail
    tid=await sh.thread()
    old=await sh.turn(tid,'first')
    await sh.drain()
    assert 'error_max_turns' in sh.store.turn(old)['error']['message']
    queued=await sh.call('claude/input/submit',{'threadId':tid,'messageId':'pending-after-failure','source':{'kind':'agent','id':'fixture'},'eventType':'message','text':'later external message'})
    await sh.drain()
    assert len(sh.provider.requests)==1
    invalid=await sh.call('turn/start',{'threadId':tid,'input':[]},error=True)
    assert invalid['code']==-32602 and sh.store.thread(tid)['halted']
    sh.provider.behavior=None
    await sh.call('thread/settings/update',{'threadId':tid,'collaborationMode':{'mode':'default','settings':{}}})
    assert sh.store.thread(tid)['halted']
    reply=await sh.call('turn/start',{'threadId':tid,'input':[{'type':'text','text':'new explicit instruction'}], 'collaborationMode':{'mode':'default','settings':{}}})
    new=reply['turn']['id']
    await sh.drain()
    assert sh.store.turn(old)['status']=='failed'
    assert [r['turn_id'] for r in sh.provider.requests]==[old,new,queued['turnId']]
    assert sh.store.turn(new)['status']=='completed'
    assert not sh.store.thread(tid)['halted']


async def test_final_answer_phase_reaches_live_subscribers(sh):
    tid = await sh.thread()
    turn = await sh.turn(tid, 'answer me')
    await sh.drain()
    live = [m['params']['item'] for m in sh.out if m.get('method') == 'item/completed'
            and m['params'].get('turnId') == turn and m['params']['item']['type'] == 'agentMessage']
    saved = [i for i in sh.store.items(turn) if i['type'] == 'agentMessage']
    assert live[-1] == saved[-1] and saved[-1]['phase'] == 'final_answer'
    assert [i['phase'] for i in live].count('final_answer') == 1


def test_claude_codes_synthetic_placeholder_is_never_an_agent_message():
    # r243 acceptance: after "[Request interrupted by user]" a resumed session got Claude Code's own
    # "No response requested." (model "<synthetic>") in its native transcript.
    from cute_claude.subscription import synthetic_placeholder
    text = lambda t: {'type': 'text', 'text': t}
    assert synthetic_placeholder({'type': 'assistant', 'model': '<synthetic>', 'content': [text('No response requested.')]})
    assert not synthetic_placeholder({'type': 'assistant', 'model': 'claude-opus-5-5', 'content': [text('No response requested.')]})
    assert not synthetic_placeholder({'type': 'assistant', 'model': '<synthetic>', 'content': [text('API Error: overloaded')]})
    assert not synthetic_placeholder({'type': 'assistant', 'model': '<synthetic>', 'content': []})
