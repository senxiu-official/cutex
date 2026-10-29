import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { homedir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { DshRuntime } from '../src/runtime.mjs';

// Real DSH (product mode: native log authority) + the cutex plugin + a
// streaming offline model. Live clients get text and reasoning as deltas on
// live items, then the committed message under the same id (never twice);
// history keeps only the native items. Opt-in: DSH_PRODUCT_INSTALL.
const install = process.env.DSH_PRODUCT_INSTALL ?? process.env.DSH_SPIKE_INSTALL;
test('real DSH streams live deltas without duplicating the committed message', { skip: !install, timeout: 180000 }, async t => {
  const base = join(homedir(), '.cutex-dsh-spike');
  mkdirSync(base, { recursive: true });
  const home = mkdtempSync(join(base, 'stream-'));
  const cwd = join(home, 'workspace');
  mkdirSync(cwd);
  writeFileSync(join(home, '.cutex-test-private-home'), 'dsh streaming\n');
  const env = Object.fromEntries(['PATH'].filter(key => process.env[key]).map(key => [key, process.env[key]]));
  Object.assign(env, { HOME: home, USERPROFILE: home, TEMP: home, TMP: home, TMPDIR: home, CUTEX_TEST_PRIVATE_HOME: home, DSH_HOME: join(home, 'dsh'), DSH_TELEMETRY_DISABLED: '1', DSH_SPIKE_INSTALL_ROOT: resolve(install) });
  const patch = join(home, 'stream.patch.yml');
  const mock = fileURLToPath(new URL('./stream-mock-llm.mjs', import.meta.url));
  const plugin = fileURLToPath(new URL('../src/dsh-plugin.mjs', import.meta.url));
  writeFileSync(patch, `
- id: llm-deepseek
  disabled: true
- id: acp
  config: { provider: cutex-offline, model: offline }
- id: session-persistence-jsonl
  config: { root: '${join(home, 'sessions')}', compression: none }
- id: session-log-deepseek
  config: { enabled: false }
- insert:
    - id: cutex-stream-offline-llm
      name: '${mock}'
    - id: cutex-bridge
      name: '${plugin}'
`);
  const live = [];
  const runtime = new DshRuntime({ command: process.execPath, args: [join(install, 'node_modules/@deepseek-ai/dsh/lib/bin.js'), '--profile', 'acp', '--patch', patch],
    cwd, env, database: join(home, 'timeline.sqlite'), sessionsRoot: join(home, 'sessions'), installRoot: resolve(install), emit: event => live.push(event) });
  t.after(async () => { await runtime.close(); rmSync(home, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 }); });
  await runtime.initialize();
  assert.equal((await runtime.prompt('stream please')).stopReason, 'end_turn');
  const deltas = live.filter(event => event.method === 'item/agentMessage/delta');
  assert.deepEqual(deltas.map(event => event.params.delta), ['Hel', 'lo ', 'live']);
  assert.deepEqual(live.filter(event => event.method === 'item/reasoning/textDelta').map(event => event.params.delta), ['think', 'ing']);
  const textId = deltas[0].params.itemId;
  const messages = live.filter(event => event.method === 'item/completed' && event.params.item.type === 'agentMessage');
  assert.equal(messages.length, 1, 'the committed message reaches live clients once');
  assert.equal(messages[0].params.item.id, textId, 'under its live id');
  assert.equal(messages[0].params.item.text, 'Hello live');
  const reasoning = live.filter(event => event.method === 'item/completed' && event.params.item.type === 'reasoning');
  assert.equal(reasoning.length, 1);
  assert.equal(reasoning[0].params.item.id, live.find(event => event.method === 'item/reasoning/textDelta').params.itemId);
  assert.equal(deltas[0].params.turnId, live.find(event => event.method === 'turn/started').params.turn.id);
  assert.ok(live.findIndex(event => event.method === 'item/completed' && event.params.item.id === textId) < live.findIndex(event => event.method === 'turn/completed'));
  // Usage: thread-cumulative total and this request's last, cute-codex shape.
  const usage = live.filter(event => event.method === 'thread/tokenUsage/updated');
  assert.equal(usage.length, 1);
  assert.deepEqual(usage[0].params.tokenUsage.last, { totalTokens: 26, inputTokens: 21, cachedInputTokens: 0, cacheWriteInputTokens: 0, outputTokens: 5, reasoningOutputTokens: 0 });
  assert.deepEqual(usage[0].params.tokenUsage.total, usage[0].params.tokenUsage.last);
  assert.equal(usage[0].params.turnId, deltas[0].params.turnId);
  await runtime.prompt('stream again');
  assert.equal(live.filter(event => event.method === 'thread/tokenUsage/updated').at(-1).params.tokenUsage.total.totalTokens, 52);
  // History: the native item only, once.
  const history = runtime.replay().filter(event => event.method === 'item/completed' && event.params.item.type === 'agentMessage');
  assert.equal(history.length, 2);
  assert.match(history[0].params.item.id, /^agent_message_chunk:/);
});
