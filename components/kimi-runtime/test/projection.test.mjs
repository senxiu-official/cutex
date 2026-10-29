// Pure unit tests (no Kimi needed): ACP updates -> Codex v2 items.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { TurnProjection } from '../src/projection.mjs';
import { toolName, toolResultItem, bashOutcome } from '../src/toolItems.mjs';
import { permissionOutcome, externalItem } from '../src/runtime.mjs';
import { usageOf, addUsage } from '../src/wire.mjs';

test('chunks form one item per run of a kind; a tool call closes the open text', () => {
  const p = new TurnProjection({ threadId: 's', turnId: 't', cwd: '/w' });
  const events = [
    ...p.update({ sessionUpdate: 'agent_thought_chunk', content: { type: 'text', text: 'hmm ' } }),
    ...p.update({ sessionUpdate: 'agent_thought_chunk', content: { type: 'text', text: 'ok' } }),
    ...p.update({ sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'Let me ' } }),
    ...p.update({ sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'look.' } }),
    ...p.update({ sessionUpdate: 'tool_call', toolCallId: '1:c1', title: 'Bash', kind: 'execute', status: 'pending', content: [{ type: 'content', content: { type: 'text', text: '' } }] }),
    ...p.update({ sessionUpdate: 'tool_call_update', toolCallId: '1:c1', status: 'in_progress', content: [{ type: 'content', content: { type: 'text', text: '{"command":"ls"}' } }] }),
    ...p.update({ sessionUpdate: 'tool_call_update', toolCallId: '1:c1', title: 'List files', kind: 'execute', status: 'in_progress', rawInput: { command: 'ls' } }),
    ...p.update({ sessionUpdate: 'tool_call_update', toolCallId: '1:c1', status: 'completed', rawOutput: 'a\nb\n', content: [{ type: 'content', content: { type: 'text', text: 'a\nb\n' } }] }),
    ...p.update({ sessionUpdate: 'plan', entries: [{ content: 'x', status: 'in_progress', priority: 'medium' }] }),
    ...p.finish(),
  ];
  const stored = events.filter(event => event.store);
  assert.deepEqual(stored.map(event => [event.method, event.params.item?.type ?? null]), [
    ['item/completed', 'reasoning'], ['item/completed', 'agentMessage'], ['item/started', 'commandExecution'], ['item/completed', 'commandExecution'], ['turn/plan/updated', null]]);
  assert.equal(stored[1].params.item.text, 'Let me look.');
  assert.equal(stored[3].params.item.aggregatedOutput, 'a\nb\n');
  assert.equal(stored[3].params.item.exitCode, 0);
  assert.equal(p.tools.get('1:c1').name, 'Bash', 'the streamed name survives the description title');
});

test('tool names from kind and arguments when ACP only shows a description', () => {
  assert.equal(toolName({ title: 'Reading notes', kind: 'read', rawInput: { path: 'a' } }), 'Read');
  assert.equal(toolName({ title: 'x', kind: 'read', rawInput: { pattern: 'foo', output_mode: 'content' } }), 'Grep');
  assert.equal(toolName({ title: 'x', kind: 'read', rawInput: { pattern: '**/*.rs' } }), 'Glob');
  assert.equal(toolName({ title: 'x', kind: 'edit', rawInput: { path: 'a', content: 'b' } }), 'Write');
  assert.equal(toolName({ title: 'x', kind: 'fetch', rawInput: { url: 'https://e' } }), 'FetchURL');
  assert.equal(toolName({ title: 'mcp__cutex__agent_tell', kind: 'other' }), 'mcp__cutex__agent_tell');
  const failed = toolResultItem('Bash', { command: 'false' }, 'c', { isError: true, rawOutput: 'Command failed with exit code: 2.' });
  assert.deepEqual([failed.status, failed.exitCode], ['failed', 2]);
  assert.deepEqual(bashOutcome('fine', false), { exitCode: 0, failed: false });
  const write = toolResultItem('Write', { path: 'n.txt', content: 'hi' }, 'w', { content: [{ type: 'diff', path: '/w/n.txt', oldText: null, newText: 'hi' }] });
  assert.deepEqual(write.changes, [{ path: '/w/n.txt', kind: { type: 'add' }, diff: 'hi' }]);
});

test('owner decisions pick Kimi options; usage counts cache as input', () => {
  const options = [{ optionId: 'approve_once', kind: 'allow_once' }, { optionId: 'approve_always', kind: 'allow_always' }, { optionId: 'reject', kind: 'reject_once' }];
  assert.deepEqual(permissionOutcome({ options }, 'accept').outcome.optionId, 'approve_once');
  assert.deepEqual(permissionOutcome({ options }, 'acceptForSession').outcome.optionId, 'approve_always');
  assert.deepEqual(permissionOutcome({ options }, 'decline').outcome.optionId, 'reject');
  assert.deepEqual(permissionOutcome({ options }, 'cancel').outcome, { outcome: 'cancelled' });
  const last = usageOf({ type: 'usage.record', usage: { inputOther: 10, output: 5, inputCacheRead: 30, inputCacheCreation: 2 } });
  assert.deepEqual(last, { totalTokens: 47, inputTokens: 42, cachedInputTokens: 30, cacheWriteInputTokens: 2, outputTokens: 5, reasoningOutputTokens: 0 });
  assert.equal(addUsage(last, last).inputTokens, 84);
  const item = externalItem('t:external:0', 'EXTERNAL_INPUT_JSON\n' + JSON.stringify({ source: { kind: 'agent', id: 'a' }, eventType: 'agent_message', cutexMessageId: 'm', text: 'hi' }));
  assert.deepEqual([item.name, item.cutexMessageId, JSON.parse(item.output).type], ['external_event', 'm', 'message']);
});
