import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { DshRuntime, approvalText } from '../src/runtime.mjs';

const fixture = fileURLToPath(new URL('./fake-acp.mjs', import.meta.url));

test('the owner reads the command or the tool call and the model justification', () => {
  assert.deepEqual(approvalText({ tool: 'bash', arguments: { command: 'rm -rf build', sandbox_permissions: 'danger-full-access', justification: 'Clean the build tree.' } }, '/work'),
    { command: 'rm -rf build', cwd: '/work', reason: 'Clean the build tree. (asks for danger-full-access)' });
  assert.deepEqual(approvalText({ tool: 'write', arguments: { file_path: '/etc/x', content: 'y', sandbox_permissions: 'danger-full-access' } }, '/work'),
    { command: 'write {"file_path":"/etc/x","content":"y"}', cwd: '/work', reason: 'DSH asks to run write once (asks for danger-full-access).' });
  assert.equal(approvalText({ tool: 'bash', arguments: { command: ['git', 'push'], cwd: '/repo' } }, '/work').cwd, '/repo');
});

test('cutex MCP tools are pre-approved; other tools ask the owner', async t => {
  const cwd = mkdtempSync(join(tmpdir(), 'cutex-dsh-approval-'));
  t.after(() => rmSync(cwd, { recursive: true, force: true }));
  const asked = [];
  const runtime = new DshRuntime({ command: process.execPath, args: [fixture], cwd, database: join(cwd, 'history.sqlite'), env: {},
    approve: async request => { asked.push(request.params); return { decision: 'accept' }; } });
  try {
    await runtime.initialize();
    await runtime.prompt('mcp-approval');
    assert.equal(asked.length, 0, 'a cutex MCP tool never reaches the owner');
    await runtime.prompt('approval');
    assert.equal(asked.length, 1);
    assert.equal(asked[0].command, 'example {"a":1}');
    assert.deepEqual(asked[0].availableDecisions, ['accept', 'decline', 'cancel']);
  } finally { await runtime.close(); }
});
