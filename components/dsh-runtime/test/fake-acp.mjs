import { createInterface } from 'node:readline';
const sessionId = '018e0000-0000-7000-8000-0000a000037f';
const send = value => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', ...value }) + '\n');
let prompt, wedged = false;
for await (const line of createInterface({ input: process.stdin })) {
  const m = JSON.parse(line);
  if (wedged) continue;
  if (m.method === 'session/list') { send({ id: m.id, result: { sessions: [] } }); continue; }
  if (!m.method) {
    if (prompt) { send({ id: prompt, result: { stopReason: 'end_turn' } }); prompt = undefined; }
    continue;
  }
  if (m.method === 'initialize') send({ id: m.id, result: { protocolVersion: 1, agentCapabilities: { sessionCapabilities: { resume: {}, close: {} } } } });
  else if (m.method === 'session/new' || m.method === 'session/resume') send({ id: m.id, result: { sessionId } });
  else if (m.method === 'session/close') send({ id: m.id, result: {} });
  else if (m.method === 'session/cancel') { send({ id: prompt, result: { stopReason: 'cancelled' } }); prompt = undefined; }
  else if (m.method === 'session/prompt') {
    const text = m.params.prompt[0].text;
    const update = update => send({ method: 'session/update', params: { sessionId, update } });
    prompt = m.id;
    // Liveness fixtures: silent but alive, wedged (answers nothing), or exits.
    if (text === 'slow') { setTimeout(() => { send({ id: m.id, result: { stopReason: 'end_turn' } }); prompt = undefined; }, 400); continue; }
    if (text === 'hang') { wedged = true; continue; }
    if (text === 'die') process.exit(3);
    if (text === 'wait') { update({ sessionUpdate: 'agent_thought_chunk', messageId: 'thought', content: { type: 'text', text: 'waiting' } }); continue; }
    if (text === 'toolcards') {
      // DSH rc.3's generic ACP tool lifecycle (kind other, rawInput, text content) for mapped tools.
      const tool = (id, title, rawInput, status, result) => {
        update({ sessionUpdate: 'tool_call', toolCallId: id, title, kind: 'other', status: 'in_progress', rawInput });
        update({ sessionUpdate: 'tool_call_update', toolCallId: id, status, content: [{ type: 'content', content: { type: 'text', text: result } }] });
      };
      tool('bash-1', 'bash', { command: 'make test', description: 'Run tests' }, 'completed', 'FAIL one\n[exit code: 2]');
      tool('edit-1', 'edit', { file_path: 'a.txt', old_string: 'x', new_string: 'y' }, 'completed', 'The file /w/a.txt has been updated successfully.');
      tool('todo-1', 'todo_write', { todos: [{ content: 'First', status: 'completed' }, { content: 'Second', status: 'in_progress' }] }, 'completed', 'Updated todo list: 0 pending, 1 in progress, 1 completed.');
      tool('mcp-1', 'mcp__cutex__agent_list', {}, 'completed', '{"ok":true,"agents":[]}');
      tool('other-1', 'skill', { name: 'x' }, 'failed', 'Error: no such skill');
      send({ id: m.id, result: { stopReason: 'end_turn' } }); prompt = undefined;
      continue;
    }
    update({ sessionUpdate: 'tool_call', toolCallId: 'tool-1', title: text === 'mcp-approval' ? 'mcp__cutex__agent_list' : 'example', rawInput: { a: 1 }, status: 'in_progress' });
    if (text === 'approval' || text === 'mcp-approval') {
      send({ id: 'permission-1', method: 'session/request_permission', params: { sessionId, toolCall: { toolCallId: 'tool-1' }, options: [{ kind: 'allow_once', optionId: 'allow' }, { kind: 'reject_once', optionId: 'reject' }] } });
      continue;
    }
    update({ sessionUpdate: 'tool_call_update', toolCallId: 'tool-1', status: 'completed', content: [{ type: 'content', content: { type: 'text', text: 'tool result' } }] });
    for (const text of ['hello ', 'world']) update({ sessionUpdate: 'agent_message_chunk', messageId: 'message', content: { type: 'text', text } });
    update({ sessionUpdate: 'usage_update', used: 42, size: 2048 });
    send({ id: m.id, result: { stopReason: 'end_turn' } }); prompt = undefined;
  }
}
