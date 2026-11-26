"""Display-only facts from the bound Claude SDK connection."""
import time

from .runtime_tasks import field

METHOD = 'claude/runtime/diagnosticsChanged'
WARNINGS = {
    'settings_not_in_effect': 'configurationNotApplied',
    'managed_hooks_not_loaded': 'managedHooksNotLoaded',
}
HOOK_EVENTS = {
    'PreToolUse', 'PostToolUse', 'PostToolUseFailure', 'PermissionRequest',
    'UserPromptSubmit', 'SessionStart', 'SessionEnd', 'Stop', 'SubagentStart',
    'SubagentStop', 'PreCompact', 'PostCompact', 'Notification', 'Setup',
    'TaskCompleted', 'TeammateIdle', 'ConfigChange', 'InstructionsLoaded',
}
MAX_HOOKS = 32


class RuntimeDiagnostics:
    def __init__(self, engine, tid):
        self.engine, self.tid = engine, tid
        self.warnings = set()
        self.hooks = {}

    def snapshot(self):
        return {'warnings': sorted(self.warnings), 'activeHooks': list(self.hooks.values())}

    async def publish(self):
        await self.engine.emit(self.tid, METHOD, {'threadId': self.tid, 'diagnostics': self.snapshot()},
                               durable=False)

    async def settings_or_hook(self, message):
        subtype = field(message, 'subtype')
        if subtype == 'init':
            servers = field(message, 'mcp_servers', [])
            if isinstance(servers, list):
                status = {row.get('status') for row in servers[:128]
                          if isinstance(row, dict) and isinstance(row.get('status'), str)}
                warnings = set()
                if 'failed' in status:
                    warnings.add('mcpConnectionFailed')
                if 'needs-auth' in status:
                    warnings.add('mcpAuthenticationRequired')
                if warnings - self.warnings:
                    self.warnings.update(warnings)
                    await self.publish()
            return False
        if subtype == 'informational':
            tag = field(message, 'tag')
            warning = WARNINGS.get(tag) if isinstance(tag, str) else None
            if warning and warning not in self.warnings:
                self.warnings.add(warning)
                await self.publish()
            return bool(warning)
        if subtype not in ('hook_started', 'hook_response'):
            return False
        hook_id = field(message, 'hook_id')
        if not isinstance(hook_id, str) or not hook_id or len(hook_id) > 256:
            return False
        if subtype == 'hook_started':
            if hook_id in self.hooks or len(self.hooks) >= MAX_HOOKS:
                return False
            event = field(message, 'hook_event')
            self.hooks[hook_id] = {'event': event if isinstance(event, str) and event in HOOK_EVENTS else 'other',
                                  'startedAtMs': time.time_ns() // 1_000_000}
        elif self.hooks.pop(hook_id, None) is None:
            return False
        await self.publish()
        return True

    async def clear_hooks(self):
        if self.hooks:
            self.hooks.clear()
            await self.publish()

    async def clear(self):
        if self.warnings or self.hooks:
            self.warnings.clear()
            self.hooks.clear()
            await self.publish()
