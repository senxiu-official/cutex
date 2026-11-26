import unittest
from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock

from cute_claude.runtime_diagnostics import RuntimeDiagnostics
from cute_claude.runtime_tasks import RuntimeTasks
from cute_claude.sdk_runtime import Control, RuntimeOwner
from cute_claude.subscription import SdkResponseError


class Engine:
    def __init__(self):
        self.events = []

    async def emit(self, thread, method, params, *, durable=True):
        self.events.append((thread, method, params, durable))


class RuntimeDiagnosticsTests(unittest.IsolatedAsyncioTestCase):
    async def test_hooks_are_correlated_and_only_display_facts_are_published(self):
        engine = Engine()
        diagnostics = RuntimeDiagnostics(engine, 'root')
        start = SimpleNamespace(subtype='hook_started', data={
            'hook_id': 'private-id', 'hook_event': 'PreToolUse', 'hook_name': 'private-name',
        })
        self.assertTrue(await diagnostics.settings_or_hook(start))
        self.assertFalse(await diagnostics.settings_or_hook(start))
        self.assertFalse(await diagnostics.settings_or_hook({'subtype': 'hook_response', 'hook_id': 'other'}))
        self.assertTrue(await diagnostics.settings_or_hook({
            'subtype': 'hook_response', 'hook_id': 'private-id', 'stdout': 'private-output',
        }))
        self.assertEqual(len(engine.events), 2)
        self.assertTrue(all(not event[3] for event in engine.events))
        active = engine.events[0][2]['diagnostics']['activeHooks']
        self.assertEqual(list(active[0]), ['event', 'startedAtMs'])
        self.assertEqual(active[0]['event'], 'PreToolUse')
        self.assertEqual(diagnostics.snapshot()['activeHooks'], [])

    async def test_result_and_interrupt_clear_hooks_without_clearing_startup_warnings(self):
        owner = RuntimeOwner.__new__(RuntimeOwner)
        owner.engine = Engine()
        owner.engine.retry_blocked = set()
        owner.tid, owner.session_id, owner.connection_id = 'root', 'native', 'connection'
        owner.active = None
        owner.recovery_arrivals = 0
        owner.diagnostics = RuntimeDiagnostics(owner.engine, owner.tid)
        owner.tasks = SimpleNamespace(observe=AsyncMock(return_value=False))
        owner._observe_autonomous = Mock()
        owner._publish_notification_hold = AsyncMock()

        await owner._event({'type': 'system', 'subtype': 'informational', 'tag': 'settings_not_in_effect'})
        start_hook = {'type': 'system', 'subtype': 'hook_started', 'hook_id': 'lost-response',
                      'hook_event': 'PreToolUse'}
        empty = {'warnings': ['configurationNotApplied'], 'activeHooks': []}
        await owner._event(start_hook)
        await owner._event({'type': 'result', 'session_id': 'native'})
        self.assertEqual(owner.diagnostics.snapshot(), empty)
        self.assertEqual(owner.engine.events[-2][2]['diagnostics'], empty)

        owner.active = SimpleNamespace(turn_id='turn', interrupted=False)
        owner.deferred = []
        owner.interrupt_waiters = []
        owner.bridge = SimpleNamespace(revoke=Mock(), cancel_tools=AsyncMock())
        owner.client = SimpleNamespace(interrupt=AsyncMock())
        owner.driver = SimpleNamespace(control_timeout=1, interrupt_timeout=1)
        owner.status = AsyncMock()
        await owner._event(start_hook)
        self.assertTrue(await owner._command(Control('interrupt', turn_id='turn')))
        owner.client.interrupt.assert_awaited_once()
        self.assertEqual(owner.engine.events[-1][2]['diagnostics'], empty)
        await owner._event({**start_hook, 'hook_id': 'late'})
        self.assertEqual(owner.diagnostics.snapshot(), empty)

    async def test_structured_policy_and_mcp_warning_allowlist(self):
        engine = Engine()
        diagnostics = RuntimeDiagnostics(engine, 'root')
        await diagnostics.settings_or_hook({'subtype': 'informational', 'tag': 'settings_not_in_effect',
                                           'content': 'private filesystem path'})
        await diagnostics.settings_or_hook({'subtype': 'informational', 'tag': 'managed_hooks_not_loaded'})
        await diagnostics.settings_or_hook({'subtype': 'informational', 'tag': 'unknown'})
        await diagnostics.settings_or_hook({'subtype': 'init', 'mcp_servers': [
            {'status': 'needs-auth', 'name': 'private name'},
            {'status': 'failed', 'url': 'private URL'},
        ]})
        self.assertEqual(diagnostics.snapshot()['warnings'], [
            'configurationNotApplied', 'managedHooksNotLoaded', 'mcpAuthenticationRequired', 'mcpConnectionFailed',
        ])
        self.assertNotIn('private', str(engine.events))
        self.assertEqual(len(engine.events), 3)
        await diagnostics.clear()
        self.assertEqual(engine.events[-1][2]['diagnostics'], {'warnings': [], 'activeHooks': []})

    async def test_background_denial_is_not_a_permission_request_or_task_completion(self):
        engine = Engine()
        tasks = RuntimeTasks(engine, 'thread', 'native')
        tasks.rows.update(
            child={'taskId': 'child', 'status': 'running', 'background': True},
            ended={'taskId': 'ended', 'status': 'completed', 'background': True},
            parent={'taskId': 'parent', 'status': 'running', 'background': False},
        )
        for task_id in ('missing', 'ended', 'parent', 'child', 'child'):
            await tasks.approval_denied(task_id)
        self.assertEqual(len(engine.events), 1)
        task = engine.events[0][2]['task']
        self.assertEqual(task['approvalState'], 'deniedUnattended')
        self.assertEqual(task['status'], 'running')
        self.assertEqual(tasks.snapshot()['tasks'][0], task)

    def test_unreachable_diagnostic_keeps_provider_text_out(self):
        error = SdkResponseError({'error': 'server_error', 'content': [
            {'text': 'API Error: Connection refused at private.example (ECONNREFUSED)'},
        ]}, None)
        self.assertIn('"diagnosticCode":"unreachable"', error.additional_details)
        self.assertNotIn('private.example', str(error) + error.additional_details)
