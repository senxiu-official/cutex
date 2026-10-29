"""OFFLINE public-SDK-shaped double + real AnyIO scope + real fixture processes.

This is deliberately NOT an import of the official SDK. Tests of the installed
SDK remain separately marked and may skip. No credentials or network are used.
"""
from __future__ import annotations
import asyncio
from dataclasses import make_dataclass
import json
from pathlib import Path
import sys
from types import SimpleNamespace
import anyio

END = object()


async def materialize(prompt):
    """(prompt text, replayable prompt) for a string or one streamed user message."""
    if isinstance(prompt, str):
        return prompt, prompt
    messages = [message async for message in prompt]
    content = messages[0]['message']['content']
    text = content if isinstance(content, str) else '\n'.join(b['text'] for b in content if b['type'] == 'text')
    async def replay():
        for message in messages:
            yield message
    return text, replay()


def make_sdk():
    fields = ['cli_path','cwd','model','tools','disallowed_tools','allowed_tools','permission_mode','mcp_servers',
              'strict_mcp_config','setting_sources','plugins','can_use_tool','hooks','system_prompt',
              'extra_args','effort','thinking','include_partial_messages','max_turns','session_id','resume','continue_conversation','env','stderr','settings',
              'enable_file_checkpointing','forward_subagent_text','max_buffer_size']
    Options = make_dataclass('ClaudeAgentOptions', [(n, object, None) for n in fields])
    clients = []

    class Client:
        def __init__(self, options):
            self.options = options
            self.events = asyncio.Queue()
            self.owner_task = self.reader_task = None
            self.readers = self.query_count = self.interrupt_count = self.disconnect_count = 0
            self.messages = []
            self.controls = []
            self.drop_interrupt = False
            self.connected = asyncio.Event()
            self._transport = SimpleNamespace(_process=None)
            self.group = None
            # Raw control requests (`apply_flag_settings`, as Claude Code 2.1.301 applies them: merged into
            # the flag settings, effort / fast mode / env from the next model request).
            self.flag_settings = {}
            self._query = SimpleNamespace(_send_control_request=self._control_request)
            clients.append(self)

        async def _control_request(self, request):
            self.controls.append(('control', request))
            if request.get('subtype') == 'apply_flag_settings':
                settings = request['settings']
                env = {**self.flag_settings.get('env', {}), **settings.get('env', {})}
                self.flag_settings.update(settings)
                if env:
                    self.flag_settings['env'] = env
                if 'effortLevel' in settings:
                    self.options.effort = settings['effortLevel']
            return None

        async def set_permission_mode(self, mode):
            self.options.permission_mode = mode

        async def set_model(self, model):
            self.options.model = model

        async def get_mcp_status(self):
            assert asyncio.current_task() is self.owner_task
            self.controls.append(('mcp_status',))
            return {'mcpServers': [{'name': 'cutex', 'status': 'connected', 'config': {'type': 'sdk'}}]}

        async def reconnect_mcp_server(self, server_name):
            assert asyncio.current_task() is self.owner_task
            self.controls.append(('mcp_reconnect', server_name))

        async def toggle_mcp_server(self, server_name, enabled):
            assert asyncio.current_task() is self.owner_task
            self.controls.append(('mcp_toggle', server_name, enabled))

        async def stop_task(self, task_id):
            assert asyncio.current_task() is self.owner_task
            self.controls.append(('task_stop', task_id))

        async def rewind_files(self, user_message_id):
            assert asyncio.current_task() is self.owner_task
            self.controls.append(('rewind_files', user_message_id))

        async def get_server_info(self):
            return {'models': [{'value': m, 'displayName': m.capitalize(), 'supportedEffortLevels': [] if m == 'haiku' else ['low','medium','high','xhigh','max']}
                               for m in ['sonnet', 'opus', 'haiku', 'fable', 'future-model']]}

        async def connect(self):
            self.owner_task = asyncio.current_task()
            self.group = anyio.create_task_group()
            await self.group.__aenter__()  # real cancel-scope ownership check
            sid = self.options.resume or self.options.session_id
            script = Path(__file__).parent/'fixtures/native_process.py'
            self._transport._process = await asyncio.create_subprocess_exec(
                sys.executable, str(script), sid, self.options.env['CLAUDE_CONFIG_DIR'],
                'yes' if self.options.resume else 'no',
                stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL)
            self.group.start_soon(self._pump)
            self.connected.set()

        async def _pump(self):
            while True:
                raw = await self._transport._process.stdout.readline()
                if not raw:
                    await self.events.put(END)
                    return
                message = json.loads(raw)
                if message.get('fixture_tool'):
                    self.group.start_soon(self._tool, message)
                else:
                    await self.events.put(message)

        async def _tool(self, message):
            tool = next(t for t in self.options.mcp_servers['cutex']['tools'] if t.name == message['name'])
            hook = self.options.hooks['PreToolUse'][0].hooks[0]
            data = {'session_id': message['session_id'], 'tool_name': 'mcp__cutex__'+message['name'],
                    'tool_input': {'arguments': message['args']}}
            permit = await hook(data, message['call_id'], {})
            if permit['hookSpecificOutput'].get('permissionDecision') == 'deny':
                return
            try:
                result = await tool.handler(permit['hookSpecificOutput']['updatedInput'])
            except asyncio.CancelledError:
                return
            await self.write({'type':'tool_result','call_id':message['call_id'],
                              'text': result['content'][0]['text']})

        async def write(self, value):
            p = self._transport._process
            p.stdin.write((json.dumps(value)+'\n').encode())
            await p.stdin.drain()

        async def query(self, prompt):
            assert asyncio.current_task() is self.owner_task
            self.query_count += 1
            if not isinstance(prompt, str):
                # Streamed user messages, as the adapter sends them.
                messages = [message async for message in prompt]
                assert len(messages) == 1 and messages[0]['type'] == 'user'
                self.messages.append(messages[0])
                content = messages[0]['message']['content']
                prompt = content if isinstance(content, str) else '\n'.join(
                    block['text'] for block in content if block['type'] == 'text')
            await self.write({'type':'query','prompt':prompt})

        async def receive_messages(self):
            assert self.readers == 0, 'More than one SDK consumer'
            self.readers += 1
            self.reader_task = asyncio.current_task()
            try:
                while True:
                    item = await self.events.get()
                    if item is END: return
                    yield item
            finally:
                self.readers -= 1

        async def interrupt(self):
            assert asyncio.current_task() is self.owner_task
            self.interrupt_count += 1
            await self.write({'type':'interrupt','drop':self.drop_interrupt})

        async def disconnect(self):
            assert asyncio.current_task() is self.owner_task
            self.disconnect_count += 1
            if self.group:
                self.group.cancel_scope.cancel()
                await self.group.__aexit__(None, None, None)
                self.group = None
            p = self._transport._process
            if p:
                if p.stdin: p.stdin.close()
                try:
                    await asyncio.wait_for(p.wait(), 2)
                except TimeoutError:
                    p.kill(); await p.wait()

    return SimpleNamespace(ClaudeAgentOptions=Options, SdkMcpTool=lambda **kw: SimpleNamespace(**kw),
            create_sdk_mcp_server=lambda **kw: kw, HookMatcher=lambda **kw: SimpleNamespace(**kw),
            PermissionResultDeny=lambda **kw: SimpleNamespace(behavior='deny', **kw), PermissionResultAllow=lambda **kw: SimpleNamespace(behavior='allow', **kw), ClaudeSDKClient=Client,
            PermissionUpdate=lambda **kw: SimpleNamespace(**kw), PermissionRuleValue=lambda tool_name, rule_content=None: SimpleNamespace(tool_name=tool_name, rule_content=rule_content),
            clients=clients)


def install_fake_sdk(monkeypatch, tmp_path):
    import cute_claude.subscription as sub
    import cute_claude.subscription_auth as auth
    sdk = make_sdk()
    real_import = sub.importlib.import_module
    monkeypatch.setattr(sub.importlib, 'import_module',
        lambda name, package=None: sdk if name == 'claude_agent_sdk' else real_import(name, package))
    # Records any `claude auth status` the driver would run. Starting a runtime
    # or a turn must not run one (it races the SDK process's OAuth refresh lock).
    logins = []
    async def login(cli, config_dir=None):
        logins.append((cli, config_dir))
        return {'loggedIn': True}
    monkeypatch.setattr(auth, 'check_official_login', login)
    driver = sub.OfficialSdkDriver(Path('/fixture/not-official-claude'), tmp_path/'account', interrupt_timeout=2.0)
    return driver, sdk, logins
