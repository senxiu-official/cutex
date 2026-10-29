import { test } from 'node:test';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { install, binary, privateDaemon, poll, agentTexts, uuid, offlinePatch } from './private-daemon.mjs';

// Private daemon + real DSH + offline model: model/list from DSH's catalog,
// a live model + effort change on the existing chat (next turn uses it), the
// record follows, and the change survives a runtime restart. Opt-in only.
const plugin = fileURLToPath(new URL('./delivery-mock-llm.mjs', import.meta.url));

test('DSH model and effort change on an existing chat, with the provider model list', { skip: !install || !binary, timeout: 600000 }, async t => {
  const daemon = await privateDaemon(t, { label: 'models', patch: offlinePatch(plugin), env: { DSH_MODELS_TEST_KEY: `dummy-${uuid()}` } });
  const { api, history, send, idle, state } = daemon;
  const { profile } = await api('/v2/profiles', { name: 'DSH Models Offline', kind: 'dsh', options: { dsh: { model: 'offline', apiKeyEnv: 'DSH_MODELS_TEST_KEY' } } });
  const agent = await daemon.createAgent('dsh-models', profile.id);
  await daemon.online(agent);
  const native = (method, params) => api(agent.path + '/app-server/requests', { requestId: uuid(), native: { message: { id: uuid(), method, params } } });
  const listed = await native('model/list', { limit: 100 });
  const models = listed.native.message.result.data;
  assert.deepEqual(models.map(model => model.id).sort(), ['offline', 'offline-pro'], JSON.stringify(listed).slice(0, 800));
  assert.equal(models.find(model => model.isDefault).id, 'offline');
  assert.deepEqual(models[0].supportedReasoningEfforts.map(effort => effort.reasoningEffort), ['off', 'low', 'high', 'max']);
  const lastModel = async () => {
    const turns = await poll(() => history(agent), value => value.at(-1)?.status === 'completed' && agentTexts([value.at(-1)]).some(text => text.startsWith('model ')), 'model report');
    return agentTexts([turns.at(-1)]).find(text => text.startsWith('model '));
  };
  await send(agent, 'SHOW-MODEL');
  assert.equal(await lastModel(), 'model offline effort high');
  await idle(agent);
  await native('thread/settings/update', { threadId: agent.facade, model: 'offline-pro', effort: 'low' });
  const updated = await poll(() => state(agent), value => value.runtime?.threadSettings?.model === 'offline-pro', 'settings reach the runtime status');
  assert.equal(updated.runtime.threadSettings.effort, 'low');
  await send(agent, 'SHOW-MODEL again');
  assert.equal(await lastModel(), 'model offline-pro effort low');
  // The change survives a restart of the runtime (the resumed chat keeps it).
  await daemon.stop(agent);
  await daemon.online(agent);
  await idle(agent);
  await send(agent, 'SHOW-MODEL after restart');
  assert.equal(await lastModel(), 'model offline-pro effort low');
  // Token usage of the DSH turns reaches /v2/usage.
  const report = await poll(() => api('/v2/usage'), value => value.totals?.requests === 3, 'usage report counts the three DSH requests', 30000);
  assert.deepEqual([report.totals.input, report.totals.output, report.totals.total], [36, 9, 45]);
  assert.deepEqual(report.unpricedModels.map(model => [model.model, model.requests]).sort(), [['offline', 1], ['offline-pro', 2]], 'attributed to the model that served each turn');
  daemon.markPassed();
});
