// Pinned DSH 0.2.1-rc.3 tool-pwsh contract: description/command/workdir
// input, presentationMeta {cwd}, stdout + [stderr] + trailing exit markers.
// No SDK process, shell, daemon, provider, filesystem or credentials involved.
import test from 'node:test';
import assert from 'node:assert/strict';
import { toolStartItem, toolResultItem, acpResultBlocks } from '../src/toolItems.mjs';
import { projectNativeEvent } from '../src/nativeProjection.mjs';

const text = value => ({ type: 'text', text: value });
const args = { description: 'Print captured Windows command output',
  command: 'Write-Output "integ-cmd"; [Console]::Error.WriteLine("warning"); exit 7',
  workdir: 'C:/work/requested', timeoutMs: 1000 };
const rendered = 'integ-cmd\n[stderr]\nwarning\n[exit code: 7]';
const expectedOutput = 'integ-cmd\n[stderr]\nwarning\n';

test('pwsh command starts and completes with the real SDK text and presentationMeta shape', () => {
  const start = toolStartItem('pwsh', args, 'pwsh-call');
  assert.equal(start.type, 'commandExecution');
  assert.equal(start.command, args.command);
  assert.equal(start.cwd, args.workdir);
  assert.deepEqual(start.commandActions, [{ type: 'unknown', command: args.command }]);
  const done = toolResultItem('pwsh', args, 'pwsh-call', {
    blocks: [text(rendered)], meta: { cwd: 'C:/work/resolved' }, durationMs: 42,
  });
  assert.deepEqual([done.type, done.command, done.cwd, done.aggregatedOutput, done.exitCode, done.status, done.durationMs],
    ['commandExecution', args.command, 'C:/work/resolved', expectedOutput, 7, 'failed', 42]);
  assert.deepEqual([toolResultItem('pwsh', args, 'ok', { blocks: [text('integ-cmd\n')] }).status,
    toolResultItem('pwsh', args, 'ok', { blocks: [text('integ-cmd\n')] }).exitCode], ['completed', 0]);
});

test('pwsh preserves spawn failure, timeout and background semantics without inventing an exit code', () => {
  for (const [isError, output, status, exitCode] of [
    [true, 'Error: spawn pwsh ENOENT', 'failed', null],
    [false, 'partial\n[timed out after 1000ms]', 'failed', null],
    [false, 'partial\n[exit code: 1]', 'failed', 1],
    [false, 'started background job pwsh-example', 'completed', null],
    [false, '(no output)', 'completed', 0],
  ]) {
    const item = toolResultItem('pwsh', args, 'p', { isError, blocks: [text(output)] });
    assert.deepEqual([item.status, item.exitCode], [status, exitCode]);
  }
});

test('native committed pwsh facts project the same command and output into live/history items', () => {
  const sessionId = 'pwsh-native', callId = 'native-pwsh';
  const toolCall = { type: 'tool/call', time: 10, data: { turn: 1, callId, name: 'pwsh', arguments: JSON.stringify(args) } };
  const started = projectNativeEvent({ sessionId, event: toolCall, updates: [] }, { turn: 1 });
  assert.equal(started[0].method, 'item/started');
  assert.equal(started[0].params.item.command, args.command);
  const result = { type: 'tool/result', time: 52, data: { meta: { cwd: 'C:/work/resolved' }, message: { toolCallId: callId } } };
  const updates = [{ sessionUpdate: 'tool_call_update', toolCallId: callId, status: 'completed',
    content: [{ type: 'content', content: text(rendered) }] }];
  const [finished] = projectNativeEvent({ sessionId, event: result, updates }, { turn: 1, toolCall });
  assert.equal(finished.method, 'item/completed');
  assert.equal(finished.params.threadId, sessionId);
  assert.deepEqual(finished.params.item, toolResultItem('pwsh', args, callId, {
    blocks: [text(rendered)], meta: result.data.meta, durationMs: 42,
  }));
  assert.equal(finished.params.item.exitCode, 7);
  assert.equal(finished.params.item.aggregatedOutput, expectedOutput);
});

test('ACP generic pwsh updates use the same command projection and retain explicit workdir without native meta', () => {
  // These are the update.title/rawInput/content fields consumed by the ACP
  // fallback in DshRuntime; it deliberately has no native presentationMeta.
  const call = { title: 'pwsh', toolCallId: 'acp-pwsh', rawInput: args };
  const update = { toolCallId: call.toolCallId, status: 'completed', content: [{ type: 'content', content: text(rendered) }] };
  const item = toolResultItem(call.title, call.rawInput, call.toolCallId, {
    isError: update.status === 'failed', blocks: acpResultBlocks(update.content),
  });
  assert.deepEqual([item.type, item.command, item.cwd, item.aggregatedOutput, item.exitCode, item.status],
    ['commandExecution', args.command, args.workdir, expectedOutput, 7, 'failed']);
});

test('bash keeps its existing command/action/output/cwd/exit semantics', () => {
  const bashArgs = { command: 'echo hi; ls; exit 3', description: 'Say hi' };
  const item = toolResultItem('bash', bashArgs, 'bash-call', {
    blocks: [text('hi\nnotes.txt\n[exit code: 3]')], meta: { cwd: '/work' }, durationMs: 50,
  });
  assert.deepEqual([item.type, item.command, item.cwd, item.aggregatedOutput, item.exitCode, item.status, item.durationMs],
    ['commandExecution', bashArgs.command, '/work', 'hi\nnotes.txt\n', 3, 'failed', 50]);
  assert.deepEqual(item.commandActions, [{ type: 'unknown', command: bashArgs.command }]);
  assert.equal(toolStartItem('bash', bashArgs, 'b').cwd, null);
});
