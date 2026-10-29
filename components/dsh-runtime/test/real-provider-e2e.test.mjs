import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdirSync, readFileSync, readdirSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import { binary, privateDaemon, poll, userTexts, agentTexts, uuid } from './private-daemon.mjs';

// Real DeepSeek acceptance on a private debug daemon with the component
// built by build.sh and installed with `cutex component install`. Opt-in:
//   DSH_E2E_COMPONENT  extracted build.sh archive (component.json, src, runtime/)
//   DSH_E2E_KEY_FILE   a DeepSeek TEST key file; its content is put only into
//                      the private daemon's env and never printed or stored.
//   DSH_PRODUCT_DAEMON debug cutex binary
// Small on purpose: a few short flash turns.
const component = process.env.DSH_E2E_COMPONENT, keyFile = process.env.DSH_E2E_KEY_FILE;
const model = process.env.DSH_E2E_MODEL ?? 'deepseek-flash';
const KEY_ENV = 'DSH_E2E_TEST_KEY';

function assertNoSecret(root, secret) {
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const path = join(root, entry.name);
    if (entry.isSymbolicLink()) continue;
    if (entry.isDirectory()) { if (entry.name !== "node_modules") assertNoSecret(path, secret); continue; }
    if (!entry.isFile()) continue;
    const bytes = readFileSync(path);
    if (bytes.includes(secret)) assert.fail(`the provider key was persisted in ${path}`);
  }
}

test('real DeepSeek: tool call with approval, delivery from another agent, interrupt, history after stop', { skip: !component || !keyFile || !binary, timeout: 30 * 60 * 1000 }, async t => {
  const key = readFileSync(keyFile, 'utf8').trim();
  assert.ok(key.length > 10, 'test key file is empty');
  const daemon = await privateDaemon(t, { label: 'e2e', component, env: { [KEY_ENV]: key }, secrets: [key] });
  const { api, history, send, idle, state } = daemon;
  const { profile } = await api('/v2/profiles', { name: 'DSH E2E DeepSeek', kind: 'dsh', options: { dsh: { model, apiKeyEnv: KEY_ENV } } });
  const a = await daemon.createAgent('dsh-e2e-a', profile.id, { sandboxMode: 'workspace-write', approvalPolicy: 'on-request' });
  const b = await daemon.createAgent('dsh-e2e-b', profile.id, { sandboxMode: 'workspace-write', approvalPolicy: 'on-request' });
  await daemon.online(a); await daemon.online(b);
  const report = {};

  // The real model list (DSH's DeepSeek catalog) and the live model.
  const listed = await api(a.path + '/app-server/requests', { requestId: uuid(), native: { message: { id: uuid(), method: 'model/list', params: { limit: 50 } } } });
  const models = listed.native?.message?.result?.data ?? listed.native?.result?.data ?? [];
  report.models = models.map(entry => ({ id: entry.id, isDefault: entry.isDefault, inputModalities: entry.inputModalities, efforts: entry.supportedReasoningEfforts?.map(e => e.reasoningEffort ?? e) }));
  assert.ok(models.length > 0, 'DSH lists its models');

  // 1. A tool call that needs approval: a write outside the workspace.
  const outside = join(daemon.home, 'outside');
  mkdirSync(outside);
  const target = join(outside, 'approved.txt');
  await send(a, `Use the write tool to create the file ${target} with the content "e2e ok". It is outside the workspace, so set sandbox_permissions to "danger-full-access" and give a one-line justification. Do nothing else and reply "done".`);
  const pending = await poll(() => state(a), value => value.runtime?.pendingServerRequests?.length === 1, 'real approval pending in Management', 240000);
  const request = pending.runtime.pendingServerRequests[0];
  report.approvalMethod = request.method;
  await daemon.answer(a, request, 'accept');
  await poll(() => history(a), turns => turns.at(-1)?.status === 'completed', 'approved real turn completed', 240000);
  assert.equal(readFileSync(target, 'utf8').trim(), 'e2e ok');

  // 2. A delivery from another agent: A tells B (wake); B answers in its own turn.
  await idle(a); await idle(b);
  await send(a, `Call the cutex tool agent_tell with to="${b.id}", text="ping from A: reply with the word pong", wake=true. Then reply "sent".`);
  const told = await poll(() => history(b), turns => userTexts(turns).some(text => text.includes('EXTERNAL_INPUT_JSON') && text.includes('ping from A'))
    && turns.at(-1)?.status === 'completed', 'agent_tell delivered into real B and answered', 300000);
  report.bReply = agentTexts([told.at(-1)]).join(' ').slice(0, 200);
  assert.equal((await state(a)).runtime.pendingServerRequests.length, 0, 'cutex tools never ask');

  // 3. An interrupt of a long answer.
  await idle(b);
  await send(b, 'Write the numbers from 1 to 2000, one per line, with no other text.');
  const active = await poll(() => state(b), value => value.runtime?.activeTurnId, 'B turn active', 120000);
  const turnId = active.runtime.activeTurnId;
  await new Promise(done => setTimeout(done, 3000));
  await api(b.path + '/app-server/requests', { requestId: uuid(), native: { message: { id: uuid(), method: 'turn/interrupt', params: { threadId: b.facade, turnId } } } });
  const interrupted = await poll(() => history(b), turns => turns.some(turn => turn.id === turnId && turn.status !== 'inProgress'), 'interrupted turn closed', 120000);
  report.interruptedStatus = interrupted.find(turn => turn.id === turnId).status;
  assert.equal(report.interruptedStatus, 'interrupted');
  await idle(b);

  // Usage reached /v2/usage.
  const usage = await poll(() => api('/v2/usage'), value => JSON.stringify(value).includes('dsh') || (value.totals?.requests ?? 0) > 0, 'usage recorded', 60000).catch(error => ({ error: error.message }));
  report.usage = usage.totals ?? usage;

  // 4. History after the adapters stop: the same turns, offline.
  const liveA = await history(a), liveB = await history(b);
  await daemon.stop(a); await daemon.stop(b);
  const offlineA = await api(a.path + '/history'), offlineB = await api(b.path + '/history');
  assert.equal(offlineA.source, 'offline');
  const shape = turns => turns.map(turn => ({ id: turn.id, status: turn.status, items: turn.items.map(item => item.type) }));
  assert.deepEqual(shape(offlineA.native.result.thread.turns), shape(liveA));
  assert.deepEqual(shape(offlineB.native.result.thread.turns), shape(liveB));
  report.turns = { a: shape(liveA).map(turn => turn.status), b: shape(liveB).map(turn => turn.status) };

  assertNoSecret(daemon.home, key);
  assert.ok(existsSync(join(daemon.deploymentDir, 'deployment.json')));
  console.log(`E2E report: ${JSON.stringify(report)}`);
  daemon.markPassed();
});
