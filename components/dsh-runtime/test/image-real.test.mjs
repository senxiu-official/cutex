import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { homedir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { DshRuntime } from '../src/runtime.mjs';

// Real DSH with an image-capable (offline) model: an inline image reaches the
// model through ACP. Opt-in: DSH_PRODUCT_INSTALL.
const install = process.env.DSH_PRODUCT_INSTALL ?? process.env.DSH_SPIKE_INSTALL;
// 1x1 transparent PNG.
const PNG = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==';
test('real DSH admits an inline image when its model declares image input', { skip: !install, timeout: 180000 }, async t => {
  const base = join(homedir(), '.cutex-dsh-spike');
  mkdirSync(base, { recursive: true });
  const home = mkdtempSync(join(base, 'image-'));
  const cwd = join(home, 'workspace');
  mkdirSync(cwd);
  writeFileSync(join(home, '.cutex-test-private-home'), 'dsh image\n');
  const env = Object.fromEntries(['PATH'].filter(key => process.env[key]).map(key => [key, process.env[key]]));
  Object.assign(env, { HOME: home, USERPROFILE: home, TEMP: home, TMP: home, TMPDIR: home, CUTEX_TEST_PRIVATE_HOME: home, DSH_HOME: join(home, 'dsh'), DSH_TELEMETRY_DISABLED: '1', DSH_SPIKE_INSTALL_ROOT: resolve(install) });
  const patch = join(home, 'image.patch.yml');
  writeFileSync(patch, `
- id: llm-deepseek
  disabled: true
- id: acp
  config: { provider: cutex-offline, model: offline-vision }
- id: session-persistence-jsonl
  config: { root: '${join(home, 'sessions')}', compression: none }
- id: session-log-deepseek
  config: { enabled: false }
- insert:
    - id: cutex-image-offline-llm
      name: '${fileURLToPath(new URL('./image-mock-llm.mjs', import.meta.url))}'
`);
  const runtime = new DshRuntime({ command: process.execPath, args: [join(install, 'node_modules/@deepseek-ai/dsh/lib/bin.js'), '--profile', 'acp', '--patch', patch],
    cwd, env, database: join(home, 'timeline.sqlite'), sessionsRoot: join(home, 'sessions'), installRoot: resolve(install) });
  t.after(async () => { await runtime.close(); rmSync(home, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 }); });
  await runtime.initialize();
  assert.equal(runtime.imagePrompts, true);
  assert.deepEqual(runtime.modelList().find(model => model.isDefault).inputModalities, ['text', 'image']);
  // The owner's upload, as server.mjs records it for turn/start's localImage (C-4).
  const upload = join(home, 'upload.png');
  writeFileSync(upload, Buffer.from(PNG, 'base64'));
  await runtime.prompt('what is in this picture?', [{ mimeType: 'image/png', data: PNG }], [{ type: 'localImage', path: upload }]);
  const reply = runtime.replay().filter(event => event.method === 'item/completed' && event.params.item.type === 'agentMessage').at(-1).params.item.text;
  assert.match(reply, /^images 1 /, reply);
  // DSH's committed user message shows the upload as cute-codex echoes it.
  const user = runtime.replay().filter(event => event.method === 'item/completed' && event.params.item.type === 'userMessage').at(-1).params.item;
  assert.deepEqual(user.content.map(part => part.type), ['text', 'localImage']);
  assert.equal(user.content[1].path, upload);
  assert.equal(user.content[1].dshAttachment.mediaType, 'image/png');
  // A data URL (no file) stays a placeholder.
  await runtime.prompt('and this one?', [{ mimeType: 'image/png', data: PNG }], [{ type: 'image', sha256: '0'.repeat(64) }]);
  const second = runtime.replay().filter(event => event.method === 'item/completed' && event.params.item.type === 'userMessage').at(-1).params.item;
  assert.deepEqual(second.content.map(part => part.type), ['text', 'text']);
  assert.match(second.content[1].text, /^\[Image/);
});
