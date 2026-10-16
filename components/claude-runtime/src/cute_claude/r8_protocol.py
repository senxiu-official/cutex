"""Additional native v2 routes; SDK-only state stays behind the runtime owner."""
from .errors import RpcError, require, text
from .session_ops import SessionOperations


class R8Protocol:
    def __init__(self, server):
        self.server = server
        self.sessions = SessionOperations(server.engine)

    async def dispatch(self, peer, method, params):
        server, engine = self.server, self.server.engine
        subscription = server.settings.backend == 'subscription'
        structural = {'thread/fork', 'claude/thread/fork', 'thread/revert', 'claude/thread/revert', 'thread/delete'}
        if method in structural:
            feature = 'fork' if method.endswith('/fork') else ('historyRevert' if method.endswith('/revert') else 'threadDelete')
            if not subscription:
                raise RpcError(-32601, 'Native session operations require the subscription backend', {
                    'unsupported': True, 'runtime': 'claude', 'feature': feature, 'method': method})
            # This is a host-owned routing policy, not clientInfo authentication.
            # Direct TUI operations cannot update Cutex's frozen native binding.
            managed = getattr(server.settings, 'external_owner_id', None) is not None
            if managed and (method.startswith('thread/') or
                            (method == 'claude/thread/fork' and not params.get('targetStateDir'))):
                raise RpcError(-32601, 'Managed session history must be changed through Cutex', {
                    'unsupported': True, 'runtime': 'claude', 'feature': feature, 'method': method,
                    'reason': 'cutexManagedSession',
                    'hint': 'Use the Cutex desktop or cutex session management entry point; direct TUI history operations cannot update the host binding.'})
            tid = text(params.get('threadId'), 'threadId', limit=256)
            if method.endswith('/fork'):
                require(set(params) <= {'threadId', 'lastTurnId', 'beforeTurnId', 'excludeTurns', 'name',
                                        'targetThreadId', 'targetStateDir'}, 'unsupported fork fields')
                require(type(params.get('excludeTurns', False)) is bool, 'excludeTurns must be a boolean')
                require(method == 'claude/thread/fork' or not any(key in params for key in ('targetThreadId', 'targetStateDir')),
                        'cross-state export requires claude/thread/fork')
                facts = await self.sessions.fork(tid, params)
                if params.get('targetStateDir') is not None:
                    return facts, None
                new_id = facts['threadId']
                peer.subscriptions.add(new_id)
                result = {**server._thread_response(new_id, not params.get('excludeTurns', False)), **facts}
                return result, lambda: engine.emit(new_id, 'thread/started', {'thread': server._thread_wire(new_id)})
            if method.endswith('/revert'):
                require(set(params) <= {'threadId', 'beforeTurnId', 'rewindFiles'}, 'unsupported revert fields')
                require(type(params.get('rewindFiles', False)) is bool, 'rewindFiles must be a boolean')
                facts = await self.sessions.revert(tid, text(params.get('beforeTurnId'), 'beforeTurnId', limit=256),
                                                  rewind_files=params.get('rewindFiles', False))
                thread = server._thread_wire(tid)
                result = {**facts, 'thread': thread, 'turnsBackwardsCursor': None, 'itemsBackwardsCursor': None}
                return result, lambda: engine.emit(tid, 'thread/reverted', {'threadId': tid})
            result = await self.sessions.delete(tid)
            for client in server.peers:
                client.subscriptions.discard(tid)
            return result, None
        if method == 'thread/metadata/update':
            tid = text(params.get('threadId'), 'threadId', limit=256)
            self.sessions.metadata(tid, params)
            return {'thread': server._thread_wire(tid)}, None
        if method == 'thread/search':
            return self.sessions.search(params, wire=server._thread_wire), None
        if method == 'thread/searchOccurrences':
            tid = text(params.get('threadId'), 'threadId', limit=256)
            return self.sessions.search_occurrences(tid, params), None
        if method == 'mcpServerStatus/list':
            return await self.mcp_status(peer, params), None
        if method in {'claude/mcp/reconnect', 'claude/mcp/toggle', 'mcpServer/reconnect', 'mcpServer/toggle'}:
            require(subscription, 'MCP runtime controls require the subscription backend', -32601)
            tid = text(params.get('threadId'), 'threadId', limit=256)
            name = text(params.get('serverName'), 'serverName', limit=256)
            require(name == 'cutex', 'only the managed cutex MCP server is available')
            if method.endswith('/toggle'):
                require(type(params.get('enabled')) is bool, 'enabled must be a boolean')
                await engine.runtime_control(tid, 'mcp_toggle', server_name=name, enabled=params['enabled'])
            else:
                await engine.runtime_control(tid, 'mcp_reconnect', server_name=name)
            return {}, None
        if method in {'claude/tasks/list', 'claude/tasks/stop', 'thread/backgroundTerminals/list',
                      'thread/backgroundTerminals/terminate', 'thread/backgroundTerminals/clean'}:
            require(subscription, 'native task controls require the subscription backend', -32601)
            tid = text(params.get('threadId'), 'threadId', limit=256)
            server.store.thread(tid)
            owner = getattr(engine.driver, 'owners', {}).get(tid)
            tasks = (await engine.runtime_control(tid, 'task_list')) if owner and not owner.task.done() else {
                'tasks': [], 'source': 'observedSdkLifecycle', 'completeInventory': False}
            if method == 'claude/tasks/list':
                return tasks, None
            if method == 'claude/tasks/stop':
                await engine.runtime_control(tid, 'task_stop', task_id=text(params.get('taskId'), 'taskId', limit=256))
                return {'stopRequested': True, 'terminalConfirmed': False}, None
            shells = [t for t in tasks['tasks'] if t.get('taskType') in {'local_bash', 'local_shell', 'bash'}
                      and t['status'] in {'running', 'pending'}]
            if method.endswith('/list'):
                offset, limit = server._cursor(params.get('cursor')), server._limit(params.get('limit'), 50)
                rows = [{'itemId': t.get('itemId') or t.get('toolUseId') or t['taskId'], 'processId': t['taskId'],
                         'command': t.get('command') or t.get('description', ''), 'cwd': t.get('cwd') or str(server.settings.workspace),
                         'osPid': None, 'cpuPercent': None, 'rssKb': None} for t in shells]
                return {'data': rows[offset:offset + limit],
                        'nextCursor': str(offset + limit) if offset + limit < len(rows) else None}, None
            requested = text(params.get('processId'), 'processId', limit=256) if method.endswith('/terminate') else None
            selected = [t for t in shells if requested is None or t['taskId'] == requested]
            for task in selected:
                await engine.runtime_control(tid, 'task_stop', task_id=task['taskId'])
            return ({'terminated': bool(selected)} if requested is not None else {}), None
        return None

    async def mcp_status(self, peer, params):
        server = self.server
        require(params.get('detail') in (None, 'full', 'toolsAndAuthOnly'), 'invalid MCP detail')
        tid = params.get('threadId')
        if tid is not None:
            text(tid, 'threadId', limit=256)
            server.store.thread(tid)
        owners = getattr(getattr(server.engine, 'driver', None), 'owners', {})
        if tid is None:
            active = [key for key in sorted(peer.subscriptions) if key in owners and not owners[key].task.done()]
            tid = active[0] if active else None
        owner = owners.get(tid)
        status, info, catalog = None, None, {}
        if owner and not owner.task.done():
            response = await server.engine.runtime_control(tid, 'mcp_status')
            for row in response.get('mcpServers', []):
                if row.get('name') != 'cutex':
                    continue
                status = {'pending': 'starting', 'needs-auth': 'authenticationRequired'}.get(row.get('status'), row.get('status'))
                if status not in {'connected', 'failed', 'disabled', 'starting', 'authenticationRequired'}:
                    status = None
                raw_info = row.get('serverInfo')
                if isinstance(raw_info, dict) and all(isinstance(raw_info.get(key), str) for key in ('name', 'version')):
                    info = {key: raw_info[key] for key in ('name', 'version')}
                if status == 'connected':
                    for tool in row.get('tools', []):
                        definition = server.engine.tools.tools.get(tool.get('name'))
                        if definition:
                            catalog[definition.name] = {'name': definition.name, 'description': definition.description,
                                'inputSchema': {'type': 'object', 'properties': {'arguments': definition.schema,
                                    '_cutex_invocation': {'type': 'string'}}, 'required': ['arguments'], 'additionalProperties': False}}
        data = [] if params.get('serverName') not in (None, 'cutex') or server.settings.backend != 'subscription' else [{
            'name': 'cutex', 'runtimeStatus': status or 'notStarted', 'pluginId': None, 'httpOrigin': None,
            'serverInfo': info, 'serverCapabilities': None, 'tools': catalog, 'toolsError': None,
            'resources': [], 'resourceTemplates': [], 'authStatus': 'unsupported'}]
        offset, limit = server._cursor(params.get('cursor')), server._limit(params.get('limit'), 100)
        return {'data': data[offset:offset + limit], 'nextCursor': None}
